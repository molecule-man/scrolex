// Application window behavior, navigation, and viewport/render coordination.
use std::cell::{Cell, RefCell};

use futures::StreamExt;
use glib::clone;
use glib::subclass::InitializingObject;
use gtk::gdk::{EventSequence, Key, ModifierType, BUTTON_PRIMARY};
use gtk::glib::closure_local;
use gtk::glib::subclass::prelude::*;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::subclass::prelude::*;
use gtk::{
    glib, Button, CompositeTemplate, Label, ListView, ScrolledWindow, SearchBar, SearchEntry,
    SingleSelection, ToggleButton,
};
use gtk::{prelude::*, GestureClick};

use crate::page;
use crate::state::State;

// Time constant of the exponential glide toward the target page position. Larger = slower and
// smoother; the perceived slide runs a few times this long. The glide is a low-pass follow, which
// damps the hadjustment jitter that GtkListView injects when async crop relayout makes it re-anchor
// mid-slide, so the page settles instead of vibrating.
const SCROLL_ANIM_TAU_US: f64 = 130_000.0;

// Ceiling on the per-frame glide gain. A stalled-then-resumed clock's huge dt would drive the gain
// toward 1 and snap onto a re-anchoring target; capping keeps the follow contractive. Above the
// 10fps gain, so it only engages on pathological stalls, not in the normal 10-60fps range.
const SCROLL_ANIM_MAX_GAIN: f64 = 0.7;
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
// Touchpad pixels per notch, used to scale a pinch's pixel travel onto the wheel's zoom rate.
const TOUCHPAD_NOTCH: f64 = 40.0;

// Time constant of the kinetic scroll that continues a touchpad swipe after the fingers lift. Travel
// lift-off speed times this, so a brisk swipe covers a couple of pages. It also sets how fast the
// pages are still moving mid-coast: the speed decays as exp(-t/tau).
const KINETIC_TAU_US: f64 = 420_000.0;
// Lift-off speed (px/s) under which the swipe counts as stopped rather than flung.
const KINETIC_MIN_VELOCITY: f64 = 100.0;

// Multiplicative zoom step per notch.
const ZOOM_STEP: f64 = 1.1;

// Quiet period after the last keystroke before a search sweep launches, coalescing a burst of typing.
const SEARCH_DEBOUNCE_MS: u64 = 100;

// In-flight state of the animated one-page slide.
//
// The end position is recomputed live each tick from the selected page widget's actual geometry, so
// async crop/zoom layout shifts during the slide are compensated and the page still lands at the
// same on-screen spot. `anchor_x` is the viewport x where the selected page's left edge should come
// to rest. `last_target` remembers the most recent geometry-derived resting position; it is used
// when the selected page isn't realised yet (e.g. selection has raced ahead during a burst) so the
// slide chases only real page positions and never overshoots into a reverse correction. The list
// view rewrites those coordinates, so rebase it before use (`rebase_target`).
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
    // where we left the hadjustment: our last write, or its value when the glide was armed. A gap vs.
    // this tick's read is someone else moving it (GtkListView re-anchor, GTK deceleration).
    last_next: f64,
    // decay time constant of this glide; page steps and kinetic scrolls move at different rates
    tau_us: f64,
    // a kinetic scroll: driven by `vel` rather than a target, and a new swipe or a page step takes
    // it over. The other fields above describe the page step and are inert here.
    kinetic: bool,
    // kinetic scroll only: speed left to spend, px/s
    vel: f64,
}

// Vertical coast after a touchpad flick. There is no page to land on, so speed is all it needs.
#[derive(Clone, Copy)]
struct KineticPan {
    // px/s, decays every tick
    vel: f64,
    // previous tick's frame time; -1 until the first tick
    last_frame: i64,
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
    pub spin_cache: TemplateChild<gtk::SpinButton>,
    #[template_child]
    pub btn_jump_back: TemplateChild<Button>,
    #[template_child]
    pub scrolledwindow: TemplateChild<ScrolledWindow>,
    // outer scroller that provides the vertical axis the horizontal listview can't; pans a
    // zoomed-in page whose rendered height exceeds the viewport
    #[template_child]
    pub vscrolledwindow: TemplateChild<ScrolledWindow>,
    #[template_child]
    pub pan_scroll: TemplateChild<gtk::EventControllerScroll>,
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
    pub btn_toc: TemplateChild<ToggleButton>,
    #[template_child]
    pub toc_revealer: TemplateChild<gtk::Revealer>,
    #[template_child]
    pub toc_list: TemplateChild<gtk::ListBox>,
    #[template_child]
    pub loading_overlay: TemplateChild<gtk::Box>,
    #[template_child]
    pub loading_spinner: TemplateChild<gtk::Spinner>,

    // target page per outline row (index-aligned), None for non-navigable entries
    toc_pages: RefCell<Vec<Option<i32>>>,

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

    // true while a tick callback drives scroll_anim; keeps a retarget from adding a second one
    scroll_ticking: Cell<bool>,

    vscroll_anim: Cell<Option<KineticPan>>,

    // a tick callback drives vscroll_anim; a new flick must not add a second one
    vscroll_ticking: Cell<bool>,

    // set when the current scroll sequence must not coast on: mouse wheel, pinch, ctrl+scroll zoom
    kinetic_blocked: Cell<bool>,

    // last horizontal scroll position, to derive travel direction (prefetch reads toward it) from
    // any scroll path - free-scroll included, not just page steps
    last_hadj: Cell<f64>,

