use std::cell::{Cell, RefCell};

use glib::clone;
use glib::subclass::InitializingObject;
use gtk::gdk::{EventSequence, Key, ModifierType};
use gtk::glib::closure_local;
use gtk::glib::subclass::prelude::*;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::subclass::prelude::*;
use gtk::{
    glib, Button, CompositeTemplate, Label, ListView, ScrolledWindow, SearchBar, SearchEntry,
    SingleSelection, ToggleButton,
};
use gtk::{prelude::*, GestureClick};
use futures::StreamExt;

use crate::page;
use crate::state::State;

// Time constant of the exponential glide toward the target page position. Larger = slower and
// smoother; the perceived slide runs a few times this long. The glide is a low-pass follow, which
// damps the hadjustment jitter that GtkListView injects when async crop relayout makes it re-anchor
// mid-slide, so the page settles instead of vibrating.
const SCROLL_ANIM_TAU_US: f64 = 130_000.0;

// Per-frame dt is clamped to this. When the frame clock stalls and resumes (or crop relayout
// delays a frame) a huge dt drives the glide gain toward 1, which turns the live-target follow into
// a sustained oscillation instead of a decaying one; clamping keeps it contractive.
const SCROLL_ANIM_MAX_DT_US: i64 = 32_000;
// Hard ceiling on a single glide. The live target is read from page geometry that crop relayout can
// keep nudging, so bound the glide so it can't chase a moving target forever; snap and stop.
const SCROLL_ANIM_MAX_US: i64 = 2_000_000;
// Minimum approach speed (px/frame) near the target. The exponential glide asymptotes and never
// quite arrives; a floor closes the last few pixels at constant velocity so the glide can finish on
// the target instead of settling a few pixels short and snapping (a visible jerk).
const SCROLL_ANIM_MIN_STEP: f64 = 1.5;

// Page-stepping thresholds for `accumulate_step`. Wheel travel is in unitless notch clicks (1.0 per
// physical notch).
const WHEEL_NOTCH: f64 = 1.0;
const WHEEL_TRIGGER: f64 = 0.2;
const TOUCHPAD_NOTCH: f64 = 40.0;
const TOUCHPAD_TRIGGER: f64 = 8.0;

// Multiplicative zoom step per notch.
const ZOOM_STEP: f64 = 1.1;

// Quiet period after the last scroll motion before the view is treated as settled and its pages are
// full-rendered. Long enough that a continuous scroll doesn't repeatedly arm it, short enough that
// stopping feels immediate.
const SETTLE_MS: u64 = 150;

// Quiet period after the last keystroke before a search sweep launches, coalescing a burst of typing.
const SEARCH_DEBOUNCE_MS: u64 = 100;

// In-flight state of the animated one-page slide.
//
// The end position is recomputed live each tick from the selected page widget's actual geometry, so
// async crop/zoom layout shifts during the slide are compensated and the page still lands at the
// same on-screen spot. `anchor_x` is the viewport x where the selected page's left edge should come
// to rest. `last_target` remembers the most recent geometry-derived resting position; it is used
// when the selected page isn't realised yet (e.g. selection has raced ahead during a burst) so the
// slide chases only real page positions and never overshoots into a reverse correction.
// `last_frame` is the previous tick's frame time (-1 until the first tick) for a
// frame-rate-independent glide.
#[derive(Clone, Copy)]
struct ScrollAnim {
    anchor_x: Option<f64>,
    last_target: f64,
    last_frame: i64,
    // frame time the glide began (-1 until the first tick); bounds total glide duration
    start_frame: i64,
    // travel direction (+1 forward, -1 back); the glide never moves against it
    dir: f64,
}

// Object holding the state
#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/andr2i/scrolex/app.ui")]
pub struct Window {
    #[template_child]
    pub state: TemplateChild<State>,
    #[template_child]
    pub model: TemplateChild<gtk::gio::ListStore>,
    #[template_child]
    pub selection: TemplateChild<SingleSelection>,

    #[template_child]
    pub btn_open: TemplateChild<Button>,
    #[template_child]
    pub btn_zoom_in: TemplateChild<Button>,
    #[template_child]
    pub btn_zoom_out: TemplateChild<Button>,
    #[template_child]
    pub btn_crop: TemplateChild<ToggleButton>,
    #[template_child]
    pub btn_animate_scroll: TemplateChild<ToggleButton>,
    #[template_child]
    pub spin_threads: TemplateChild<gtk::SpinButton>,
    #[template_child]
    pub btn_jump_back: TemplateChild<Button>,
    #[template_child]
    pub scrolledwindow: TemplateChild<ScrolledWindow>,
    #[template_child]
    pub listview: TemplateChild<ListView>,
    #[template_child]
    pub entry_page_num: TemplateChild<gtk::Entry>,
    #[template_child]
    pub search_bar: TemplateChild<SearchBar>,
    #[template_child]
    pub search_entry: TemplateChild<SearchEntry>,
    #[template_child]
    pub search_status: TemplateChild<Label>,
    #[template_child]
    pub btn_search_case: TemplateChild<ToggleButton>,

    // set while a re-search is queued, to coalesce keystrokes into one sweep
    search_debounce: RefCell<Option<glib::SourceId>>,

    drag_coords: RefCell<Option<(f64, f64)>>,
    drag_cursor: RefCell<Option<gtk::gdk::Cursor>>,

    // set while a selection sync is queued on idle, to coalesce a burst of
    // scroll events (e.g. aggressive wheeling) into a single sync that runs
    // after the list view has finished re-laying-out
    sync_pending: Cell<bool>,

    // in-flight animated one-page scroll; None when no slide is running
    scroll_anim: RefCell<Option<ScrollAnim>>,

    // fires once scrolling has been quiet for SETTLE_MS, to flip State::scrolling back off and
    // full-render the pages that came to rest on screen; reset on every scroll motion
    settle_source: RefCell<Option<glib::SourceId>>,

    // accumulates hi-res mouse-wheel deltas: libinput splits one physical notch into several
    // sub-events that sum to 1.0, so we advance a page only when the running total crosses a whole
    // notch, keeping the remainder
    wheel_accum: Cell<f64>,
    // the previous wheel delta, to detect a direction reversal and reset the accumulator so
    // reversing doesn't over-step
    wheel_last_dy: Cell<f64>,

    // same accumulation as the wheel, but for vertical touchpad scroll (kept separate because its
    // travel is measured in pixels, not notch clicks)
    touch_accum: Cell<f64>,
    touch_last_dy: Cell<f64>,

    // zoom captured when a pinch gesture begins; scale-changed reports scale relative to that start,
    // so the live zoom is base * scale
    zoom_gesture_base: Cell<f64>,

    // true while a touchpad pinch is in progress; gates off the two-finger scroll events the touchpad
    // still emits during a pinch
    zoom_gesturing: Cell<bool>,
}

// The central trait for subclassing a GObject
#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "MyApp";
    type Type = super::Window;
    type ParentType = gtk::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
        klass.bind_template_instance_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for Window {
    fn constructed(&self) {
        self.parent_constructed();

        if let Some(editable) = self.entry_page_num.delegate() {
            editable.connect_insert_text(|entry, s, _| {
                for c in s.chars() {
                    if !c.is_numeric() {
                        entry.stop_signal_emission_by_name("insert-text");
                    }
                }
            });
        }

        self.setup_scroll_selection_sync();
        self.setup_thread_setting();
        self.setup_search();

        // Give keyboard focus to the scroll area rather than the header entry
        self.scrolledwindow.set_focusable(true);
        self.listview.set_focusable(false);
        let scrolledwindow = self.scrolledwindow.clone();
        self.obj().connect_map(move |_| {
            scrolledwindow.grab_focus();
        });
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn on_factory_setup(&self, list_item: &gtk::ListItem) {
        let page = &page::Page::new(&self.state);

        let obj = self.obj().clone();
        page.connect_closure(
            "named-link-clicked",
            false,
            closure_local!(move |_: &crate::page::Page, page_num: i32| {
                obj.imp().goto_page(page_num as u32);
            }),
        );

        list_item.set_child(Some(page));
    }

    #[template_callback]
    fn on_factory_bind(_: &gtk::SignalListItemFactory, list_item: &gtk::ListItem) {
        let page_number = list_item.item().and_downcast::<page::PageNumber>().unwrap();
        let page = list_item
            .child()
            .and_downcast::<crate::page::Page>()
            .unwrap();

        page.bind(&page_number);
    }

    #[template_callback]
    fn handle_scroll(
        &self,
        dx: f64,
        dy: f64,
        scroll: &gtk::EventControllerScroll,
    ) -> glib::Propagation {
        let unit = scroll.unit();
        log::debug!("scroll event: dx={dx}, dy={dy}, unit={unit:?}");

        // swallow the two-finger scroll a pinch emits alongside the zoom gesture
        if self.zoom_gesturing.get() {
            return glib::Propagation::Stop;
        }

        // Ctrl+scroll zooms instead of navigating; dy<0 zooms in, dy>0 out. Touchpad pixels are
        // scaled to the wheel's notch so both zoom at a comparable rate.
        if scroll.current_event_state().contains(ModifierType::CONTROL_MASK) {
            if dy != 0.0 {
                let notches = match unit {
                    gtk::gdk::ScrollUnit::Wheel => dy,
                    _ => dy / TOUCHPAD_NOTCH,
                };
                self.note_scroll_activity();
                self.state.set_zoom(self.state.zoom() * ZOOM_STEP.powf(-notches));
            }
            return glib::Propagation::Stop;
        }

        self.note_scroll_activity();

        match unit {
            // Mouse wheel: hi-res wheels split one notch into several sub-events summing to 1.0.
            // Fire the first page as soon as a small fraction of a notch has scrolled (TRIGGER) so
            // the wheel responds immediately, then pace one page per full notch (subtract NOTCH,
            // keep the remainder). Steps land at cumulative 0.2, 1.2, 2.2, … of a notch.
            gtk::gdk::ScrollUnit::Wheel => {
                let (accum, step) = accumulate_step(
                    self.wheel_accum.get(),
                    self.wheel_last_dy.get(),
                    dy,
                    WHEEL_NOTCH,
                    WHEEL_TRIGGER,
                );
                self.wheel_accum.set(accum);
                self.wheel_last_dy.set(dy);
                self.step_page(step);
            }
            // Touchpad (and any other pixel-precise device): a horizontal swipe scrolls the page
            // flow smoothly (like dragging); a vertical swipe steps pages, one per notch of travel.
            _ => {
                // Apply the touchpad's horizontal delta to the scroll position, but only when no
                // page slide is running. During a slide `scroll_tick` owns the adjustment; adding dx
                // here (from a scroll event that arrives mid-slide) would fight its writes frame by
                // frame. Vertical page-stepping below runs either way, so flicking through pages
                // during a slide still works.
                if self.scroll_anim.borrow().is_none() {
                    let hadj = self.scrolledwindow.hadjustment();
                    hadj.set_value(self.clamp_scroll(hadj.value() + dx));
                }

                let (accum, step) = accumulate_step(
                    self.touch_accum.get(),
                    self.touch_last_dy.get(),
                    dy,
                    TOUCHPAD_NOTCH,
                    TOUCHPAD_TRIGGER,
                );
                self.touch_accum.set(accum);
                self.touch_last_dy.set(dy);
                self.step_page(step);
            }
        }

        // Scrolling changes the selection, and when GtkListView moves the selected row it can hand
        // keyboard focus to a header entry (the list view and its rows aren't focusable). Reassert
        // focus on the scroll area so h/l/arrows keep working after a scroll.
        if !self.scrolledwindow.has_focus() {
            self.scrolledwindow.grab_focus();
        }

        glib::Propagation::Stop
    }

    // Apply a page step from a scroll accumulator: +1 forward, -1 back, 0 nothing.
    fn step_page(&self, step: i32) {
        if step > 0 {
            self.next_page();
        } else if step < 0 {
            self.prev_page();
        }
    }

    #[template_callback]
    fn handle_drag_start(&self, _n_press: i32, x: f64, y: f64) {
        // A drag takes over the horizontal position; drop any in-flight slide so scroll_tick stops
        // writing hadj and fighting the drag.
        *self.scroll_anim.borrow_mut() = None;
        *self.drag_coords.borrow_mut() = Some((x, y));

        if let Some(surface) = self.obj().surface() {
            *self.drag_cursor.borrow_mut() = surface.cursor();
            surface.set_cursor(gtk::gdk::Cursor::from_name("grabbing", None).as_ref());
        }
    }

    #[template_callback]
    fn handle_drag_move(&self, seq: Option<&EventSequence>, gc: &GestureClick) {
        if let Some((prev_x, _)) = *self.drag_coords.borrow() {
            if let Some((x, _)) = gc.point(seq) {
                let dx = x - prev_x;
                let hadjustment = self.scrolledwindow.hadjustment();
                self.note_scroll_activity();
                hadjustment.set_value(hadjustment.value() - dx);
            }
        }
        *self.drag_coords.borrow_mut() = gc.point(seq);
    }

    #[template_callback]
    fn handle_drag_end(&self) {
        if let Some(surface) = self.obj().surface() {
            surface.set_cursor(self.drag_cursor.borrow().as_ref());
        }
    }

    #[template_callback]
    fn zoom_out(&self) {
        self.state.set_zoom(self.state.zoom() / ZOOM_STEP);
    }

    #[template_callback]
    fn zoom_in(&self) {
        self.state.set_zoom(self.state.zoom() * ZOOM_STEP);
    }

    #[template_callback]
    fn handle_zoom_begin(&self) {
        self.zoom_gesturing.set(true);
        self.zoom_gesture_base.set(self.state.zoom());
    }

    #[template_callback]
    fn handle_zoom_end(&self) {
        self.zoom_gesturing.set(false);
    }

    #[template_callback]
    fn handle_zoom_scale_changed(&self, scale: f64) {
        if scale <= 0.0 {
            return;
        }
        // A pinch rescales the cheap previews live; defer the slow full re-renders until the gesture
        // settles, the same way scrolling does.
        self.note_scroll_activity();
        self.state.set_zoom(self.zoom_gesture_base.get() * scale);
    }

    #[template_callback]
    fn handle_key_press(
        &self,
        keyval: Key,
        _keycode: u32,
        _modifier: ModifierType,
    ) -> glib::Propagation {
        match keyval {
            Key::o => {
                self.open_document();
            }
            Key::f => {
                self.open_search();
            }
            Key::l => {
                self.next_page();
            }
            Key::h => {
                self.prev_page();
            }
            Key::bracketleft => {
                self.zoom_out();
            }
            Key::bracketright => {
                self.zoom_in();
            }
            Key::n | Key::N => {
                if self.state.search().borrow().total() == 0 {
                    return glib::Propagation::Proceed;
                }
                if keyval == Key::N {
                    self.prev_match();
                } else {
                    self.next_match();
                }
            }
            Key::Left | Key::Right => {
                // fine horizontal scroll; handled here rather than relying on the scrolled window's
                // own key bindings, which only fire when it directly holds focus
                //
                // Like a drag, this fine nudge takes over the horizontal position, so drop any
                // in-flight page slide; otherwise scroll_tick would overwrite the nudge each frame.
                // (h/l intentionally keep the slide running - they step pages and retarget it.)
                *self.scroll_anim.borrow_mut() = None;
                self.note_scroll_activity();
                let hadj = self.scrolledwindow.hadjustment();
                let step = if hadj.step_increment() > 0.0 {
                    hadj.step_increment()
                } else {
                    hadj.page_size() * 0.1
                };
                let delta = if keyval == Key::Left { -step } else { step };
                hadj.set_value(hadj.value() + delta);
            }
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    }

    #[template_callback]
    fn handle_page_number_entered(&self, entry: &gtk::Entry) {
        let Ok(page_num) = entry.text().parse::<u32>() else {
            return;
        };

        self.goto_page(page_num);
    }

    #[template_callback]
    fn handle_page_number_icon_pressed(&self, _: gtk::EntryIconPosition, entry: &gtk::Entry) {
        let Ok(page_num) = entry.text().parse::<u32>() else {
            return;
        };

        self.goto_page(page_num);
    }

    #[template_callback]
    fn handle_zoom_entry_icon(&self, _: gtk::EntryIconPosition, entry: &gtk::Entry) {
        self.handle_zoom_entry(entry);
    }

    #[template_callback]
    fn handle_zoom_entry(&self, entry: &gtk::Entry) {
        let Ok(zoom) = entry.text().parse::<f64>() else {
            return;
        };

        if zoom < 5.0 {
            return;
        }

        self.state.set_zoom(zoom / 100.0);
    }

    fn goto_page(&self, page_num: u32) {
        self.state.jump_list_add(self.state.page() + 1);
        self.navigate_to_page(page_num);
    }

    // same as goto_page, but doesn't add to jump list
    fn navigate_to_page(&self, page_num: u32) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        let page_num = page_num.min(selection.n_items());

        self.listview.scroll_to(
            page_num.saturating_sub(1),
            gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
            None,
        );
    }

    fn prev_page(&self) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        self.state.set_scroll_forward(false);

        // where the page we're leaving sits now; the newly selected page slides
        // to this same spot
        let anchor = self.selected_page_left_x();

        // normally I'd use list_view.scroll_to() here, but it doesn't scroll if the item
        // is already visible :(
        selection.select_item(selection.selected().saturating_sub(1), true);
        let width = f64::from(
            selection
                .selected_item()
                .and_downcast::<page::PageNumber>()
                .unwrap()
                .width(),
        ) + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this

        self.animate_scroll(anchor, -width);
    }

    fn next_page(&self) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        self.state.set_scroll_forward(true);

        // where the page we're leaving sits now; the newly selected page slides to this same spot
        let anchor = self.selected_page_left_x();

        // normally I'd use list_view.scroll_to() here, but it doesn't scroll if the item
        // is already visible :(
        let width = f64::from(
            selection
                .selected_item()
                .and_downcast::<page::PageNumber>()
                .unwrap()
                .width(),
        ) + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this

        selection.select_item(
            (selection.selected() + 1).min(selection.n_items() - 1),
            true,
        );
        self.animate_scroll(anchor, width);
    }

    // Slide the horizontal scroll by one page instead of jumping, so the reader sees the page move
    // and keeps their place. The selected page comes to rest at `anchor_x` (the viewport x it
    // occupied before the step), matching the old instant behaviour but with motion. Wheeling again
    // while a slide runs keeps the same anchor and retargets the running animation, so a burst
    // stays smooth. `delta` seeds a resting position only for the degenerate case where the
    // selected page's live geometry can't be read at all.
    fn animate_scroll(&self, anchor_x: Option<f64>, delta: f64) {
        // Keyboard/step navigation reaches the view here (not through the scroll controller), so
        // arm the settle timer here too - full renders wait until the stepping stops.
        self.note_scroll_activity();

        // Cancel any kinetic deceleration the GTK is doing to the scrolled window. Why calling it
        // two times? The Api is a bit strange: its cancel only runs on a real property change.
        self.scrolledwindow.set_kinetic_scrolling(true);
        self.scrolledwindow.set_kinetic_scrolling(false);

        let hadj = self.scrolledwindow.hadjustment();

        // animation toggled off: jump straight to the page
        if !self.state.animate_scroll() {
            hadj.set_value(self.clamp_scroll(hadj.value() + delta));
            return;
        }

        let mut anim = self.scroll_anim.borrow_mut();
        // A retarget resets the duration ceiling (start_frame -1); else a long burst force-settles
        // short of target, landing the page off its anchor. last_frame carries over to keep pacing.
        let (anchor_x, prev_target, last_frame, start_frame) = match anim.as_ref() {
            Some(a) => (a.anchor_x, Some(a.last_target), a.last_frame, -1),
            None => (anchor_x, None, -1, -1),
        };
        // Prefer the selected page's exact live position. When it isn't laid out yet (selection
        // raced ahead in a burst), advance by one page-width from the previous target so the burst
        // keeps covering ground; live geometry snaps it to the exact spot once the page is actually
        // mapped.
        let live = self.live_target(anchor_x);
        let last_target = live
            .unwrap_or_else(|| self.clamp_scroll(prev_target.unwrap_or(hadj.value()) + delta));
        log::debug!(
            "slide arm: anchor_x={anchor_x:?} target={last_target:.2} live={} fresh={} hadj={:.2}",
            live.is_some(),
            anim.is_none(),
            hadj.value(),
        );
        let start_fresh = anim.is_none();
        *anim = Some(ScrollAnim {
            anchor_x,
            last_target,
            last_frame,
            start_frame,
            dir: delta.signum(),
        });
        drop(anim);

        if start_fresh {
            self.scrolledwindow.add_tick_callback(clone!(
                #[weak(rename_to = imp)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move |_, clock| imp.scroll_tick(clock)
            ));
        }
    }

    fn scroll_tick(&self, clock: &gtk::gdk::FrameClock) -> glib::ControlFlow {
        let Some(mut anim) = *self.scroll_anim.borrow() else {
            return glib::ControlFlow::Break;
        };

        // Chase the selected page's live resting position; when it isn't realised yet (selection
        // raced ahead in a burst) hold the last known one. Real page positions only advance as
        // selection advances, so the target never jumps behind us and the slide never reverses.
        let target = self.live_target(anim.anchor_x).unwrap_or(anim.last_target);
        anim.last_target = target;

        let now = clock.frame_time();
        if anim.start_frame < 0 {
            anim.start_frame = now;
        }
        let raw_dt = if anim.last_frame < 0 {
            0
        } else {
            now - anim.last_frame
        };
        anim.last_frame = now;

        let hadj = self.scrolledwindow.hadjustment();
        let value = hadj.value();
        let (next, settled) = glide_step(value, target, raw_dt, now - anim.start_frame, anim.dir);
        if settled {
            // land at the eased position (within sub-pixel of target) and let the normal sync
            // reconcile selection; never jump to a distant target - that snap is the visible jerk
            *self.scroll_anim.borrow_mut() = None;
            hadj.set_value(next);
            log::debug!(
                "slide settle: target={target:.2} value={next:.2} short={:.2} left_x={:?}",
                target - next,
                self.selected_page_left_x(),
            );
            return glib::ControlFlow::Break;
        }

        *self.scroll_anim.borrow_mut() = Some(anim);
        hadj.set_value(next);
        glib::ControlFlow::Continue
    }

    fn clamp_scroll(&self, value: f64) -> f64 {
        let hadj = self.scrolledwindow.hadjustment();
        let lower = hadj.lower();
        value.clamp(lower, (hadj.upper() - hadj.page_size()).max(lower))
    }

    // Resting hadjustment that puts the selected page's left edge at `anchor_x`, from its live
    // geometry. None if there's no anchor or the page widget isn't realised. page_content_left =
    // on-screen x + current value; the resting value is page_content_left - anchor_x.
    fn live_target(&self, anchor_x: Option<f64>) -> Option<f64> {
        let anchor = anchor_x?;
        let left_x = self.selected_page_left_x()?;
        let value = self.scrolledwindow.hadjustment().value();
        Some(self.clamp_scroll(left_x + value - anchor))
    }

    // On-screen x (in scrolled-window coordinates) of the currently selected page's left edge, if
    // that page widget is currently laid out in the viewport. Recycled/spare list widgets can
    // already carry the selected index while sitting unmapped at the origin; trusting their (0, 0)
    // position would drive the slide backwards, so those are skipped.
    fn selected_page_left_x(&self) -> Option<f64> {
        let selected = self.selection.selected() as i32;
        let mut child = self.listview.first_child();
        while let Some(c) = child {
            if let Some(page) = descendant_page(&c) {
                if page.index() == selected && page.is_mapped() && page.width() > 0 {
                    if let Some(point) = page
                        .compute_point(&*self.scrolledwindow, &gtk::graphene::Point::new(0.0, 0.0))
                    {
                        return Some(f64::from(point.x()));
                    }
                }
            }
            child = c.next_sibling();
        }
        None
    }

    fn ensure_ready_selection(&self) -> Option<&gtk::SingleSelection> {
        let selection: &gtk::SingleSelection = self.selection.as_ref();

        if selection.n_items() == 0 {
            return None;
        }

        selection.selected_item()?;

        Some(selection)
    }

    #[template_callback]
    fn clear_model(&self) {
        self.model.remove_all();
    }

    #[template_callback]
    fn open_document(&self) {
        let filter = gtk::FileFilter::new();
        filter.add_mime_type("application/pdf");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);

        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .filters(&filters)
            .build();

        let obj = self.obj();
        dialog.open(
            Some(obj.as_ref()),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[strong(rename_to = state)]
                self.state,
                #[strong]
                obj,
                move |file| match file {
                    Ok(file) => {
                        state.load(&file).unwrap_or_else(|err| {
                            obj.show_error_dialog(&format!("Error loading file: {err}"));
                        });
                    }
                    Err(err) => {
                        obj.show_error_dialog(&format!("Error opening file: {err}"));
                    }
                },
            ),
        );
    }

    #[template_callback]
    fn handle_document_load(&self, state: &State) {
        let Some(doc) = state.doc() else {
            return;
        };

        let model = self.model.clone();
        let selection = self.selection.clone();

        let n_pages = doc.n_pages() as u32;
        let scroll_to = state.page().min(n_pages - 1);
        let init_load_from = scroll_to.saturating_sub(1);
        let init_load_till = (scroll_to + 10).min(n_pages - 1);

        let vector: Vec<page::PageNumber> = (init_load_from as i32..init_load_till as i32)
            .map(page::PageNumber::new)
            .collect();
        model.extend_from_slice(&vector);
        selection.select_item(scroll_to - init_load_from, true);

        glib::idle_add_local(move || {
            if init_load_from > 0 {
                let vector: Vec<page::PageNumber> = (0..init_load_from as i32)
                    .map(page::PageNumber::new)
                    .collect();
                model.splice(0, 0, &vector);
            }
            if init_load_till < n_pages {
                let vector: Vec<page::PageNumber> = (init_load_till as i32..n_pages as i32)
                    .map(page::PageNumber::new)
                    .collect();
                model.extend_from_slice(&vector);
            }
            glib::ControlFlow::Break
        });

        // move keyboard focus off the header entry so h/l/arrows work
        self.scrolledwindow.grab_focus();
    }

    // Track the page at the centre of the viewport as the user scrolls (by touchpad, scrollbar or
    // drag) and keep the selection on it, so navigation and the page indicator reflect where the
    // user actually is. Page index under a point in the scrolled window's viewport coordinates.
    fn page_index_at(&self, x: f64, y: f64) -> Option<i32> {
        let mut node = self.scrolledwindow.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(n) = node {
            if let Some(page) = n.downcast_ref::<page::Page>() {
                return Some(page.index());
            }
            node = n.parent();
        }
        None
    }

    fn setup_scroll_selection_sync(&self) {
        self.scrolledwindow
            .hadjustment()
            .connect_value_changed(clone!(
                #[weak(rename_to = imp)]
                self,
                move |_| imp.schedule_selection_sync()
            ));
    }

    // Mark the view as scrolling and (re)arm the settle timer. Driven by user scroll input (wheel,
    // touchpad, page steps) - deliberately NOT by the hadjustment, whose animation frames would keep
    // resetting the timer for the whole glide and hold full renders hostage until the slide settled.
    // When the timer fires (input has been quiet for SETTLE_MS), drop the scrolling flag and redraw
    // the on-screen pages so their full renders get scheduled.
    fn note_scroll_activity(&self) {
        self.state.set_scrolling(true);
        if let Some(source) = self.settle_source.take() {
            source.remove();
        }
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(SETTLE_MS),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move || {
                    imp.settle_source.replace(None);
                    imp.state.set_scrolling(false);
                    imp.refresh_visible_renders();
                }
            ),
        );
        self.settle_source.replace(Some(source));
    }

    // Load the render-thread setting into the spin button and pool, and persist any user change.
    fn setup_thread_setting(&self) {
        let max = crate::config::max_render_threads();
        let threads = crate::config::load_render_threads();
        self.spin_threads.set_range(1.0, max as f64);
        self.spin_threads.set_increments(1.0, 1.0);
        self.spin_threads.set_value(threads as f64);
        self.apply_render_threads(threads);

        self.spin_threads.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |spin| {
                let n = spin.value() as usize;
                imp.apply_render_threads(n);
                crate::config::save_render_threads(n);
            }
        ));
    }

    fn apply_render_threads(&self, n: usize) {
        log::info!("Render threads: {n}");
        self.state.set_render_threads(n);
        crate::page::set_render_threads(n);
    }

    // Count pages that fit fully across the viewport width; prefetch depth is derived from it.
    fn update_visible_page_count(&self) {
        let viewport_w = f64::from(self.scrolledwindow.width());
        if viewport_w <= 0.0 {
            return;
        }
        let mut count = 0;
        let mut child = self.listview.first_child();
        while let Some(c) = child {
            if let Some(page) = descendant_page(&c) {
                if page.is_mapped() && page.width() > 0 {
                    if let Some(p) = page.compute_point(
                        &*self.scrolledwindow,
                        &gtk::graphene::Point::new(0.0, 0.0),
                    ) {
                        let left = f64::from(p.x());
                        let right = left + f64::from(page.width());
                        if left >= -0.5 && right <= viewport_w + 0.5 {
                            count += 1;
                        }
                    }
                }
            }
            child = c.next_sibling();
        }
        self.state.set_visible_page_count(count);
    }

    // Redraw every page widget currently laid out in the viewport. With scrolling now off, each
    // one's draw schedules its full render (and prefetch), so the settled pages sharpen.
    fn refresh_visible_renders(&self) {
        self.update_visible_page_count();
        let mut child = self.listview.first_child();
        while let Some(c) = child {
            if let Some(page) = descendant_page(&c) {
                if page.is_mapped() {
                    page.queue_draw();
                }
            }
            child = c.next_sibling();
        }
    }

    // Coalesce a burst of scroll events into a single sync run on idle, after the list view has
    // re-laid-out. Running per-event would read stale page positions mid-burst (e.g. aggressive
    // wheeling) and mis-select.
    fn schedule_selection_sync(&self) {
        // during an animated one-page slide the selection is already set explicitly; skip the
        // viewport sync so the moving pages don't fight it
        if self.scroll_anim.borrow().is_some() {
            return;
        }
        if self.sync_pending.replace(true) {
            return;
        }
        glib::idle_add_local_once(clone!(
            #[weak(rename_to = imp)]
            self,
            move || {
                imp.sync_pending.set(false);
                imp.sync_selection_to_viewport();
            }
        ));
    }

    // Sample the viewport across its width. Keep the current selection as long
    // as its page is still visible anywhere; only move it once that page has
    // scrolled off. This anchors wheel/h/l navigation and ignores the layout
    // shifts from crop/zoom recompute, while still following free scroll.
    fn sync_selection_to_viewport(&self) {
        let (w, h) = (self.scrolledwindow.width(), self.scrolledwindow.height());
        let n_items = self.selection.n_items();
        if w == 0 || n_items == 0 {
            return;
        }
        self.update_visible_page_count();

        let selected = self.selection.selected() as i32;
        let cy = f64::from(h) / 2.0;

        let mut center = None;
        for (i, frac) in [0.05, 0.275, 0.5, 0.725, 0.95].iter().enumerate() {
            let Some(index) = self.page_index_at(f64::from(w) * frac, cy) else {
                continue;
            };
            if index == selected {
                return;
            }
            if i == 2 {
                center = Some(index);
            }
        }

        if let Some(index) = center {
            if index >= 0 && (index as u32) < n_items {
                self.selection.set_selected(index as u32);
            }
        }
    }

    fn setup_search(&self) {
        // let the search bar drive its entry (built-in reveal/conceal plumbing)
        self.search_bar.connect_entry(&*self.search_entry);

        // Route every dismissal (Esc, close button, stop-search) through one cleanup.
        self.search_bar.connect_search_mode_enabled_notify(clone!(
            #[weak(rename_to = imp)]
            self,
            move |bar| {
                if !bar.is_search_mode() {
                    imp.clear_search();
                }
            }
        ));

        // Window-level keys so Ctrl+F / F3 / Esc work regardless of focus. Capture phase lets F3 fire
        // while typing and stops Esc from double-firing the entry's stop-search.
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        key.connect_key_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _keycode, modifier| imp.handle_global_key(keyval, modifier)
        ));
        self.obj().add_controller(key);
    }

    fn handle_global_key(&self, keyval: Key, modifier: ModifierType) -> glib::Propagation {
        match keyval {
            Key::f if modifier.contains(ModifierType::CONTROL_MASK) => {
                self.open_search();
                glib::Propagation::Stop
            }
            Key::F3 => {
                if modifier.contains(ModifierType::SHIFT_MASK) {
                    self.prev_match();
                } else {
                    self.next_match();
                }
                glib::Propagation::Stop
            }
            Key::Escape if self.search_bar.is_search_mode() => {
                self.search_bar.set_search_mode(false);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    fn open_search(&self) {
        self.search_bar.set_search_mode(true);
        self.search_entry.grab_focus();
        self.search_entry.select_region(0, -1);
        // restore highlights for a leftover query
        let query = self.search_entry.text().to_string();
        if !query.is_empty() {
            self.run_search(query);
        }
    }

    // Cleanup on dismissal: clear highlights, refocus the document, but keep the query text so
    // reopening restores it.
    fn clear_search(&self) {
        if let Some(source) = self.search_debounce.take() {
            source.remove();
        }

        let pages: Vec<i32> = {
            let search = self.state.search();
            let mut search = search.borrow_mut();
            let pages = search.results.keys().copied().collect();
            search.clear();
            pages
        };
        for page in pages {
            self.redraw_page(page);
        }
        self.update_search_status();
        self.scrolledwindow.grab_focus();
    }

    #[template_callback]
    fn menu_search(&self, btn: &Button) {
        // dismiss the settings popover first
        if let Some(popover) = btn
            .ancestor(gtk::Popover::static_type())
            .and_downcast::<gtk::Popover>()
        {
            popover.popdown();
        }
        self.open_search();
    }

    #[template_callback]
    fn search_changed(&self, entry: &SearchEntry) {
        self.schedule_search(entry.text().to_string());
    }

    #[template_callback]
    fn search_activate(&self) {
        // Enter advances; the first match was auto-revealed when the sweep began
        self.next_match();
    }

    #[template_callback]
    fn search_stop(&self) {
        self.search_bar.set_search_mode(false);
    }

    #[template_callback]
    fn search_next(&self) {
        self.next_match();
    }

    #[template_callback]
    fn search_prev(&self) {
        self.prev_match();
    }

    #[template_callback]
    fn search_case_toggled(&self, btn: &ToggleButton) {
        self.state.search().borrow_mut().case_sensitive = btn.is_active();
        // deliberate action, no debounce
        self.run_search(self.search_entry.text().to_string());
    }

    fn schedule_search(&self, query: String) {
        if let Some(source) = self.search_debounce.take() {
            source.remove();
        }
        // clear immediately when emptied, so highlights vanish without a delay
        if query.is_empty() {
            self.run_search(query);
            return;
        }
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move || {
                    imp.search_debounce.replace(None);
                    imp.run_search(query);
                }
            ),
        );
        self.search_debounce.replace(Some(source));
    }

    // Launch a fresh sweep: cancel the previous (epoch bump), clear old highlights, then stream
    // results back and repaint pages as matches arrive.
    fn run_search(&self, query: String) {
        let old_pages: Vec<i32> = self
            .state
            .search()
            .borrow()
            .results
            .keys()
            .copied()
            .collect();

        let (epoch, shared_epoch, flags) = {
            let search = self.state.search();
            let mut search = search.borrow_mut();
            search.query = query.clone();
            let (epoch, shared) = search.begin_sweep();
            (epoch, shared, search.flags())
        };

        for page in old_pages {
            self.redraw_page(page);
        }
        self.update_search_status();

        let Some(doc) = self.state.doc() else {
            return;
        };
        if query.is_empty() {
            return;
        }

        let mut rx = crate::search::spawn_search(
            self.state.uri(),
            query,
            flags,
            doc.n_pages(),
            self.selection.selected() as i32,
            epoch,
            shared_epoch,
        );

        glib::spawn_future_local(clone!(
            #[weak(rename_to = imp)]
            self,
            async move {
                while let Some(update) = rx.next().await {
                    {
                        let search = imp.state.search();
                        let mut search = search.borrow_mut();
                        if update.epoch != search.epoch() {
                            continue; // superseded
                        }
                        let first = search.current.is_none();
                        search.results.insert(update.page, update.rects);
                        if first {
                            // outward order => first arrival is the nearest match
                            search.current = Some((update.page, 0));
                        }
                        drop(search);
                        if first {
                            imp.reveal_current();
                        }
                    }
                    imp.redraw_page(update.page);
                    imp.update_search_status();
                }

                // sweep done (or superseded); report no results if it found nothing
                let search = imp.state.search();
                let search = search.borrow();
                if search.epoch() == epoch && !search.query.is_empty() && search.total() == 0 {
                    imp.search_status.set_text("No results");
                }
            }
        ));
    }

    fn next_match(&self) {
        self.move_match(true);
    }

    fn prev_match(&self) {
        self.move_match(false);
    }

    fn move_match(&self, forward: bool) {
        let (old, new) = {
            let search = self.state.search();
            let mut search = search.borrow_mut();
            let Some(next) = search.step(forward) else {
                return;
            };
            let old = search.current;
            search.current = Some(next);
            (old, next)
        };
        if let Some((page, _)) = old {
            self.redraw_page(page);
        }
        self.reveal_current();
        self.redraw_page(new.0);
        self.update_search_status();
    }

    // Bring the current match into view: select its page (keeping entry focus), then scroll
    // horizontally to the match once the page is laid out.
    fn reveal_current(&self) {
        let (page, rect) = {
            let search = self.state.search();
            let search = search.borrow();
            let Some((p, i)) = search.current else {
                return;
            };
            let Some(r) = search.rect(p, i) else {
                return;
            };
            (p, r)
        };

        self.scroll_to_page_no_focus(page);
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(60),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move || {
                    imp.reveal_match_x(page, rect);
                    imp.redraw_page(page);
                }
            ),
        );
    }

    fn scroll_to_page_no_focus(&self, page_index: i32) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };
        let idx = (page_index.max(0) as u32).min(selection.n_items().saturating_sub(1));
        // SELECT only (no FOCUS) so typing focus stays in the entry
        self.listview
            .scroll_to(idx, gtk::ListScrollFlags::SELECT, None);
    }

    // Scroll horizontally if the current match's column is off-screen, landing it near the left third.
    // No-op unless its page is selected and laid out.
    fn reveal_match_x(&self, page_index: i32, rect: page::Rectangle) {
        if self.selection.selected() as i32 != page_index {
            return;
        }
        let Some(left_x) = self.selected_page_left_x() else {
            return;
        };
        let vw = f64::from(self.scrolledwindow.width());
        if vw <= 0.0 {
            return;
        }
        let zoom = self.state.zoom();
        let bbox_x1 = self
            .state
            .bbox_cache()
            .borrow()
            .get(&page_index)
            .map_or(0.0, |b| b.x1);
        let match_x = left_x + (rect.x1 - bbox_x1) * zoom;
        let margin = vw * 0.15;
        if match_x < margin || match_x > vw - margin {
            *self.scroll_anim.borrow_mut() = None; // stop any in-flight slide
            let hadj = self.scrolledwindow.hadjustment();
            let delta = match_x - vw * 0.3;
            hadj.set_value(self.clamp_scroll(hadj.value() + delta));
        }
    }

    fn redraw_page(&self, index: i32) {
        let mut child = self.listview.first_child();
        while let Some(c) = child {
            if let Some(page) = descendant_page(&c) {
                if page.index() == index && page.is_mapped() {
                    page.queue_draw();
                }
            }
            child = c.next_sibling();
        }
    }

    fn update_search_status(&self) {
        let search = self.state.search();
        let search = search.borrow();
        let text = if search.query.is_empty() {
            String::new()
        } else if let Some(ordinal) = search.current_ordinal() {
            format!("{ordinal} / {}", search.total())
        } else {
            // query set, no match yet: still searching
            "Searching…".to_string()
        };
        self.search_status.set_text(&text);
    }

    #[template_callback]
    fn jump_back(&self) {
        if let Some(page) = self.state.jump_list_pop() {
            self.navigate_to_page(page);
        }
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn can_jump_back(&self, prev_page: u32) -> bool {
        prev_page > 0
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn back_btn_text(&self, prev_page: u32) -> String {
        format!("Jump back to page {prev_page}")
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn page_entry_text(&self, page: i32) -> String {
        format!("{}", page + 1)
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn zoom_entry_text(&self, zoom_value: f64) -> String {
        format!("{}", zoom_value * 100.0)
    }
}

// Trait shared by all widgets
impl WidgetImpl for Window {}

// Trait shared by all windows
impl WindowImpl for Window {}

// Trait shared by all application windows
impl ApplicationWindowImpl for Window {}

// Accumulate a scroll delta and decide whether to step a page. Returns the new accumulator and the
// page step (+1 next, -1 prev, 0 none). `notch` is the travel that advances one page; the first
// page fires as soon as `trigger` (< notch) has accumulated so the gesture responds immediately,
// then one page per full notch (steps land at cumulative trigger, trigger+notch, … ). The step is
// gated on the event's direction so the opposite-signed remainder left after a fire can't trigger a
// bogus step in reverse. Shared by the mouse wheel (notch in unitless clicks: a hi-res wheel splits
// one physical notch into sub-events summing to 1.0) and vertical touchpad scroll (notch in pixels
// of finger travel).
fn accumulate_step(accum: f64, prev: f64, delta: f64, notch: f64, trigger: f64) -> (f64, i32) {
    // On a direction reversal, drop the leftover under-shoot from the previous direction: that
    // residual belongs to the old direction, and counting it toward the new one gives it a head
    // start that fires an extra page.
    let base = if delta * prev < 0.0 { 0.0 } else { accum };
    let accum = base + delta;
    if delta > 0.0 && accum >= trigger {
        (accum - notch, 1)
    } else if delta < 0.0 && accum <= -trigger {
        (accum + notch, -1)
    } else {
        (accum, 0)
    }
}

// One frame of the exponential glide toward `target`. Returns the next scroll value and whether the
// glide has settled. `dt_us` is clamped so a stalled-then-resumed frame clock can't spike the gain
// into a sustained oscillation; the glide settles on arrival, on a sub-pixel step, or once
// `elapsed_us` passes the ceiling (bounding a target that relayout keeps nudging).
fn glide_step(value: f64, target: f64, dt_us: i64, elapsed_us: i64, dir: f64) -> (f64, bool) {
    let dt = dt_us.min(SCROLL_ANIM_MAX_DT_US);
    let k = if dt <= 0 {
        0.0
    } else {
        1.0 - (-(dt as f64) / SCROLL_ANIM_TAU_US).exp()
    };
    let remaining = target - value;
    let mut step = remaining * k;
    // Floor the approach speed so the exponential tail closes in a few frames rather than crawling
    // toward a target it never reaches (which forced a settle-and-snap a few pixels short). Never
    // overshoot the target.
    if dt > 0 && step.abs() < SCROLL_ANIM_MIN_STEP {
        step = SCROLL_ANIM_MIN_STEP.copysign(remaining);
    }
    if step.abs() > remaining.abs() {
        step = remaining;
    }
    let mut next = value + step;
    // Never move against the travel direction: crop relayout can jump the live target backward for a
    // frame, which otherwise shows as a visible reverse (the "back and forth" wobble).
    if dir > 0.0 {
        next = next.max(value);
    } else if dir < 0.0 {
        next = next.min(value);
    }
    // Settle on arrival, when the target has jumped behind us (relayout overshoot; don't reverse to
    // chase it), or once the glide has run past its ceiling.
    let overshot = (dir > 0.0 && target < value) || (dir < 0.0 && target > value);
    let settled = (target - next).abs() < 0.5 || overshot || elapsed_us > SCROLL_ANIM_MAX_US;
    (next, settled)
}

// Find the Page widget within a list item's widget subtree.
fn descendant_page(widget: &gtk::Widget) -> Option<page::Page> {
    if let Some(page) = widget.downcast_ref::<page::Page>() {
        return Some(page.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(page) = descendant_page(&c) {
            return Some(page);
        }
        child = c.next_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        accumulate_step, glide_step, SCROLL_ANIM_MAX_US, TOUCHPAD_NOTCH, TOUCHPAD_TRIGGER,
        WHEEL_NOTCH, WHEEL_TRIGGER,
    };

    // Drive the glide toward a fixed target at a steady frame rate; return frames until it settles.
    fn glide_frames(mut value: f64, target: f64, dt_us: i64) -> usize {
        let dir = (target - value).signum();
        let mut elapsed = 0;
        for frame in 1..100_000 {
            let (next, settled) = glide_step(value, target, dt_us, elapsed, dir);
            value = next;
            elapsed += dt_us;
            if settled {
                return frame;
            }
        }
        panic!("glide never settled");
    }

    #[test]
    fn glide_settles_at_normal_frame_rate() {
        // a full page-width glide at 60fps settles in well under a second
        let frames = glide_frames(0.0, 500.0, 16_000);
        assert!(frames > 1 && frames < 60, "settled in {frames} frames");
    }

    #[test]
    fn glide_settles_even_with_huge_dt() {
        // a stalled-then-resumed clock delivers a 1s dt; clamping keeps the step bounded (no wild
        // overshoot) and it still converges
        let (next, _) = glide_step(0.0, 500.0, 1_000_000, 0, 1.0);
        assert!(next < 500.0, "clamped step overshot: {next}");
        glide_frames(0.0, 500.0, 1_000_000);
    }

    #[test]
    fn glide_force_settles_past_ceiling() {
        // a target far away still settles once the duration ceiling is passed
        let (_, settled) = glide_step(0.0, 100_000.0, 16_000, SCROLL_ANIM_MAX_US + 1, 1.0);
        assert!(settled);
    }

    #[test]
    fn glide_never_reverses_on_backward_target_jump() {
        // relayout jumps the target behind us mid-forward-glide: the view must not move back
        let (next, settled) = glide_step(100.0, 50.0, 16_000, 0, 1.0);
        assert_eq!(next, 100.0, "glide reversed to {next}");
        assert!(settled, "should settle rather than crawl backward");
    }

    #[test]
    fn glide_lands_on_target_without_snapping() {
        // the glide must ease all the way onto the target, not settle a few pixels short and snap
        // (which is the visible jerk); the last moving step is bounded so there's no jump
        let mut value = 0.0;
        let mut elapsed = 0;
        loop {
            let (next, settled) = glide_step(value, 500.0, 16_000, elapsed, 1.0);
            let step = next - value;
            value = next;
            elapsed += 16_000;
            if settled {
                assert!(step.abs() < 2.0, "final step was a {step}px jump");
                break;
            }
            assert!(elapsed < 5_000_000, "glide never settled");
        }
        assert!((value - 500.0).abs() < 0.5, "landed short at {value}");
    }

    // Run a sequence of wheel deltas through the accumulator at wheel scale.
    fn run(deltas: &[f64]) -> Vec<i32> {
        run_scaled(deltas, WHEEL_NOTCH, WHEEL_TRIGGER)
    }

    fn run_scaled(deltas: &[f64], notch: f64, trigger: f64) -> Vec<i32> {
        let mut accum = 0.0;
        let mut prev = 0.0;
        deltas
            .iter()
            .map(|&d| {
                let (a, step) = accumulate_step(accum, prev, d, notch, trigger);
                accum = a;
                prev = d;
                step
            })
            .collect()
    }

    // One physical notch as a hi-res wheel reports it: several sub-events (multiples of 1/15)
    // summing to 1.0.
    const NOTCH: [f64; 4] = [7.0 / 15.0, 2.0 / 15.0, 4.0 / 15.0, 2.0 / 15.0];

    #[test]
    fn one_notch_steps_exactly_one_page_and_never_reverses() {
        let steps = run(&NOTCH);
        assert_eq!(steps.iter().sum::<i32>(), 1);
        // the opposite-signed remainder after the fire must not trigger a step back
        assert!(
            steps.iter().all(|&s| s >= 0),
            "unexpected reverse step: {steps:?}"
        );
    }

    #[test]
    fn wheel_responds_on_the_first_sub_event() {
        assert_eq!(run(&NOTCH)[0], 1);
    }

    #[test]
    fn two_notches_step_two_pages() {
        let seq: Vec<f64> = NOTCH.iter().chain(NOTCH.iter()).copied().collect();
        assert_eq!(run(&seq).iter().sum::<i32>(), 2);
    }

    #[test]
    fn reverse_notch_steps_one_page_back_and_never_advances() {
        let back: Vec<f64> = NOTCH.iter().map(|d| -d).collect();
        let steps = run(&back);
        assert_eq!(steps.iter().sum::<i32>(), -1);
        assert!(
            steps.iter().all(|&s| s <= 0),
            "unexpected forward step: {steps:?}"
        );
    }

    #[test]
    fn forward_then_reverse_nets_zero() {
        let seq: Vec<f64> = NOTCH
            .iter()
            .copied()
            .chain(NOTCH.iter().map(|d| -d))
            .collect();
        assert_eq!(run(&seq).iter().sum::<i32>(), 0);
    }

    #[test]
    fn reversal_after_partial_forward_steps_one_page_back_not_two() {
        // Stop mid-notch right after a forward page fires (accumulator left at a negative
        // residual), then reverse a full notch. The stale residual must not fire a second page back
        let mut seq = vec![7.0 / 15.0]; // one forward sub-event: fires +1, leaves residual
        seq.extend(NOTCH.iter().map(|d| -d)); // a full reverse notch
        let steps = run(&seq);
        assert_eq!(steps[0], 1, "forward sub-event should fire once");
        assert_eq!(
            steps[1..].iter().sum::<i32>(),
            -1,
            "reversal must step exactly one page back, got {:?}",
            &steps[1..]
        );
    }

    #[test]
    fn reversal_from_rest_steps_one_page_back() {
        // A full forward notch leaves the accumulator at rest, then a reverse notch.
        let seq: Vec<f64> = NOTCH
            .iter()
            .copied()
            .chain(NOTCH.iter().map(|d| -d))
            .collect();
        let steps = run(&seq);
        assert_eq!(steps[..NOTCH.len()].iter().sum::<i32>(), 1);
        assert_eq!(steps[NOTCH.len()..].iter().sum::<i32>(), -1);
    }

    #[test]
    fn low_res_wheel_single_event_steps_one_page() {
        assert_eq!(run(&[1.0]), vec![1]);
        assert_eq!(run(&[-1.0]), vec![-1]);
    }

    #[test]
    fn first_step_early_then_paced_one_notch_apart() {
        // 0.25 per event (exactly representable): first page at 0.25, then every full notch (four
        // events). Fires land at 0, 4, 8, 12.
        let fired: Vec<usize> = run(&[0.25; 16])
            .iter()
            .enumerate()
            .filter(|(_, &s)| s != 0)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(fired, vec![0, 4, 8, 12]);
    }

    #[test]
    fn touchpad_scale_first_page_fires_before_a_full_notch() {
        // One trigger's worth of travel (well under a notch) still turns a page.
        let steps = run_scaled(&[TOUCHPAD_TRIGGER], TOUCHPAD_NOTCH, TOUCHPAD_TRIGGER);
        assert_eq!(steps, vec![1]);
    }

    #[test]
    fn touchpad_full_swipe_steps_about_fifteen_pages() {
        let deltas = vec![12.0; 44];
        let steps: i32 = run_scaled(&deltas, TOUCHPAD_NOTCH, TOUCHPAD_TRIGGER)
            .iter()
            .sum();
        assert_eq!(steps, 14);
    }
}