    // the position set_hscroll asked for last, and which input asked for it. If the position ends up
    // somewhere else, something other than us moved it.
    hscroll_intent: Cell<(f64, &'static str)>,

    // the selected page and where its left edge sits on screen, as of the last position change. This
    // is what the reader sees; the adjustment value is not. When page widths change, the list view
    // can change its own coordinates, which moves the value while the page stays where it is.
    seen_page_x: Cell<Option<(u32, f64)>>,

    // accumulates hi-res mouse-wheel deltas (libinput splits one notch into sub-events summing to
    // 1.0); a page advances when the total crosses a notch, keeping the remainder. Not reset between
    // gestures, so a reverse after a pause can need up to ~0.8 notch - accepted.
    wheel_accum: Cell<f64>,
    // the previous wheel delta, to detect a direction reversal and reset the accumulator so
    // reversing doesn't over-step
    wheel_last_dy: Cell<f64>,

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
        self.setup_cache_setting();
        let cfg = crate::config::load_config();
        self.state.set_preview_cache_pages(cfg.preview_cache_pages);
        self.setup_animate_scroll();
        self.setup_text_selection();
        self.setup_search();
        self.setup_toc();
        self.setup_drop_target();

        // Give keyboard focus to the scroll area rather than the header entry
        self.scrolledwindow.set_focusable(true);
        self.listview.set_focusable(false);
        let scrolledwindow = self.scrolledwindow.clone();
        self.obj().connect_map(move |_| {
            scrolledwindow.grab_focus();
        });

        // Drop this window's render-pool state when it closes, so its entries don't linger.
        self.obj().connect_close_request(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                let client = imp.state.render_client_id();
                crate::page::clear_all_renders(client);
                crate::page::set_wanted_pages(client, None);
                glib::Propagation::Proceed
            }
        ));
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

    // Runs in the capture phase, so the scrollers' kinetic controllers never get the event.
    // If it does, a touchpad flick leaves it decelerating for ~1s, writing positions from its own
    // model. Meanwhile GtkListView shifts its coordinate origin as page widths get measured. With
    // varying page sizes the two fight every frame and pages jump by almost a page (issue #41).
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
            self.kinetic_blocked.set(true);
            return glib::Propagation::Stop;
        }

        // Ctrl+scroll zooms instead of navigating; dy<0 zooms in, dy>0 out. Touchpad pixels are
        // scaled to the wheel's notch so both zoom at a comparable rate.
        if scroll
            .current_event_state()
            .contains(ModifierType::CONTROL_MASK)
        {
            self.kinetic_blocked.set(true);
            if dy != 0.0 {
                let notches = match unit {
                    gtk::gdk::ScrollUnit::Wheel => dy,
                    _ => dy / TOUCHPAD_NOTCH,
                };
                self.zoom_to(self.state.zoom() * ZOOM_STEP.powf(-notches));
            }
            return glib::Propagation::Stop;
        }

        match unit {
            // Mouse wheel: hi-res wheels split one notch into several sub-events summing to 1.0.
            // Fire the first page as soon as a small fraction of a notch has scrolled (TRIGGER) so
            // the wheel responds immediately, then pace one page per full notch (subtract NOTCH,
            // keep the remainder). Steps land at cumulative 0.2, 1.2, 2.2, … of a notch.
            gtk::gdk::ScrollUnit::Wheel => {
                // the wheel steps pages; it never coasts on
                self.kinetic_blocked.set(true);
                let (accum, step) = accumulate_step(
                    self.wheel_accum.get(),
                    self.wheel_last_dy.get(),
                    dy,
                    WHEEL_NOTCH,
                    WHEEL_TRIGGER,
                );
                log::trace!(
                    target: "scrolex::pan",
                    "wheel scroll: dy={dy:+.3} prev_dy={:+.3} accum {:+.3} -> {accum:+.3} step={step:+}",
                    self.wheel_last_dy.get(),
                    self.wheel_accum.get(),
                );
                self.wheel_accum.set(accum);
                self.wheel_last_dy.set(dy);
                self.step_page(step);
            }
            // Touchpad (and any other pixel-precise device): a horizontal swipe pans the page flow;
            // a vertical swipe pans a zoomed-in page along the axis the outer scroller owns.
            _ => {
                // a new swipe takes over from the coast the previous one left running
                self.cancel_coast();
                // Apply the horizontal delta to the scroll position, but only when no page slide is
                // running. During a slide `scroll_tick` owns the adjustment; adding dx here (from a
                // scroll event that arrives mid-slide) would fight its writes frame by frame.
                let sliding = self.scroll_anim.borrow().is_some();
                if !sliding {
                    let hadj = self.scrolledwindow.hadjustment();
                    self.set_hscroll(self.clamp_scroll(hadj.value() + dx), "touchpad");
                }
                log::trace!(
                    target: "scrolex::pan",
                    "touchpad scroll: dx={dx:+.2} dy={dy:+.2}{}",
                    // a slide that never ends would ignore every swipe
                    if sliding { " DROPPED (slide in progress)" } else { "" },
                );

                // Vertical pan is independent of the horizontal slide, so it always applies. The
                // adjustment clamps to its range, so this is a no-op when the page fits the viewport.
                let vadj = self.vscrolledwindow.vadjustment();
                vadj.set_value(vadj.value() + dy);
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

    #[template_callback]
    fn handle_scroll_begin(&self) {
        self.kinetic_blocked.set(false);
    }

    // End of a touchpad swipe. GTK measured the lift-off speed for us; keep scrolling with it so long
    // distances don't need swipe after swipe. Both axes coast, because both axes pan.
    //
    // The speeds are px/s. GTK's docs say px/ms, but its code divides pixels by a time in ms and then
    // scales by 1000. GtkScrolledWindow feeds them to the same place as GtkGestureSwipe, also px/s.
    #[template_callback]
    fn handle_decelerate(&self, vel_x: f64, vel_y: f64) {
        if self.kinetic_blocked.get() {
            return;
        }
        if vel_x.abs() >= KINETIC_MIN_VELOCITY {
            self.start_kinetic_scroll(vel_x);
        }
        if vel_y.abs() >= KINETIC_MIN_VELOCITY {
            self.start_kinetic_pan(vel_y);
        }
    }

    // Keep the pages moving after the fingers lift: the swipe's speed decays over KINETIC_TAU_US, which
    // covers about `vel_x * tau` pixels. Speed, not a target position - the list view rewrites the
    // scroll coordinates as it measures page widths, and a target would drift with them.
    fn start_kinetic_scroll(&self, vel_x: f64) {
        // a page slide owns the position; don't scroll on top of it
        if self.scroll_anim.borrow().is_some() {
            return;
        }

        let value = self.scrolledwindow.hadjustment().value();
        log::debug!(
            target: "scrolex::scroll",
            "kinetic scroll: vel={vel_x:+.0}px/s hadj={value:.2} travel={:+.0}",
            vel_x * KINETIC_TAU_US / 1_000_000.0,
        );
        *self.scroll_anim.borrow_mut() = Some(ScrollAnim {
            anchor_x: None,
            last_target: value,
            last_frame: -1,
            start_frame: -1,
            dir: vel_x.signum(),
            last_next: value,
            tau_us: KINETIC_TAU_US,
            kinetic: true,
            vel: vel_x,
        });
        self.start_scroll_ticks();
    }

    // Vertical counterpart of start_kinetic_scroll: coasts a zoomed-in page up or down.
    fn start_kinetic_pan(&self, vel_y: f64) {
        let vadj = self.vscrolledwindow.vadjustment();
        // the page fits the viewport: nothing to pan
        if vadj.upper() - vadj.lower() <= vadj.page_size() {
            return;
        }

        log::debug!(
            target: "scrolex::scroll",
            "kinetic pan: vel={vel_y:+.0}px/s vadj={:.2} travel={:+.0}",
            vadj.value(),
            vel_y * KINETIC_TAU_US / 1_000_000.0,
        );
        self.vscroll_anim.set(Some(KineticPan {
            vel: vel_y,
            last_frame: -1,
        }));

        if self.vscroll_ticking.replace(true) {
            return;
        }
        self.vscrolledwindow.add_tick_callback(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move |_, clock| imp.pan_tick(clock)
        ));
    }

    fn pan_tick(&self, clock: &gtk::gdk::FrameClock) -> glib::ControlFlow {
        let Some(mut pan) = self.vscroll_anim.get() else {
            self.vscroll_ticking.set(false);
            return glib::ControlFlow::Break;
        };

        let vadj = self.vscrolledwindow.vadjustment();
        let value = vadj.value();
        let now = clock.frame_time();
        let dt = if pan.last_frame < 0 {
            0
        } else {
            now - pan.last_frame
        };
        pan.last_frame = now;

        let (next, vel, spent) = kinetic_step(value, pan.vel, dt, KINETIC_TAU_US);
        pan.vel = vel;
        vadj.set_value(next);
        // nowhere left to go: vadj clamped our write, so we sit at the top or the bottom
        let pinned = dt > 0 && (vadj.value() - value).abs() < 0.01;

        log::trace!(
            target: "scrolex::scroll",
            "kinetic pan frame: dt={:5.1}ms v={value:9.2} step={:+7.2} left={vel:5.0}px/s",
            dt as f64 / 1000.0,
            vadj.value() - value,
        );

        if spent || pinned {
            self.vscroll_anim.set(None);
            self.vscroll_ticking.set(false);
            log::debug!(
                target: "scrolex::scroll",
                "kinetic pan done: value={:.2}", vadj.value(),
            );
            return glib::ControlFlow::Break;
        }

        self.vscroll_anim.set(Some(pan));
        glib::ControlFlow::Continue
    }

    // Stop both axes. For inputs that own the position outright - a jump, a drag, a new document -
    // because a slide or a coast keeps writing the adjustments every frame and would drag us off.
    fn cancel_scroll_motion(&self) {
        *self.scroll_anim.borrow_mut() = None;
        self.vscroll_anim.set(None);
    }

    // Stop the coasts, on both axes. A page slide survives: it aims at a page and re-reads where that
    // page sits every frame, so a relayout can't throw it off. A coast has nothing to re-aim at.
    fn cancel_coast(&self) {
        if (*self.scroll_anim.borrow()).is_some_and(|a| a.kinetic) {
            *self.scroll_anim.borrow_mut() = None;
        }
        self.vscroll_anim.set(None);
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
        self.cancel_scroll_motion();
        *self.drag_coords.borrow_mut() = self.drag_point_in_window(x, y);

        if let Some(surface) = self.obj().surface() {
            *self.drag_cursor.borrow_mut() = surface.cursor();
            surface.set_cursor(gtk::gdk::Cursor::from_name("grabbing", None).as_ref());
        }
    }

    #[template_callback]
    fn handle_drag_move(&self, seq: Option<&EventSequence>, gc: &GestureClick) {
        let Some((x, y)) = gc
            .point(seq)
            .and_then(|(px, py)| self.drag_point_in_window(px, py))
        else {
            return;
        };
        if let Some((prev_x, prev_y)) = *self.drag_coords.borrow() {
            let hadjustment = self.scrolledwindow.hadjustment();
            self.set_hscroll(hadjustment.value() - (x - prev_x), "drag");
            let vadjustment = self.vscrolledwindow.vadjustment();
            vadjustment.set_value(vadjustment.value() - (y - prev_y));
        }
        *self.drag_coords.borrow_mut() = Some((x, y));
    }

    // Pointer in the fixed window frame, not the inner scroller's: panning slides that inner frame,
    // so deltas measured there feed back into the pan and oscillate.
    fn drag_point_in_window(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        self.scrolledwindow
            .compute_point(&*self.obj(), &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (f64::from(p.x()), f64::from(p.y())))
    }

    #[template_callback]
    fn handle_drag_end(&self) {
        if let Some(surface) = self.obj().surface() {
            surface.set_cursor(self.drag_cursor.borrow().as_ref());
        }
    }

    // Every zoom goes through here. A zoom relayouts the pages, so a coast has to stop first.
    fn zoom_to(&self, zoom: f64) {
        self.cancel_coast();
        self.state.zoom_to(zoom);
    }

    #[template_callback]
    fn zoom_out(&self) {
        self.zoom_to(self.state.zoom() / ZOOM_STEP);
    }

    #[template_callback]
    fn zoom_in(&self) {
        self.zoom_to(self.state.zoom() * ZOOM_STEP);
    }

    #[template_callback]
    fn handle_zoom_begin(&self) {
        // the pinch takes over before it has changed the zoom at all
        self.cancel_coast();
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
        self.zoom_to(self.zoom_gesture_base.get() * scale);
    }

    #[template_callback]
    fn handle_key_press(
        &self,
        keyval: Key,
        _keycode: u32,
        modifier: ModifierType,
    ) -> glib::Propagation {
        match keyval {
            Key::c
                if modifier.contains(ModifierType::CONTROL_MASK) && self.state.has_selection() =>
            {
                self.copy_selection();
            }
            Key::Escape if self.state.has_selection() => {
                self.state.clear_selection();
            }
            Key::o => {
                self.open_document();
            }
            Key::t => {
                if self.btn_toc.is_sensitive() {
                    self.toc_revealer
                        .set_reveal_child(!self.toc_revealer.reveals_child());
                }
            }
            Key::f => {
                self.open_search();
            }
            Key::l | Key::Page_Down => {
                self.next_page();
            }
            Key::h | Key::Page_Up => {
                self.prev_page();
            }
            Key::Home => {
                self.goto_page(1);
            }
            Key::End => {
                // clamps to the last page in navigate_to_page
                self.goto_page(u32::MAX);
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
                let hadj = self.scrolledwindow.hadjustment();
                let step = if hadj.step_increment() > 0.0 {
                    hadj.step_increment()
                } else {
                    hadj.page_size() * 0.1
                };
                let delta = if keyval == Key::Left { -step } else { step };
                self.set_hscroll(hadj.value() + delta, "arrow-key");
            }
            Key::Up | Key::Down | Key::k | Key::j => {
                // vertical pan of a zoomed-in page. The outer scroller owns the vertical axis (the
                // horizontal listview doesn't scroll its cross axis); k/Up pan up, j/Down pan down.
                // the nudge owns the axis; a coast would overwrite it next frame
                self.vscroll_anim.set(None);
                let vadj = self.vscrolledwindow.vadjustment();
                let step = if vadj.step_increment() > 0.0 {
                    vadj.step_increment()
                } else {
                    vadj.page_size() * 0.1
                };
                let up = keyval == Key::Up || keyval == Key::k;
                vadj.set_value(vadj.value() + if up { -step } else { step });
            }
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    }

    // The only writer of the clipboard; a drag publishes to the primary selection instead.
    fn copy_selection(&self) {
        if let Some(text) = self.state.selected_text() {
            self.obj().clipboard().set_text(&text);
        }
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

        self.zoom_to(zoom / 100.0);
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

        self.cancel_scroll_motion();
        self.expect_hscroll("goto-page");
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
        self.expect_hscroll("prev-page select");
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

        self.expect_hscroll("next-page select");
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
        // A page step supersedes a coast: the step anchors on a page, the coast has no anchor to
        // retarget. Both axes stop, so the step decides where the reader lands.
        self.cancel_coast();

        let hadj = self.scrolledwindow.hadjustment();

        // animation toggled off: jump straight to the page
        if !self.state.animate_scroll() {
            self.set_hscroll(self.clamp_scroll(hadj.value() + delta), "page-step");
            return;
        }

        let mut anim = self.scroll_anim.borrow_mut();
        // A retarget resets the duration ceiling (start_frame -1); else a long burst force-settles
        // short of target, landing the page off its anchor. last_frame carries over to keep pacing.
        let (anchor_x, prev_target, last_frame, start_frame) = match anim.as_ref() {
            Some(a) => (
                a.anchor_x,
                Some(self.rebase_target(a, hadj.value())),
                a.last_frame,
                -1,
            ),
            None => (anchor_x, None, -1, -1),
        };
        // Prefer the selected page's exact live position. When it isn't laid out yet (selection
        // raced ahead in a burst), advance by one page-width from the previous target so the burst
        // keeps covering ground; live geometry snaps it to the exact spot once the page is actually
        // mapped.
        let live = self.live_target(anchor_x);
        let last_target =
            live.unwrap_or_else(|| self.clamp_scroll(prev_target.unwrap_or(hadj.value()) + delta));
        log::debug!(
            target: "scrolex::scroll",
            "arm: anchor_x={anchor_x:?} target={last_target:.2} live={} fresh={} hadj={:.2}",
            live.is_some(),
            anim.is_none(),
            hadj.value(),
        );
        *anim = Some(ScrollAnim {
            anchor_x,
            last_target,
            last_frame,
            start_frame,
            dir: delta.signum(),
            last_next: hadj.value(),
            tau_us: SCROLL_ANIM_TAU_US,
            kinetic: false,
            vel: 0.0,
        });
        drop(anim);

        self.start_scroll_ticks();
    }

    fn start_scroll_ticks(&self) {
        if self.scroll_ticking.replace(true) {
            return;
        }
        self.scrolledwindow.add_tick_callback(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move |_, clock| imp.scroll_tick(clock)
        ));
    }

    fn scroll_tick(&self, clock: &gtk::gdk::FrameClock) -> glib::ControlFlow {
        let Some(mut anim) = *self.scroll_anim.borrow() else {
            self.scroll_ticking.set(false);
            return glib::ControlFlow::Break;
        };

        let hadj = self.scrolledwindow.hadjustment();
        let value = hadj.value();
        let prev_target = anim.last_target;
        let drift = value - anim.last_next;

        // Chase the selected page's live resting position. While it slides in from off-screen it
        // isn't realised, so rebase the last known one instead.
        let live = self.live_target(anim.anchor_x);
        let target = live.unwrap_or_else(|| self.rebase_target(&anim, value));
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

        let (next, settled) = if anim.kinetic {
            let (next, vel, spent) = kinetic_step(value, anim.vel, raw_dt, anim.tau_us);
            anim.vel = vel;
            let next = self.clamp_scroll(next);
            // pinned against an end of the document: there is nowhere left to go
            let pinned = raw_dt > 0 && (next - value).abs() < 0.01;
            (next, spent || pinned)
        } else {
            glide_step(
                value,
                target,
                raw_dt,
                now - anim.start_frame,
                anim.dir,
                anim.tau_us,
            )
        };

        // Per-frame trace (RUST_LOG=scrolex::scroll=trace). drift = external hadj motion since our
        // last write; dtgt = how far the target moved; vel = px/ms. Smooth = regular dt, decaying
        // vel, dtgt tracking drift. A kinetic scroll has no target: `left` is the speed to spend.
        let dt_ms = raw_dt as f64 / 1000.0;
        let step_vel = if dt_ms > 0.0 {
            (next - value).abs() / dt_ms
        } else {
            0.0
        };
        if anim.kinetic {
            log::trace!(
                target: "scrolex::scroll",
                "kinetic frame: dt={dt_ms:5.1}ms v={value:9.2} drift={drift:+7.2} step={:+7.2} vel={step_vel:5.2}px/ms left={:5.0}px/s",
                next - value,
                anim.vel,
            );
        } else {
            log::trace!(
                target: "scrolex::scroll",
                "frame: dt={dt_ms:5.1}ms v={value:9.2} drift={drift:+7.2} tgt={target:9.2} dtgt={:+7.2} live={} step={:+7.2} vel={step_vel:5.2}px/ms",
                target - prev_target,
                live.is_some(),
                next - value,
            );
        }
        anim.last_next = next;
        if settled {
            // land at the eased position (within sub-pixel of target) and let the normal sync
            // reconcile selection; never jump to a distant target - that snap is the visible jerk
            *self.scroll_anim.borrow_mut() = None;
            self.scroll_ticking.set(false);
            self.set_hscroll(
                next,
                if anim.kinetic {
                    "kinetic-settle"
                } else {
                    "slide-settle"
                },
            );
            if anim.kinetic {
                log::debug!(
                    target: "scrolex::scroll",
                    "kinetic scroll done: value={next:.2} left_x={:?}",
                    self.selected_page_left_x(),
                );
            } else {
                log::debug!(
                    target: "scrolex::scroll",
                    "settle: target={target:.2} value={next:.2} short={:.2} left_x={:?}",
                    target - next,
                    self.selected_page_left_x(),
                );
            }
            return glib::ControlFlow::Break;
        }

        *self.scroll_anim.borrow_mut() = Some(anim);
        self.set_hscroll(next, if anim.kinetic { "kinetic" } else { "slide" });
        glib::ControlFlow::Continue
    }

    // The only place that changes the horizontal position, so the log can say which input moved it.
    fn set_hscroll(&self, value: f64, cause: &'static str) {
        self.hscroll_intent.set((value, cause));
        self.scrolledwindow.hadjustment().set_value(value);
    }

    // Note that GtkListView is about to move us (scroll_to, or showing a page that was just
    // selected). We cannot work out where it will stop, so store "unknown" and let the log print
    // off=NaN instead of saying nobody asked for the move.
    fn expect_hscroll(&self, cause: &'static str) {
        self.hscroll_intent.set((f64::NAN, cause));
    }

    fn clamp_scroll(&self, value: f64) -> f64 {
        let hadj = self.scrolledwindow.hadjustment();
        let lower = hadj.lower();
        value.clamp(lower, (hadj.upper() - hadj.page_size()).max(lower))
    }

    // Move an older target into the hadjustment's current coordinates. Measuring page widths changes
    // `upper`, and the list view shifts the value to match while the pages stay put on screen. A
    // target left behind names a spot a page or more off: the slide lunges, or settles short.
    fn rebase_target(&self, anim: &ScrollAnim, value: f64) -> f64 {
        let drift = value - anim.last_next;
        if drift.is_finite() && drift != 0.0 {
            return self.clamp_scroll(anim.last_target + drift);
        }
        anim.last_target
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

    fn populate_toc(&self) {
        self.toc_list.remove_all();
        let items = crate::outline::entries(&self.state.uri());
        let mut pages = Vec::with_capacity(items.len());
        for item in &items {
            let label = gtk::Label::new(Some(&item.title));
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.set_margin_start(8 + item.depth as i32 * 16);
            label.set_margin_end(8);
            label.set_margin_top(3);
            label.set_margin_bottom(3);
            if item.page.is_none() {
                label.add_css_class("dim-label");
            }
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&label));
            row.set_activatable(item.page.is_some());
            self.toc_list.append(&row);
            pages.push(item.page);
        }
        self.btn_toc.set_sensitive(!pages.is_empty());
        self.toc_pages.replace(pages);
        self.toc_revealer.set_reveal_child(false);
    }

    #[template_callback]
    fn toc_row_activated(&self, row: &gtk::ListBoxRow) {
        let idx = row.index();
        let page = if idx >= 0 {
            self.toc_pages.borrow().get(idx as usize).copied().flatten()
        } else {
            None
        };
        if let Some(page) = page {
            self.goto_page(page as u32);
        }
        self.toc_revealer.set_reveal_child(false);
    }

    fn setup_toc(&self) {
        // follow focus into the panel while open, back to the reader when it closes
        self.toc_revealer.connect_reveal_child_notify(clone!(
            #[weak(rename_to = imp)]
            self,
            move |rev| {
                if rev.reveals_child() {
                    imp.toc_list.grab_focus();
                } else {
                    imp.scrolledwindow.grab_focus();
                }
            }
        ));

        // Esc or t closes; the panel holds focus while open, so the reader's key handler never sees these.
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _, _| {
                if keyval == Key::Escape || keyval == Key::t {
                    imp.toc_revealer.set_reveal_child(false);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.toc_revealer.add_controller(key);

        // A click on the page area (never on the panel, which is a separate overlay child) dismisses.
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |gesture, _, _, _| {
                if imp.toc_revealer.reveals_child() {
                    imp.toc_revealer.set_reveal_child(false);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        ));
        self.scrolledwindow.add_controller(click);
    }

    fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );

        drop_target.connect_drop(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                    return false;
                };
                let Some(file) = files.files().into_iter().next() else {
                    return false;
                };

                imp.state.load(&file);
                true
            }
        ));

        self.obj().add_controller(drop_target);
    }

    #[template_callback]
    fn open_document(&self) {
        const SUPPORTED_SUFFIXES: &[&str] = &[
            "pdf", "xps", "oxps", "epub", "mobi", "fb2", "cbz", "svg", "txt", "png", "jpg", "jpeg",
            "jp2", "jpx", "gif", "tif", "tiff", "bmp", "pnm", "pgm", "ppm", "pbm", "pam",
        ];
        let supported = gtk::FileFilter::new();
        supported.set_name(Some("Supported documents"));
        for suffix in SUPPORTED_SUFFIXES {
            supported.add_suffix(suffix);
        }
        let all = gtk::FileFilter::new();
        all.set_name(Some("All files"));
        all.add_pattern("*");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&supported);
        filters.append(&all);

        let dialog = gtk::FileDialog::builder()
            .title("Open Document")
            .modal(true)
            .filters(&filters)
            .default_filter(&supported)
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
                    Ok(file) => state.load(&file),
                    Err(err) => {
                        obj.show_error_dialog(&format!("Error opening file: {err}"));
                    }
                },
            ),
        );
    }

    #[template_callback]
    fn on_load_started(&self) {
        self.cancel_scroll_motion();
        self.loading_spinner.start();
        self.loading_overlay.set_visible(true);
    }

    fn hide_loading(&self) {
        self.loading_overlay.set_visible(false);
        self.loading_spinner.stop();
    }

    #[template_callback]
    fn on_load_failed(&self, message: &str) {
        self.hide_loading();
        self.obj()
            .show_error_dialog(&format!("Error loading file: {message}"));
    }

    #[template_callback]
    fn handle_document_load(&self, state: &State) {
        self.hide_loading();

        let n_pages = state.n_pages() as u32;
        if n_pages == 0 {
            return;
        }

        self.populate_toc();

        let model = self.model.clone();
        let selection = self.selection.clone();

        let scroll_to = state.page().min(n_pages - 1);
        let init_load_from = scroll_to.saturating_sub(1);
        let init_load_till = (scroll_to + 10).min(n_pages - 1);

        let vector: Vec<page::PageNumber> = (init_load_from as i32..init_load_till as i32)
            .map(page::PageNumber::new)
            .collect();
        model.extend_from_slice(&vector);
        self.expect_hscroll("restore page");
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
        // a backward move here is the view going back a page
        self.selection.connect_selected_notify(|selection| {
            log::debug!(target: "scrolex::pan", "selected page: {}", selection.selected());
        });

        let hadj = self.scrolledwindow.hadjustment();
        // value-changed: the position moved (any scroll path). Track travel direction for prefetch,
        // refresh the wanted range, sync the selection.
        hadj.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |adj| {
                let prev = imp.last_hadj.replace(adj.value());
                if (adj.value() - prev).abs() > f64::EPSILON {
                    imp.state.set_scroll_forward(adj.value() > prev);
                }
                // `shift` is the one number that shows a real problem: how far the selected page moved
                // on screen. REVERSED means it moved back while the reader was going forward.
                // `off` is how far the position ended up from what `cause` asked for, and UNASKED
                // means nobody asked for that move. UNASKED is usually harmless: when page widths get
                // measured the list view also changes its own coordinates, so the value moves but the
                // page does not - so expect UNASKED together with `shift` near 0. An `off` of NaN
                // means the list view is doing the move and we cannot know where it will stop, so
                // where it stops becomes the new starting point. Reading the page position takes
                // time, so do it only when this log is turned on.
                if log::log_enabled!(target: "scrolex::pan", log::Level::Trace) {
                    let (want, cause) = imp.hscroll_intent.get();
                    let off = adj.value() - want;
                    if want.is_nan() {
                        imp.hscroll_intent.set((adj.value(), cause));
                    }
                    let selected = imp.selection.selected();
                    let page_x = imp.selected_page_left_x();
                    // only means something while the same page stays selected: picking the next page
                    // moves the edge for a good reason
                    let shift = match (imp.seen_page_x.get(), page_x) {
                        (Some((seen, was)), Some(now)) if seen == selected => now - was,
                        _ => 0.0,
                    };
                    if let Some(now) = page_x {
                        imp.seen_page_x.set(Some((selected, now)));
                    }
                    let reversed = if imp.state.scroll_forward() {
                        shift > 8.0
                    } else {
                        shift < -8.0
                    };
                    log::trace!(
                        target: "scrolex::pan",
                        "hadj: v={:9.2} d={:+8.2} off={off:+8.2} upper={:9.0} page={:6.0} max={:9.0} sel={selected} x={:+8.2} shift={shift:+8.2} cause={cause}{}{}",
                        adj.value(),
                        adj.value() - prev,
                        adj.upper(),
                        adj.page_size(),
                        adj.upper() - adj.page_size(),
                        page_x.unwrap_or(f64::NAN),
                        if off.abs() > 2.0 { " UNASKED" } else { "" },
                        if reversed { " REVERSED" } else { "" },
                    );
                }
                imp.update_wanted_render_range();
                imp.schedule_selection_sync();
            }
        ));
        // changed: bounds/page-size moved without the value (e.g. zoom-out at the start), which can
        // remap the visible pages - refresh the range so newly-shown pages aren't dropped.
        hadj.connect_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |adj| {
                // `upper` changes as pages of different widths get measured, and that moves the
                // position.
                log::trace!(
                    target: "scrolex::pan",
                    "hadj bounds: v={:9.2} upper={:9.0} page={:6.0} max={:9.0}",
                    adj.value(),
                    adj.upper(),
                    adj.page_size(),
                    adj.upper() - adj.page_size(),
                );
                imp.update_wanted_render_range();
            }
        ));
    }

    // Track the page range worth a full render as the viewport moves; flung-past pages drop at the
    // queue. Spans the mapped page widgets plus a prefetch margin. A briefly-excluded visible page
    // reschedules via the render-waiter redraw, so the range only needs to be roughly right.
    fn update_wanted_render_range(&self) {
        let mut lo = i32::MAX;
        let mut hi = i32::MIN;
        let mut child = self.listview.first_child();
        while let Some(c) = child {
            if let Some(page) = descendant_page(&c) {
                if page.is_mapped() {
                    let idx = page.index();
                    lo = lo.min(idx);
                    hi = hi.max(idx);
                }
            }
            child = c.next_sibling();
        }
        let range = if lo <= hi {
            let margin = self.state.render_threads() as i32 + 4;
            Some((lo - margin, hi + margin))
        } else {
            None
        };
        crate::page::set_wanted_pages(self.state.render_client_id(), range);
    }

    // Load the render-thread setting into the spin button and pool, and persist any user change.
    fn setup_thread_setting(&self) {
        let max = crate::config::max_render_threads();
        let threads = crate::config::load_config().render_threads;
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
                let mut config = crate::config::load_config();
                config.render_threads = n;
                if let Err(e) = crate::config::save_config(&config) {
                    eprintln!("Error saving config: {e}");
                }
            }
        ));
    }

    fn setup_cache_setting(&self) {
        let mb = crate::config::load_config().render_cache_mb;
        self.spin_cache.set_range(
            crate::config::MIN_RENDER_CACHE_MB as f64,
            crate::config::MAX_RENDER_CACHE_MB as f64,
        );
        self.spin_cache.set_increments(32.0, 64.0);
        self.spin_cache.set_value(mb as f64);
        self.state.set_render_cache_mb(mb);

        self.spin_cache.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |spin| {
                let mb = spin.value() as usize;
                imp.state.set_render_cache_mb(mb);
                let mut config = crate::config::load_config();
                config.render_cache_mb = mb;
                if let Err(e) = crate::config::save_config(&config) {
                    eprintln!("Error saving config: {e}");
                }
            }
        ));
    }

    fn setup_animate_scroll(&self) {
        self.state
            .set_animate_scroll(crate::config::load_config().animate_scroll);

        self.state
            .connect_notify_local(Some("animate-scroll"), |state, _| {
                let mut config = crate::config::load_config();
                config.animate_scroll = state.animate_scroll();
                if let Err(e) = crate::config::save_config(&config) {
                    eprintln!("Error saving config: {e}");
                }
            });
    }

    fn setup_text_selection(&self) {
        // The window is what can reach the page widgets.
        self.state.connect_closure(
            "selection-changed",
            false,
            closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |_: &State, page: i32| imp.redraw_page(page)
            ),
        );

        // Pages clear the selection themselves (see Page::setup_text_selection); this covers the
        // margins and gaps. Primary button only: right-click keeps the selection, middle pans.
        let click = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |_, _, _, _| imp.state.clear_selection()
        ));
        self.scrolledwindow.add_controller(click);
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
                    if let Some(p) = page
                        .compute_point(&*self.scrolledwindow, &gtk::graphene::Point::new(0.0, 0.0))
                    {
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

    // Coalesce a burst of scroll events into a single sync run on idle, after the list view has
    // re-laid-out. Running per-event would read stale page positions mid-burst (e.g. aggressive
    // wheeling) and mis-select.
    fn schedule_selection_sync(&self) {
        // during an animated one-page slide the selection is already set explicitly; skip the
        // viewport sync so the moving pages don't fight it. A kinetic scroll picks no page, so it syncs like
        // any free scroll.
        if (*self.scroll_anim.borrow()).is_some_and(|a| !a.kinetic) {
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
                log::debug!(
                    target: "scrolex::pan",
                    "viewport sync: selection {selected} -> {index}",
                );
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

        // Search keys (Ctrl+F / F3 / Esc) that must work regardless of focus. Capture phase lets F3
        // fire while typing and stops Esc from double-firing the entry's stop-search.
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        key.connect_key_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _keycode, modifier| imp.handle_search_key(keyval, modifier)
        ));
        self.obj().add_controller(key);
    }

    fn handle_search_key(&self, keyval: Key, modifier: ModifierType) -> glib::Propagation {
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

        let (epoch, shared_epoch) = {
            let search = self.state.search();
            let mut search = search.borrow_mut();
            search.query = query.clone();
            search.begin_sweep()
        };

        for page in old_pages {
            self.redraw_page(page);
        }
        self.update_search_status();

        let n_pages = self.state.n_pages();
        if n_pages == 0 || query.is_empty() {
            return;
        }

        let mut rx = crate::search::spawn_search(
            self.state.uri(),
            query,
            n_pages,
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
                        search.results.insert(update.page, update.matches);
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
        self.cancel_scroll_motion();
        // SELECT only (no FOCUS) so typing focus stays in the entry
        self.expect_hscroll("search scroll");
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
    // On a mid-gesture reversal, seed against the new direction so the first reverse step needs
    // nearly a full notch of travel, not the eager `trigger` (stops an accidental back-nudge from
    // firing a page back).
    let base = if delta * prev < 0.0 {
        (notch - 2.0 * trigger).copysign(prev)
    } else {
        accum
    };
    let accum = base + delta;
    if delta > 0.0 && accum >= trigger {
        (accum - notch, 1)
    } else if delta < 0.0 && accum <= -trigger {
        (accum + notch, -1)
    } else {
        (accum, 0)
    }
}

// One frame of a kinetic scroll. The speed decays exponentially and the position moves by the distance
// covered while it does, so each frame is a step relative to wherever the position is now - the list
// view can rewrite the scroll coordinates underneath it without cutting the swipe short. Returns the next
// scroll value, the speed left, and whether it has run out.
fn kinetic_step(value: f64, vel: f64, dt_us: i64, tau_us: f64) -> (f64, f64, bool) {
    if dt_us <= 0 {
        return (value, vel, false);
    }
    let decay = (-(dt_us as f64) / tau_us).exp();
    let step = vel * (tau_us / 1_000_000.0) * (1.0 - decay);
    let vel = vel * decay;
    (value + step, vel, vel.abs() < KINETIC_MIN_VELOCITY)
}

// One frame of the exponential glide toward `target`. Returns the next scroll value and whether the
// glide has settled. `dt_us` is clamped so a stalled-then-resumed frame clock can't spike the gain
// into a sustained oscillation; the glide settles on arrival, on a sub-pixel step, or once
// `elapsed_us` passes the ceiling (bounding a target that relayout keeps nudging).
fn glide_step(
    value: f64,
    target: f64,
    dt_us: i64,
    elapsed_us: i64,
    dir: f64,
    tau_us: f64,
) -> (f64, bool) {
    let k = if dt_us <= 0 {
        0.0
    } else {
        (1.0 - (-(dt_us as f64) / tau_us).exp()).min(SCROLL_ANIM_MAX_GAIN)
    };
    let remaining = target - value;
    let mut step = remaining * k;
    // Floor the approach speed so the exponential tail closes in a few frames rather than crawling
    // toward a target it never reaches (which forced a settle-and-snap a few pixels short). Never
    // overshoot the target.
    if dt_us > 0 && step.abs() < SCROLL_ANIM_MIN_STEP {
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
        accumulate_step, glide_step, kinetic_step, KINETIC_TAU_US, SCROLL_ANIM_MAX_US,
        SCROLL_ANIM_TAU_US, WHEEL_NOTCH, WHEEL_TRIGGER,
    };

    // Drive the glide toward a fixed target at a steady frame rate; return frames until it settles.
    fn glide_frames(mut value: f64, target: f64, dt_us: i64, tau_us: f64) -> usize {
        let dir = (target - value).signum();
        let mut elapsed = 0;
        for frame in 1..100_000 {
            let (next, settled) = glide_step(value, target, dt_us, elapsed, dir, tau_us);
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
        let frames = glide_frames(0.0, 500.0, 16_000, SCROLL_ANIM_TAU_US);
        assert!(frames > 1 && frames < 60, "settled in {frames} frames");
    }

    #[test]
    fn glide_settles_even_with_huge_dt() {
        // a 1s dt from a stalled clock; the gain cap bounds the step and it still converges
        let (next, _) = glide_step(0.0, 500.0, 1_000_000, 0, 1.0, SCROLL_ANIM_TAU_US);
        assert!(next < 500.0, "capped step overshot: {next}");
        glide_frames(0.0, 500.0, 1_000_000, SCROLL_ANIM_TAU_US);
    }

    // Total wall-clock time for the glide to settle, driven at a steady frame rate.
    fn glide_duration_us(target: f64, dt_us: i64) -> i64 {
        glide_frames(0.0, target, dt_us, SCROLL_ANIM_TAU_US) as i64 * dt_us
    }

    #[test]
    fn glide_duration_is_frame_rate_independent() {
        // Gain tracks real elapsed time, so the slide lasts about the same wall-clock at 60fps and
        // at 10fps. Not exact (a 10fps frame quantizes the tail in 100ms chunks), but both land in
        // the same sub-second band rather than diverging.
        let fast = glide_duration_us(500.0, 16_667); // ~60fps
        let slow = glide_duration_us(500.0, 100_000); // ~10fps
        assert!(
            (fast - slow).abs() < 350_000,
            "durations diverged beyond frame quantization: 60fps={fast}us 10fps={slow}us"
        );
        for (label, d) in [("60fps", fast), ("10fps", slow)] {
            assert!(
                (500_000..=1_100_000).contains(&d),
                "{label} settled in {d}us, outside the expected sub-second band"
            );
        }
    }

    // Drive a kinetic scroll to a standstill at a steady frame rate; return the distance and frames.
    fn kinetic_run(vel: f64, dt_us: i64) -> (f64, usize) {
        let (mut value, mut vel) = (0.0, vel);
        for frame in 1..100_000 {
            let (next, left, spent) = kinetic_step(value, vel, dt_us, KINETIC_TAU_US);
            value = next;
            vel = left;
            if spent {
                return (value, frame);
            }
        }
        panic!("kinetic scroll never ran out");
    }

    #[test]
    fn kinetic_scroll_travels_speed_times_tau() {
        // a 3000px/s swipe covers ~900px, less the tail below KINETIC_MIN_VELOCITY
        let ideal = 3000.0 * KINETIC_TAU_US / 1_000_000.0;
        let (distance, frames) = kinetic_run(3000.0, 16_667);
        assert!(
            (distance - ideal).abs() < ideal * 0.05,
            "covered {distance} of {ideal}"
        );
        assert!(frames > 10 && frames < 120, "ran for {frames} frames");
    }

    #[test]
    fn kinetic_scroll_distance_is_frame_rate_independent() {
        // the step is the integral over the frame, so a slow clock covers the same ground
        let (fast, _) = kinetic_run(3000.0, 16_667); // ~60fps
        let (slow, _) = kinetic_run(3000.0, 100_000); // ~10fps
        assert!((fast - slow).abs() < 20.0, "60fps={fast} 10fps={slow}");
    }

    #[test]
    fn kinetic_scroll_is_symmetric() {
        let (fwd, fwd_frames) = kinetic_run(3000.0, 16_667);
        let (back, back_frames) = kinetic_run(-3000.0, 16_667);
        assert!((fwd + back).abs() < 0.01, "forward {fwd} vs back {back}");
        assert_eq!(fwd_frames, back_frames);
    }

    #[test]
    fn glide_force_settles_past_ceiling() {
        // a target far away still settles once the duration ceiling is passed
        let (_, settled) = glide_step(
            0.0,
            100_000.0,
            16_000,
            SCROLL_ANIM_MAX_US + 1,
            1.0,
            SCROLL_ANIM_TAU_US,
        );
        assert!(settled);
    }

    #[test]
    fn glide_never_reverses_on_backward_target_jump() {
        // relayout jumps the target behind us mid-forward-glide: the view must not move back
        let (next, settled) = glide_step(100.0, 50.0, 16_000, 0, 1.0, SCROLL_ANIM_TAU_US);
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
            let (next, settled) =
                glide_step(value, 500.0, 16_000, elapsed, 1.0, SCROLL_ANIM_TAU_US);
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
    fn small_reverse_after_forward_step_does_not_step_back() {
        // Forward 0.8 fires one page and leaves a residual, then an accidental 0.2 back-nudge. The
        // reverse is well under a notch, so it must not fire a page back.
        let steps = run(&[0.2, 0.6, -0.1, -0.1]);
        assert_eq!(steps[0], 1, "forward should fire once");
        assert!(
            steps[1..].iter().all(|&s| s == 0),
            "sub-notch reverse fired a step: {steps:?}"
        );
    }

    #[test]
    fn deliberate_full_notch_reverse_after_forward_step_steps_back() {
        // The same forward step, then a committed full-notch reverse still turns one page back.
        let mut seq = vec![0.2, 0.6];
        seq.extend(NOTCH.iter().map(|d| -d));
        let steps = run(&seq);
        assert_eq!(steps[0], 1);
        assert_eq!(steps[2..].iter().sum::<i32>(), -1, "got {steps:?}");
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
}

#[cfg(test)]
mod widget_tests {
    use super::KINETIC_MIN_VELOCITY;
    use gtk::prelude::*;
    use gtk::subclass::prelude::ObjectSubclassIsExt;

    fn window() -> crate::window::Window {
        gtk::gio::resources_register_include!("scrolex-ui.gresource").expect("ui resources");
        crate::state::State::static_type();
        crate::page::PageNumber::static_type();
        crate::page::Page::static_type();

        gtk::glib::Object::new()
    }

    // GTK's own kinetic scrolling is off on both scrollers, so nothing else coasts for us.
    #[gtk::test]
    fn vertical_flick_coasts_when_the_page_is_taller_than_the_viewport() {
        let window = window();
        let imp = window.imp();
        let vadj = imp.vscrolledwindow.vadjustment();
        vadj.configure(0.0, 0.0, 2000.0, 10.0, 100.0, 500.0);

        imp.handle_decelerate(0.0, -1500.0);

        let pan = imp.vscroll_anim.get().expect("kinetic pan armed");
        assert!((pan.vel + 1500.0).abs() < f64::EPSILON, "vel={}", pan.vel);
    }

    #[gtk::test]
    fn a_page_that_fits_the_viewport_has_nothing_to_coast() {
        let window = window();
        let imp = window.imp();
        let vadj = imp.vscrolledwindow.vadjustment();
        // page_size must not exceed `upper`; GtkAdjustment drops the whole call if it does
        vadj.configure(0.0, 0.0, 400.0, 10.0, 100.0, 400.0);

        imp.handle_decelerate(0.0, -1500.0);

        assert!(imp.vscroll_anim.get().is_none());
    }

    // Zoom relayouts the pages under a coast that has no target to re-aim at.
    #[gtk::test]
    fn a_pinch_stops_both_coasts() {
        let window = window();
        let imp = window.imp();
        let vadj = imp.vscrolledwindow.vadjustment();
        vadj.configure(0.0, 0.0, 2000.0, 10.0, 100.0, 500.0);
        imp.handle_decelerate(-1500.0, -1500.0);
        assert!(imp.scroll_anim.borrow().is_some(), "coasting horizontally");
        assert!(imp.vscroll_anim.get().is_some(), "coasting vertically");

        imp.handle_zoom_begin();

        assert!(imp.scroll_anim.borrow().is_none());
        assert!(imp.vscroll_anim.get().is_none());
    }

    // Unlike a coast, a page slide reads its page's position every frame, which is what keeps the page
    // landing on its anchor when zoom or crop moves it mid-slide.
    #[gtk::test]
    fn a_pinch_leaves_a_page_slide_running() {
        let window = window();
        let imp = window.imp();
        *imp.scroll_anim.borrow_mut() = Some(super::ScrollAnim {
            anchor_x: Some(0.0),
            last_target: 100.0,
            last_frame: -1,
            start_frame: -1,
            dir: 1.0,
            last_next: f64::NAN,
            tau_us: super::SCROLL_ANIM_TAU_US,
            kinetic: false,
            vel: 0.0,
        });

        imp.handle_zoom_begin();

        assert!(imp.scroll_anim.borrow().is_some());
    }

    // A slide already running: we last wrote `value`, and aim at `target` a page ahead.
    fn slide(imp: &super::Window, value: f64, target: f64) {
        let hadj = imp.scrolledwindow.hadjustment();
        hadj.configure(value, 0.0, 100_000.0, 10.0, 100.0, 1_000.0);
        *imp.scroll_anim.borrow_mut() = Some(super::ScrollAnim {
            anchor_x: None,
            last_target: target,
            last_frame: 0,
            start_frame: 0,
            dir: (target - value).signum(),
            last_next: value,
            tau_us: super::SCROLL_ANIM_TAU_US,
            kinetic: false,
            vel: 0.0,
        });
    }

    // The pages don't move on screen when the list view shifts the value, so the target must shift too.
    #[gtk::test]
    fn a_coordinate_rewrite_carries_the_slide_target_with_it() {
        let window = window();
        let imp = window.imp();
        slide(imp, 50_000.0, 50_677.0);
        let anim = imp.scroll_anim.borrow().expect("slide armed");

        assert!((imp.rebase_target(&anim, 50_000.0) - 50_677.0).abs() < f64::EPSILON);
        // the list view moved us back 4631px; so does the target
        assert!((imp.rebase_target(&anim, 45_369.0) - 46_046.0).abs() < f64::EPSILON);
    }

    // Wheeling mid-slide retargets the running slide, and starts from where it was already heading.
    #[gtk::test]
    fn a_retarget_advances_from_the_rebased_target() {
        let window = window();
        let imp = window.imp();
        imp.state.set_animate_scroll(true);
        slide(imp, 50_000.0, 50_677.0);

        imp.scrolledwindow.hadjustment().set_value(45_369.0); // the list view rewrites coordinates
        imp.animate_scroll(None, 677.0);

        let anim = imp.scroll_anim.borrow().expect("slide still running");
        assert!(
            (anim.last_target - 46_723.0).abs() < f64::EPSILON,
            "target={} - want the rebased 46046 plus a page",
            anim.last_target,
        );
    }

    // A slow lift-off means the fingers stopped before they left, not a fling.
    #[gtk::test]
    fn a_slow_vertical_lift_off_does_not_coast() {
        let window = window();
        let imp = window.imp();
        let vadj = imp.vscrolledwindow.vadjustment();
        vadj.configure(0.0, 0.0, 2000.0, 10.0, 100.0, 500.0);

        imp.handle_decelerate(0.0, -(KINETIC_MIN_VELOCITY - 1.0));

        assert!(imp.vscroll_anim.get().is_none());
    }

    // Both scrollers keep scrolling on their own after a touchpad flick, each from a capture-phase
    // kinetic controller. We set the position ourselves, so we must get the event first: capture
    // phase, on the outer scroller, because capture runs outside-in (issue #41).
    // gtk::test, not test: it runs the body on the thread GTK is initialized on.
    #[gtk::test]
    fn scroll_controller_runs_before_gtk_coasts() {
        let window = window();
        let imp = window.imp();
        let ours = imp.pan_scroll.get();

        assert_eq!(ours.propagation_phase(), gtk::PropagationPhase::Capture);
        // outside the inner scroller: capture reaches ours first
        assert!(imp.scrolledwindow.is_ancestor(&*imp.vscrolledwindow));

        let controllers = imp.vscrolledwindow.observe_controllers();
        let mut ours_at = None;
        let mut kinetic_at = None;
        for i in 0..controllers.n_items() {
            let item = controllers.item(i).unwrap();
            if item == ours {
                ours_at = Some(i);
                continue;
            }
            let Ok(scroll) = item.downcast::<gtk::EventControllerScroll>() else {
                continue;
            };
            if scroll.propagation_phase() == gtk::PropagationPhase::Capture
                && scroll
                    .flags()
                    .contains(gtk::EventControllerScrollFlags::KINETIC)
            {
                kinetic_at.get_or_insert(i);
            }
        }

        let ours_at = ours_at.expect("our controller on the outer scroller");
        let kinetic_at = kinetic_at.expect("GtkScrolledWindow's capture-phase kinetic controller");
        assert!(
            ours_at < kinetic_at,
            "ours is at {ours_at}, GTK's at {kinetic_at}, so GTK's runs first"
        );
    }
}
