use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use gtk::gdk::{Key, ModifierType};
use gtk::glib;
use gtk::glib::clone;
use gtk::prelude::*;

use crate::document_pane::DocumentPane;
use crate::links::{DocumentLocation, LinkHover};
use crate::page::{Page, Rectangle};
use crate::viewport::Viewport;

pub const OPEN_DELAY: Duration = Duration::from_millis(300);
pub const CLOSE_DELAY: Duration = Duration::from_millis(300);
const MARGIN: f64 = 12.0;
// Minimum readable preview size.
const MIN_SIZE: f64 = 120.0;
// Matches the .link-preview border in style.css.
const BORDER: f64 = 1.0;
const ACTION: &str = "show-link-previews";

static ENABLED: AtomicBool = AtomicBool::new(true);
static HEIGHT_PERCENT: AtomicU32 =
    AtomicU32::new(crate::config::DEFAULT_LINK_PREVIEW_HEIGHT_PERCENT);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

// The popup page height limit, as a percentage of the target page height.
pub fn height_percent() -> u32 {
    HEIGHT_PERCENT.load(Ordering::Relaxed)
}

pub fn set_height_percent(percent: u32) {
    HEIGHT_PERCENT.store(percent, Ordering::Relaxed);
}

// The action state keeps the menu toggle in sync with the preference.
pub fn preference_action() -> gtk::gio::SimpleAction {
    let action = gtk::gio::SimpleAction::new_stateful(ACTION, None, &enabled().to_variant());
    action.connect_activate(|action, _| {
        let current = action.state().and_then(|v| v.get::<bool>()).unwrap_or(true);
        action.change_state(&(!current).to_variant());
    });
    action.connect_change_state(|action, value| {
        let Some(enabled) = value.and_then(glib::Variant::get::<bool>) else {
            return;
        };
        action.set_state(&enabled.to_variant());
        apply_preference(enabled);
    });
    action
}

// Save the preference. If disabled, dismiss previews across all open documents.
pub fn apply_preference(enabled: bool) {
    set_enabled(enabled);

    let mut settings = crate::config::load_config();
    settings.show_link_previews = enabled;
    if let Err(err) = crate::config::save_config(&settings) {
        log::warn!("could not save the link preview setting: {err}");
    }

    if !enabled {
        for window in gtk::Window::list_toplevels() {
            if let Ok(window) = window.downcast::<crate::window::Window>() {
                for document in window.documents() {
                    document.dismiss_link_preview();
                }
            }
        }
    }
}

// Use the app action to keep the menu toggle in sync.
fn disable_from_popup() {
    let action = gtk::gio::Application::default().and_then(|app| app.lookup_action(ACTION));
    match action {
        Some(action) => action.change_state(&false.to_variant()),
        None => apply_preference(false),
    }
}

// Modifier keys alone must not dismiss the preview before Ctrl+wheel.
fn is_modifier(keyval: Key) -> bool {
    matches!(
        keyval,
        Key::Control_L
            | Key::Control_R
            | Key::Shift_L
            | Key::Shift_R
            | Key::Alt_L
            | Key::Alt_R
            | Key::Meta_L
            | Key::Meta_R
            | Key::Super_L
            | Key::Super_R
            | Key::Hyper_L
            | Key::Hyper_R
            | Key::ISO_Level3_Shift
            | Key::Caps_Lock
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    fn right(&self) -> f64 {
        self.x + self.width
    }

    fn bottom(&self) -> f64 {
        self.y + self.height
    }

    fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn area(&self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Right,
    Left,
    Below,
    Above,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placement {
    side: Side,
    region: Bounds,
    link: Bounds,
    pub width: f64,
    pub max_page_height: f64,
}

// Keep the popup inside the area without overlap with the link.
// Return None if no side has room for a readable popup.
pub(crate) fn place(link: Bounds, area: Bounds, preferred: (f64, f64)) -> Option<Placement> {
    let inner = Bounds {
        x: area.x + MARGIN,
        y: area.y + MARGIN,
        width: area.width - 2.0 * MARGIN,
        height: area.height - 2.0 * MARGIN,
    };
    let region = |side| match side {
        Side::Right => {
            let x = link.right() + MARGIN;
            Bounds {
                x,
                width: inner.right() - x,
                ..inner
            }
        }
        Side::Left => Bounds {
            width: link.x - MARGIN - inner.x,
            ..inner
        },
        Side::Below => {
            let y = link.bottom() + MARGIN;
            Bounds {
                y,
                height: inner.bottom() - y,
                ..inner
            }
        }
        Side::Above => Bounds {
            height: link.y - MARGIN - inner.y,
            ..inner
        },
    };
    let (preferred_width, preferred_height) = preferred;
    let readable = |side: &Side| {
        let r = region(*side);
        r.width >= MIN_SIZE && r.height >= MIN_SIZE
    };

    let fits_vertically = [Side::Below, Side::Above].into_iter().find(|side| {
        let r = region(*side);
        readable(side) && r.width >= preferred_width && r.height >= preferred_height
    });
    let beside = [Side::Right, Side::Left]
        .into_iter()
        .find(|side| readable(side) && region(*side).width >= preferred_width);
    let vertical = || {
        [Side::Below, Side::Above]
            .into_iter()
            .filter(readable)
            .max_by(|a, b| region(*a).height.total_cmp(&region(*b).height))
    };
    let largest = || {
        [Side::Right, Side::Left, Side::Below, Side::Above]
            .into_iter()
            .filter(readable)
            .max_by(|a, b| region(*a).area().total_cmp(&region(*b).area()))
    };
    let side = fits_vertically
        .or(beside)
        .or_else(|| vertical().filter(|side| region(*side).width >= preferred_width))
        .or_else(largest)?;
    let region = region(side);

    Some(Placement {
        side,
        region,
        link,
        width: preferred_width.min(region.width),
        max_page_height: preferred_height.min(region.height),
    })
}

impl Placement {
    // Center the popup along the link, within the available region.
    pub(crate) fn bounds(&self, height: f64) -> Bounds {
        let height = height.min(self.region.height);
        let r = self.region;
        let (cx, cy) = self.link.center();
        let along_y = (cy - height / 2.0).clamp(r.y, r.bottom() - height);
        let along_x = (cx - self.width / 2.0).clamp(r.x, r.right() - self.width);
        let (x, y) = match self.side {
            Side::Right => (r.x, along_y),
            Side::Left => (r.right() - self.width, along_y),
            Side::Below => (along_x, r.y),
            Side::Above => (along_x, r.bottom() - height),
        };
        Bounds {
            x,
            y,
            width: self.width,
            height,
        }
    }
}

#[derive(Clone)]
struct Hover {
    pane: DocumentPane,
    page: Page,
    location: DocumentLocation,
    rect: Rectangle,
}

impl Hover {
    fn same_link(&self, other: &Hover) -> bool {
        self.page == other.page
            && self.page.index() == other.page.index()
            && self.rect == other.rect
            && self.location == other.location
    }
}

struct Popup {
    root: gtk::Box,
    scroller: gtk::ScrolledWindow,
    checkbox: gtk::CheckButton,
    viewport: Viewport,
    link: Hover,
    placement: Placement,
    area: (i32, i32),
    // Restore this focus when keyboard access to the popup ends.
    previous_focus: RefCell<Option<glib::WeakRef<gtk::Widget>>>,
}

struct State {
    overlay: glib::WeakRef<gtk::Overlay>,
    open_delay: Cell<Duration>,
    close_delay: Cell<Duration>,
    // Dismissal invalidates callbacks that hold a previous generation.
    generation: Cell<u64>,
    hover: RefCell<Option<Hover>>,
    pending: RefCell<Option<glib::SourceId>>,
    closing: RefCell<Option<glib::SourceId>>,
    popup: RefCell<Option<Rc<Popup>>>,
    pointer_in_popup: Cell<bool>,
}

// Each document view owns one preview outside its pane scrollers.
// This keeps popup input out of the pane capture handlers.
#[derive(Clone)]
pub(crate) struct LinkPreview {
    state: Rc<State>,
}

impl LinkPreview {
    pub(crate) fn new(overlay: &gtk::Overlay) -> Self {
        let state = Rc::new(State {
            overlay: overlay.downgrade(),
            open_delay: Cell::new(OPEN_DELAY),
            close_delay: Cell::new(CLOSE_DELAY),
            generation: Cell::new(0),
            hover: RefCell::new(None),
            pending: RefCell::new(None),
            closing: RefCell::new(None),
            popup: RefCell::new(None),
            pointer_in_popup: Cell::new(false),
        });
        let weak = Rc::downgrade(&state);
        overlay.connect_get_child_position(move |overlay, widget| {
            let preview = Self::upgrade(&weak)?;
            preview.child_position(overlay, widget)
        });
        Self { state }
    }

    fn upgrade(weak: &Weak<State>) -> Option<Self> {
        weak.upgrade().map(|state| Self { state })
    }

    fn weak(&self) -> Weak<State> {
        Rc::downgrade(&self.state)
    }

    #[cfg(test)]
    pub(crate) fn set_delays(&self, open: Duration, close: Duration) {
        self.state.open_delay.set(open);
        self.state.close_delay.set(close);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.state.popup.borrow().is_some()
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.state.pending.borrow().is_some()
    }

    #[cfg(test)]
    pub(crate) fn popup_widget(&self) -> Option<gtk::Box> {
        self.popup().map(|popup| popup.root.clone())
    }

    #[cfg(test)]
    pub(crate) fn scroller(&self) -> Option<gtk::ScrolledWindow> {
        self.popup().map(|popup| popup.scroller.clone())
    }

    #[cfg(test)]
    pub(crate) fn checkbox(&self) -> Option<gtk::CheckButton> {
        self.popup().map(|popup| popup.checkbox.clone())
    }

    #[cfg(test)]
    pub(crate) fn viewport(&self) -> Option<Viewport> {
        self.popup().map(|popup| popup.viewport.clone())
    }

    #[cfg(test)]
    pub(crate) fn open_location(&self) -> Option<DocumentLocation> {
        self.popup().map(|popup| popup.link.location)
    }

    fn popup(&self) -> Option<Rc<Popup>> {
        self.state.popup.borrow().clone()
    }

    pub(crate) fn hover(&self, pane: &DocumentPane, page: &Page, hover: LinkHover) {
        let hover = hover.link.map(|(location, rect)| Hover {
            pane: pane.clone(),
            page: page.clone(),
            location,
            rect,
        });
        self.cancel_pending();
        let over_popup_link = self
            .popup()
            .zip(hover.as_ref())
            .is_some_and(|(popup, hover)| popup.link.same_link(hover));
        self.state.hover.replace(hover.clone());

        if over_popup_link {
            self.cancel_closing();
            return;
        }
        if let Some(hover) = hover {
            if enabled() && !hover.pane.is_moving() {
                self.schedule_open();
            }
        }
        if self.is_open() && !self.state.pointer_in_popup.get() {
            self.schedule_close();
        }
    }

    fn schedule_open(&self) {
        let generation = self.state.generation.get();
        let weak = self.weak();
        let source = glib::timeout_add_local_once(self.state.open_delay.get(), move || {
            let Some(preview) = Self::upgrade(&weak) else {
                return;
            };
            preview.state.pending.take();
            if preview.state.generation.get() != generation || !enabled() {
                return;
            }
            let hover = preview.state.hover.borrow().clone();
            let Some(hover) = hover else {
                return;
            };
            if hover.pane.is_moving() {
                return;
            }
            preview.open(&hover);
        });
        self.state.pending.replace(Some(source));
    }

    fn schedule_close(&self) {
        if self.state.closing.borrow().is_some() {
            return;
        }
        let generation = self.state.generation.get();
        let weak = self.weak();
        let source = glib::timeout_add_local_once(self.state.close_delay.get(), move || {
            let Some(preview) = Self::upgrade(&weak) else {
                return;
            };
            preview.state.closing.take();
            if preview.state.generation.get() != generation
                || preview.state.pointer_in_popup.get()
                || preview.focus_inside()
            {
                return;
            }
            let over_link = preview
                .popup()
                .zip(preview.state.hover.borrow().as_ref())
                .is_some_and(|(popup, hover)| popup.link.same_link(hover));
            if !over_link {
                preview.close_popup();
            }
        });
        self.state.closing.replace(Some(source));
    }

    fn cancel_pending(&self) {
        if let Some(source) = self.state.pending.take() {
            source.remove();
        }
    }

    fn cancel_closing(&self) {
        if let Some(source) = self.state.closing.take() {
            source.remove();
        }
    }

    pub(crate) fn dismiss(&self) {
        self.state
            .generation
            .set(self.state.generation.get().wrapping_add(1));
        self.state.hover.replace(None);
        self.cancel_pending();
        self.close_popup();
    }

    // Dismiss only when this pane owns the pending or open preview.
    pub(crate) fn dismiss_for(&self, pane: &DocumentPane) {
        let hovered = self
            .state
            .hover
            .borrow()
            .as_ref()
            .is_some_and(|hover| hover.pane == *pane);
        let shown = self.popup().is_some_and(|popup| popup.link.pane == *pane);
        if hovered || shown {
            self.dismiss();
        }
    }

    fn close_popup(&self) {
        self.cancel_closing();
        self.state.pointer_in_popup.set(false);
        let Some(popup) = self.state.popup.take() else {
            return;
        };
        let restore = self.focus_inside_popup(&popup);
        // Stop kinetic scroll before the popup leaves the widget tree.
        popup.scroller.set_kinetic_scrolling(false);
        popup.viewport.release_renders();
        if let Some(overlay) = self.state.overlay.upgrade() {
            overlay.remove_overlay(&popup.root);
        }
        if restore {
            let previous = popup
                .previous_focus
                .take()
                .and_then(|focus| focus.upgrade());
            match previous {
                Some(widget) => {
                    widget.grab_focus();
                }
                None => popup.link.pane.focus_reader(),
            }
        }
    }

    fn focus_inside(&self) -> bool {
        self.popup()
            .is_some_and(|popup| self.focus_inside_popup(&popup))
    }

    fn focus_inside_popup(&self, popup: &Popup) -> bool {
        popup
            .root
            .root()
            .and_then(|root| root.focus())
            .is_some_and(|focus| focus.is_ancestor(&popup.root) || focus == popup.root)
    }

    // Some skips the window shortcuts. None lets the window handle the key.
    pub(crate) fn handle_key(
        &self,
        keyval: Key,
        modifiers: ModifierType,
    ) -> Option<glib::Propagation> {
        let popup = self.popup();
        if popup.is_none() && !self.is_pending() || is_modifier(keyval) {
            return None;
        }
        let plain = modifiers.is_empty();
        if let Some(popup) = popup {
            if self.focus_inside_popup(&popup) {
                if plain && keyval == Key::Escape {
                    self.dismiss();
                    return Some(glib::Propagation::Stop);
                }
                // Send plain Space and Enter to the checkbox. Let other keys reach window shortcuts.
                let activates = matches!(keyval, Key::space | Key::Return | Key::KP_Enter);
                return (plain && activates).then_some(glib::Propagation::Proceed);
            }
            if plain && keyval == Key::Tab {
                let previous = popup.root.root().and_then(|root| root.focus());
                popup
                    .previous_focus
                    .replace(previous.map(|widget| widget.downgrade()));
                popup.checkbox.grab_focus();
                self.cancel_closing();
                return Some(glib::Propagation::Stop);
            }
            self.dismiss();
            return (plain && keyval == Key::Escape).then_some(glib::Propagation::Stop);
        }
        // Let the key reach the document when the preview is not visible.
        self.dismiss();
        None
    }

    fn open(&self, hover: &Hover) {
        self.close_popup();
        let Some(overlay) = self.state.overlay.upgrade() else {
            return;
        };
        let source = &hover.page;
        let link = Self::overlay_bounds(source, &overlay, hover.rect);
        let Some(link) = link else {
            return;
        };
        let area = Bounds {
            x: 0.0,
            y: 0.0,
            width: f64::from(overlay.width()),
            height: f64::from(overlay.height()),
        };
        let document = source.document();
        let crop = source.crop();
        let Some(bbox) = document.page_bounds(hover.location.page, crop) else {
            return;
        };
        let (page_width, page_height) = bbox.size();

        // Use the page size request because the list row can exceed the page height.
        let (width, _) = source.size_request();
        let height = page_height * source.zoom() * f64::from(height_percent()) / 100.0;
        let preferred = (f64::from(width), height);
        let Some(mut placement) = place(link, area, preferred) else {
            return;
        };

        let viewport = Viewport::new(&document);
        viewport.set_crop(crop);
        let zoom = source
            .zoom()
            .min((placement.width - 2.0 * BORDER) / page_width);
        viewport.fit_zoom_to(zoom);
        let height_limit = page_height * viewport.zoom() * f64::from(height_percent()) / 100.0;
        placement.max_page_height = placement.max_page_height.min(height_limit);

        let target = Page::new_link_target(&viewport, hover.location.page);
        target.set_halign(gtk::Align::Start);
        let (root, scroller, checkbox) = self.build_popup(&target);
        let popup = Rc::new(Popup {
            root: root.clone(),
            scroller: scroller.clone(),
            checkbox,
            viewport: viewport.clone(),
            link: hover.clone(),
            placement,
            area: (overlay.width(), overlay.height()),
            previous_focus: RefCell::new(None),
        });
        self.state.popup.replace(Some(popup));
        overlay.add_overlay(&root);

        let vadj = scroller.vadjustment();
        vadj.connect_value_changed(clone!(
            #[weak]
            target,
            move |_| target.queue_draw()
        ));
        let start = hover
            .location
            .y
            .map_or(0.0, |y| (y - bbox.y1).max(0.0) * viewport.zoom());
        Self::scroll_once_allocated(&vadj, start);
    }

    // The change signal runs during allocation, after GTK positions the child.
    // Defer the scroll until idle so GTK can apply the destination offset.
    // The destination shows at 1/3 of the popup height.
    fn scroll_once_allocated(vadj: &gtk::Adjustment, start: f64) {
        if start <= 0.0 {
            return;
        }
        let handler = Rc::new(RefCell::new(None::<glib::SignalHandlerId>));
        let id = vadj.connect_changed(clone!(
            #[strong]
            handler,
            move |vadj| {
                let range = vadj.upper() - vadj.page_size();
                if vadj.page_size() <= 0.0 || range <= 0.0 {
                    return;
                }
                if let Some(id) = handler.take() {
                    vadj.disconnect(id);
                }
                glib::idle_add_local_once(clone!(
                    #[weak]
                    vadj,
                    move || {
                        let value = start - vadj.page_size() / 3.0;
                        vadj.set_value(value.clamp(0.0, vadj.upper() - vadj.page_size()));
                    }
                ));
            }
        ));
        handler.replace(Some(id));
    }

    fn overlay_bounds(page: &Page, overlay: &gtk::Overlay, rect: Rectangle) -> Option<Bounds> {
        let point = |x: f64, y: f64| {
            page.compute_point(overlay, &gtk::graphene::Point::new(x as f32, y as f32))
                .map(|p| (f64::from(p.x()), f64::from(p.y())))
        };
        let (x1, y1) = point(rect.x1, rect.y1)?;
        let (x2, y2) = point(rect.x2, rect.y2)?;
        Some(Bounds {
            x: x1,
            y: y1,
            width: x2 - x1,
            height: y2 - y1,
        })
    }

    fn build_popup(&self, target: &Page) -> (gtk::Box, gtk::ScrolledWindow, gtk::CheckButton) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .focusable(false)
            .overflow(gtk::Overflow::Hidden)
            .css_classes(["link-preview"])
            .build();

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .kinetic_scrolling(true)
            .propagate_natural_height(true)
            .vexpand(true)
            .focusable(false)
            .child(target)
            .build();
        root.append(&scroller);

        let checkbox = gtk::CheckButton::builder()
            .label("Do not show link previews")
            .build();
        checkbox.connect_toggled(|checkbox| {
            if checkbox.is_active() {
                disable_from_popup();
            }
        });
        checkbox.add_css_class("link-preview-footer");
        root.append(&checkbox);

        self.add_input_controllers(&root);
        (root, scroller, checkbox)
    }

    fn add_input_controllers(&self, root: &gtk::Box) {
        // Block Ctrl+scroll to preserve the popup zoom.
        let zoom_scroll =
            gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        zoom_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        zoom_scroll.connect_scroll(|scroll, _, _| {
            if scroll
                .current_event_state()
                .contains(ModifierType::CONTROL_MASK)
            {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        root.add_controller(zoom_scroll);

        // Keep scroll events inside the popup, even at its scroll limits.
        let rest = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        rest.connect_scroll(|_, _, _| glib::Propagation::Stop);
        root.add_controller(rest);

        let pinch = gtk::GestureZoom::new();
        pinch.connect_begin(|gesture, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        root.add_controller(pinch);

        let motion = gtk::EventControllerMotion::new();
        let weak = self.weak();
        motion.connect_enter(clone!(
            #[strong]
            weak,
            move |_, _, _| {
                if let Some(preview) = Self::upgrade(&weak) {
                    preview.state.pointer_in_popup.set(true);
                    preview.cancel_closing();
                }
            }
        ));
        motion.connect_leave(clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(preview) = Self::upgrade(&weak) {
                    preview.state.pointer_in_popup.set(false);
                    preview.schedule_close();
                }
            }
        ));
        root.add_controller(motion);

        // If focus leaves after the pointer, restart the close timer.
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            if let Some(preview) = Self::upgrade(&weak) {
                if !preview.state.pointer_in_popup.get() {
                    preview.schedule_close();
                }
            }
        });
        root.add_controller(focus);
    }

    fn child_position(
        &self,
        overlay: &gtk::Overlay,
        widget: &gtk::Widget,
    ) -> Option<gtk::gdk::Rectangle> {
        let popup = self.popup()?;
        if widget != popup.root.upcast_ref::<gtk::Widget>() {
            return None;
        }
        if (overlay.width(), overlay.height()) != popup.area {
            // Defer dismissal because GTK must not remove children during allocation.
            let weak = self.weak();
            glib::idle_add_local_once(move || {
                if let Some(preview) = Self::upgrade(&weak) {
                    preview.dismiss();
                }
            });
        }
        let width = popup.placement.width.round() as i32;
        let (_, natural, _, _) = popup.root.measure(gtk::Orientation::Vertical, width);
        let (_, page, _, _) = popup.scroller.measure(gtk::Orientation::Vertical, width);
        let footer = f64::from(natural - page);
        let height = f64::from(natural).min(popup.placement.max_page_height + footer);
        let bounds = popup.placement.bounds(height);
        Some(gtk::gdk::Rectangle::new(
            bounds.x.round() as i32,
            bounds.y.round() as i32,
            width,
            bounds.height.round() as i32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Bounds = Bounds {
        x: 0.0,
        y: 0.0,
        width: 1600.0,
        height: 900.0,
    };

    fn link(x: f64, y: f64) -> Bounds {
        Bounds {
            x,
            y,
            width: 40.0,
            height: 12.0,
        }
    }

    fn inflate(b: Bounds, by: f64) -> Bounds {
        Bounds {
            x: b.x - by,
            y: b.y - by,
            width: b.width + 2.0 * by,
            height: b.height + 2.0 * by,
        }
    }

    fn intersects(a: Bounds, b: Bounds) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    fn assert_valid(popup: Bounds, link: Bounds, area: Bounds) {
        assert!(
            !intersects(popup, inflate(link, MARGIN - 0.001)),
            "{popup:?} covers {link:?}"
        );
        assert!(
            popup.x >= area.x + MARGIN && popup.y >= area.y + MARGIN,
            "{popup:?}"
        );
        assert!(popup.right() <= area.right() - MARGIN, "{popup:?}");
        assert!(popup.bottom() <= area.bottom() - MARGIN, "{popup:?}");
    }

    #[test]
    fn prefers_the_right_side() {
        let l = link(300.0, 400.0);
        let p = place(l, AREA, (500.0, 700.0)).unwrap();
        assert_eq!(p.side, Side::Right);
        let b = p.bounds(700.0);
        assert_eq!((b.width, b.height), (500.0, 700.0));
        assert_valid(b, l, AREA);
    }

    #[test]
    fn uses_the_left_side_when_the_right_is_too_narrow() {
        let l = link(1300.0, 400.0);
        let p = place(l, AREA, (500.0, 700.0)).unwrap();
        assert_eq!(p.side, Side::Left);
        assert_valid(p.bounds(700.0), l, AREA);
    }

    #[test]
    fn goes_below_or_above_when_no_side_fits_the_width() {
        let area = Bounds {
            width: 700.0,
            ..AREA
        };
        let high = link(330.0, 100.0);
        let p = place(high, area, (600.0, 500.0)).unwrap();
        assert_eq!(p.side, Side::Below);
        assert_valid(p.bounds(500.0), high, area);

        let low = link(330.0, 800.0);
        let p = place(low, area, (600.0, 500.0)).unwrap();
        assert_eq!(p.side, Side::Above);
        assert_valid(p.bounds(500.0), low, area);
    }

    #[test]
    fn a_short_popup_prefers_below_then_above() {
        let l = link(300.0, 400.0);
        let p = place(l, AREA, (500.0, 300.0)).unwrap();
        assert_eq!(p.side, Side::Below);
        assert_valid(p.bounds(300.0), l, AREA);

        let low = link(300.0, 700.0);
        let p = place(low, AREA, (500.0, 300.0)).unwrap();
        assert_eq!(p.side, Side::Above);
        assert_valid(p.bounds(300.0), low, AREA);
    }

    #[test]
    fn a_popup_too_tall_for_above_and_below_goes_beside() {
        let l = link(300.0, 400.0);
        let p = place(l, AREA, (500.0, 480.0)).unwrap();
        assert_eq!(p.side, Side::Right);
        assert_valid(p.bounds(480.0), l, AREA);
    }

    #[test]
    fn shrinks_to_the_largest_side_when_nothing_fits() {
        let area = Bounds {
            x: 0.0,
            y: 0.0,
            width: 500.0,
            height: 400.0,
        };
        let l = link(100.0, 180.0);
        let p = place(l, area, (900.0, 1200.0)).unwrap();
        assert_eq!(p.side, Side::Right);
        let b = p.bounds(1200.0);
        assert!(b.width < 900.0 && b.height < 1200.0);
        assert_valid(b, l, area);
    }

    #[test]
    fn a_short_popup_centers_on_the_link_beside_it() {
        let l = link(300.0, 400.0);
        let b = place(l, AREA, (500.0, 700.0)).unwrap().bounds(200.0);
        assert_eq!(b.height, 200.0);
        assert!((b.center().1 - l.center().1).abs() < 0.001);
        assert_valid(b, l, AREA);
    }

    #[test]
    fn no_room_means_no_popup() {
        let area = Bounds {
            x: 0.0,
            y: 0.0,
            width: 150.0,
            height: 150.0,
        };
        assert!(place(link(55.0, 70.0), area, (500.0, 700.0)).is_none());
    }
}

#[cfg(test)]
mod widget_tests {
    use std::time::Instant;

    use gtk::subclass::prelude::ObjectSubclassIsExt;

    use super::*;
    use crate::test_support::{fixture, wait_until, TestWindow};

    // Link centers on page 0 of links.pdf, in page points.
    const TO_LONG_PAGE: (f64, f64) = (100.0, 77.0); // page 2, XYZ y 20000
    const TO_SECOND_FIT: (f64, f64) = (100.0, 177.0); // page 1, Fit
    const TO_SECOND_TOP: (f64, f64) = (350.0, 77.0); // page 1, XYZ y 0
    const LONG_PAGE_DEST_Y: f64 = 20000.0;
    // Short delays limit test time because wait_until uses real time.
    const OPEN: Duration = Duration::from_millis(30);
    const CLOSE: Duration = Duration::from_millis(20);

    fn links_window() -> TestWindow {
        let window = crate::test_support::window();
        window.present();
        window.load(&fixture("links.pdf"));
        wait_until(|| window.pane().imp().mapped_page(0).is_some());
        wait_until(|| window.pane().imp().selection.n_items() == 3);
        preview(&window).set_delays(OPEN, CLOSE);
        window
    }

    fn preview(window: &TestWindow) -> LinkPreview {
        window.imp().link_preview().clone()
    }

    fn source_page(window: &TestWindow) -> Page {
        window.pane().imp().mapped_page(0).expect("page 0 is shown")
    }

    fn controller<T: IsA<gtk::EventController> + IsA<glib::Object>>(
        widget: &impl IsA<gtk::Widget>,
    ) -> T {
        let controllers = widget.observe_controllers();
        (0..controllers.n_items())
            .filter_map(|index| controllers.item(index))
            .find_map(|controller| controller.downcast::<T>().ok())
            .expect("the controller")
    }

    fn hover(window: &TestWindow, (x, y): (f64, f64)) {
        let page = source_page(window);
        let document = page.document();
        document.request_links(0);
        wait_until(|| document.imp().links.borrow().get_link(0, x, y).is_some());
        let bbox = document.page_bounds(0, page.crop()).expect("page 0 bounds");
        let zoom = page.zoom();
        controller::<gtk::EventControllerMotion>(&page).emit_by_name::<()>(
            "motion",
            &[&((x - bbox.x1) * zoom), &((y - bbox.y1) * zoom)],
        );
    }

    fn leave_link(window: &TestWindow) {
        controller::<gtk::EventControllerMotion>(&source_page(window))
            .emit_by_name::<()>("leave", &[]);
    }

    fn open(window: &TestWindow, link: (f64, f64)) -> LinkPreview {
        hover(window, link);
        let preview = preview(window);
        wait_until(|| preview.is_open());
        preview
    }

    fn run_for(duration: Duration) {
        let context = glib::MainContext::default();
        let end = Instant::now() + duration;
        while Instant::now() < end {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn key(window: &TestWindow, keyval: Key, modifiers: ModifierType) -> glib::Propagation {
        window.header().handle_window_key(keyval, modifiers)
    }

    fn reader_key(window: &TestWindow, keyval: Key) {
        window.handle_reader_key(
            keyval,
            ModifierType::empty(),
            crate::document_view::ReaderKeyContext::Document,
        );
    }

    fn focus(window: &TestWindow) -> Option<gtk::Widget> {
        gtk::prelude::GtkWindowExt::focus(&*gtk::subclass::prelude::ObjectSubclassExt::obj(
            window.header(),
        ))
    }

    fn target_page(preview: &LinkPreview) -> Page {
        preview
            .scroller()
            .and_then(|scroller| scroller.child())
            .and_then(|viewport| viewport.first_child())
            .and_downcast::<Page>()
            .expect("the target page")
    }

    #[gtk::test]
    fn opens_after_the_delay() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        assert!(preview.is_pending());
        assert!(!preview.is_open());
        wait_until(|| preview.is_open());

        assert_eq!(preview.open_location().map(|l| l.page), Some(2));
        window.close();
    }

    #[gtk::test]
    fn leaving_before_the_delay_opens_nothing() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        leave_link(&window);
        run_for(OPEN * 4);

        assert!(!preview.is_pending());
        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn a_new_target_restarts_the_delay() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        hover(&window, TO_SECOND_TOP);
        wait_until(|| preview.is_open());

        assert_eq!(preview.open_location().map(|l| l.page), Some(1));
        window.close();
    }

    #[gtk::test]
    fn leaving_closes_after_the_delay_and_returning_keeps_it_open() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        preview.set_delays(OPEN, Duration::from_millis(150));

        leave_link(&window);
        run_for(Duration::from_millis(50));
        assert!(preview.is_open());
        hover(&window, TO_LONG_PAGE);
        run_for(Duration::from_millis(250));
        assert!(preview.is_open(), "the pointer came back to the link");

        leave_link(&window);
        wait_until(|| !preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn the_pointer_over_the_popup_keeps_it_open() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let popup = controller::<gtk::EventControllerMotion>(&preview.popup_widget().unwrap());

        popup.emit_by_name::<()>("enter", &[&5.0f64, &5.0f64]);
        leave_link(&window);
        run_for(CLOSE * 5);
        assert!(preview.is_open());

        popup.emit_by_name::<()>("leave", &[]);
        wait_until(|| !preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn disabled_previews_keep_the_link_tooltip_and_never_start() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        assert_eq!(source_page(&window).tooltip_text(), None);
        leave_link(&window);
        preview.dismiss();

        set_enabled(false);
        hover(&window, TO_LONG_PAGE);
        assert!(!preview.is_pending());
        run_for(OPEN * 4);
        assert!(!preview.is_open());
        assert!(source_page(&window).tooltip_text().is_some());
        window.close();
    }

    #[gtk::test]
    fn a_moving_pane_starts_no_preview() {
        let window = links_window();
        let preview = preview(&window);
        let pane = window.pane();
        pane.viewport().set_animate_scroll(true);

        reader_key(&window, Key::l);
        assert!(pane.is_moving());
        hover(&window, TO_LONG_PAGE);

        assert!(!preview.is_pending());
        window.close();
    }

    #[gtk::test]
    fn a_dismissed_preview_stays_closed() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        preview.dismiss();
        run_for(OPEN * 4);
        assert!(!preview.is_open(), "the stale timer opened nothing");

        leave_link(&window);
        let preview = open(&window, TO_LONG_PAGE);
        preview.dismiss();
        run_for(Duration::from_millis(300));
        assert!(!preview.is_open(), "late renders opened nothing");
        window.close();
    }

    #[gtk::test]
    fn escape_closes_an_open_preview_and_is_consumed() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        assert_eq!(
            key(&window, Key::Escape, ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn a_modifier_alone_keeps_the_preview() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        key(&window, Key::Control_L, ModifierType::empty());

        assert!(preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn escape_cancels_a_pending_preview() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        key(&window, Key::Escape, ModifierType::empty());
        assert!(!preview.is_pending());
        run_for(OPEN * 4);
        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn scrolling_the_document_closes_the_preview() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let pane = window.pane();
        let scroll = controller::<gtk::EventControllerScroll>(&pane);
        assert_eq!(scroll.propagation_phase(), gtk::PropagationPhase::Capture);

        let taken = scroll.emit_by_name::<bool>("scroll", &[&0.0f64, &1.0f64]);

        assert!(!taken, "the document still scrolls");
        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn navigation_closes_the_preview() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        window.pane().goto_page(2);

        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn a_tab_switch_closes_the_preview() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        window.header().add_document().expect("a second tab");

        wait_until(|| !preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn a_load_closes_the_preview() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        window.load(&fixture("links.pdf"));

        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn closing_a_split_pane_closes_the_preview() {
        let window = links_window();
        window.split_here();
        wait_until(|| window.imp().split_settled() && source_page_mapped(&window));
        let preview = open(&window, TO_LONG_PAGE);

        assert!(window.close_active_pane());

        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn a_resize_closes_the_preview() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        window.set_default_size(1100, 800);

        wait_until(|| !preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn the_footer_checkbox_turns_previews_off() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);

        preview.checkbox().unwrap().set_active(true);

        assert!(!preview.is_open());
        assert!(!enabled());
        assert!(!crate::config::load_config().show_link_previews);
        window.close();
    }

    #[gtk::test]
    fn turning_previews_off_cancels_a_pending_preview() {
        let window = links_window();
        let preview = preview(&window);

        hover(&window, TO_LONG_PAGE);
        apply_preference(false);

        assert!(!preview.is_pending());
        run_for(OPEN * 4);
        assert!(!preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn the_menu_action_turns_previews_back_on() {
        let window = links_window();
        apply_preference(false);
        let action = preference_action();
        assert_eq!(action.state().and_then(|v| v.get::<bool>()), Some(false));

        action.activate(None);

        assert_eq!(action.state().and_then(|v| v.get::<bool>()), Some(true));
        assert!(enabled());
        assert!(crate::config::load_config().show_link_previews);
        hover(&window, TO_LONG_PAGE);
        wait_until(|| preview(&window).is_open());
        window.close();
    }

    #[gtk::test]
    fn tab_focuses_the_checkbox_and_escape_restores_the_focus() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let before = focus(&window);

        assert_eq!(
            key(&window, Key::Tab, ModifierType::empty()),
            glib::Propagation::Stop
        );
        let checkbox = preview.checkbox().unwrap();
        assert_eq!(focus(&window), Some(checkbox.clone().upcast()));
        assert!(preview.is_open());

        leave_link(&window);
        run_for(CLOSE * 5);
        assert!(preview.is_open(), "focus inside keeps the popup");
        assert_eq!(
            key(&window, Key::space, ModifierType::empty()),
            glib::Propagation::Proceed,
            "Space reaches the checkbox"
        );
        assert!(preview.is_open());

        assert_eq!(
            key(&window, Key::Escape, ModifierType::empty()),
            glib::Propagation::Stop
        );
        assert!(!preview.is_open());
        assert_eq!(focus(&window), before);
        window.close();
    }

    #[gtk::test]
    fn ctrl_tab_closes_the_preview_and_switches_the_tab() {
        let window = links_window();
        let first = window.header().active_document().unwrap();
        window.header().add_document().expect("a second tab");
        window.header().notebook.set_current_page(Some(0));
        wait_until(|| source_page_mapped(&window));
        let preview = open(&window, TO_LONG_PAGE);

        assert_eq!(
            key(&window, Key::Tab, ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );

        assert!(!preview.is_open());
        assert_ne!(window.header().active_document().as_ref(), Some(&first));
        window.close();
    }

    fn source_page_mapped(window: &TestWindow) -> bool {
        window.pane().imp().mapped_page(0).is_some()
    }

    #[gtk::test]
    fn window_shortcuts_work_while_the_checkbox_has_focus() {
        let window = links_window();
        let first = window.header().active_document().unwrap();
        window.header().add_document().expect("a second tab");
        window.header().notebook.set_current_page(Some(0));
        wait_until(|| source_page_mapped(&window));
        let preview = open(&window, TO_LONG_PAGE);
        key(&window, Key::Tab, ModifierType::empty());
        assert!(preview.checkbox().unwrap().has_focus());

        assert_eq!(
            key(&window, Key::Tab, ModifierType::CONTROL_MASK),
            glib::Propagation::Stop
        );

        assert_ne!(window.header().active_document().as_ref(), Some(&first));
        wait_until(|| !preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn the_popup_closes_when_focus_and_pointer_leave() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        key(&window, Key::Tab, ModifierType::empty());
        leave_link(&window);
        run_for(CLOSE * 5);
        assert!(preview.is_open(), "focus inside keeps the popup");

        window.pane().focus_reader();

        wait_until(|| !preview.is_open());
        window.close();
    }

    #[gtk::test]
    fn shift_tab_closes_the_preview_without_entering_it() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let checkbox = preview.checkbox().unwrap();

        key(&window, Key::ISO_Left_Tab, ModifierType::SHIFT_MASK);

        assert!(!preview.is_open());
        assert_ne!(focus(&window), Some(checkbox.upcast()));
        window.close();
    }

    #[gtk::test]
    fn another_key_closes_the_preview_and_still_acts() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let viewport = window.pane().viewport().clone();
        let page = viewport.page();

        // Match event order: window capture, then the document key handler.
        assert_eq!(
            key(&window, Key::l, ModifierType::empty()),
            glib::Propagation::Proceed
        );
        assert!(!preview.is_open());
        reader_key(&window, Key::l);

        wait_until(|| viewport.page() != page);
        window.close();
    }

    #[gtk::test]
    fn an_xyz_link_shows_its_coordinates_at_a_third_of_the_popup() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let vadj = preview.scroller().unwrap().vadjustment();
        let zoom = preview.viewport().unwrap().zoom();

        wait_until(|| vadj.value() > 0.0);

        let expected = LONG_PAGE_DEST_Y * zoom - vadj.page_size() / 3.0;
        assert!(expected > 0.0 && expected < vadj.upper() - vadj.page_size());
        assert!(
            (vadj.value() - expected).abs() < 1.0,
            "{} vs {expected}",
            vadj.value()
        );
        window.close();
    }

    #[gtk::test]
    fn a_fit_link_starts_at_the_top() {
        let window = links_window();
        let preview = open(&window, TO_SECOND_FIT);
        let vadj = preview.scroller().unwrap().vadjustment();

        run_for(Duration::from_millis(100));

        assert_eq!(vadj.value(), 0.0);
        window.close();
    }

    #[gtk::test]
    fn the_preview_leaves_the_document_unchanged() {
        let window = links_window();
        let viewport = window.pane().viewport().clone();
        let before = (
            viewport.page(),
            viewport.zoom(),
            viewport.prev_page(),
            viewport.next_page(),
            focus(&window),
        );

        let preview = open(&window, TO_LONG_PAGE);
        let vadj = preview.scroller().unwrap().vadjustment();
        wait_until(|| vadj.value() > 0.0);
        vadj.set_value(0.0);
        leave_link(&window);
        wait_until(|| !preview.is_open());

        let after = (
            viewport.page(),
            viewport.zoom(),
            viewport.prev_page(),
            viewport.next_page(),
            focus(&window),
        );
        assert_eq!(after, before);
        window.close();
    }

    #[gtk::test]
    fn the_popup_is_placed_beside_the_link_inside_the_view() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let popup = preview.popup_widget().unwrap();
        let overlay = popup.parent().expect("the overlay");
        wait_until(|| popup.width() > 0);

        let bounds = popup.compute_bounds(&overlay).unwrap();
        assert!(f64::from(bounds.x()) >= MARGIN - 0.5);
        assert!(f64::from(bounds.y()) >= MARGIN - 0.5);
        assert!(
            f64::from(bounds.x() + bounds.width()) <= f64::from(overlay.width()) - MARGIN + 0.5
        );
        assert!(
            f64::from(bounds.y() + bounds.height()) <= f64::from(overlay.height()) - MARGIN + 0.5
        );
        let (width, height) = source_page(&window).size_request();
        assert!(f64::from(bounds.width()) <= f64::from(width) + 0.5);
        assert!(f64::from(bounds.height()) <= f64::from(height) + 0.5);
        window.close();
    }

    #[gtk::test]
    fn a_page_that_fits_shows_whole_with_the_footer() {
        let window = links_window();
        window.pane().viewport().zoom_to(0.5);
        wait_until(|| source_page(&window).size_request().1 < 450);
        let preview = open(&window, TO_SECOND_FIT);
        let vadj = preview.scroller().unwrap().vadjustment();
        let checkbox = preview.checkbox().unwrap();

        wait_until(|| vadj.page_size() > 0.0 && checkbox.height() > 0);

        assert!(
            vadj.upper() <= vadj.page_size() + 0.5,
            "{} > {}",
            vadj.upper(),
            vadj.page_size()
        );
        window.close();
    }

    // Restore the global setting, also when a test fails.
    struct HeightPercent;

    impl HeightPercent {
        fn set(percent: u32) -> Self {
            set_height_percent(percent);
            Self
        }
    }

    impl Drop for HeightPercent {
        fn drop(&mut self) {
            set_height_percent(crate::config::DEFAULT_LINK_PREVIEW_HEIGHT_PERCENT);
        }
    }

    #[gtk::test]
    fn the_height_setting_shows_part_of_the_page_with_a_scroll() {
        let _height = HeightPercent::set(30);
        let window = links_window();
        window.pane().viewport().zoom_to(0.5);
        wait_until(|| source_page(&window).size_request().1 < 450);
        let preview = open(&window, TO_SECOND_FIT);
        let vadj = preview.scroller().unwrap().vadjustment();
        let target = target_page(&preview);

        wait_until(|| vadj.page_size() > 0.0 && target.height() > 0);

        let expected = f64::from(target.height()) * 0.3;
        assert!(
            (vadj.page_size() - expected).abs() <= 1.0,
            "{} != {expected}",
            vadj.page_size()
        );
        assert!(vadj.upper() > vadj.page_size() + 1.0);
        window.close();
    }

    #[gtk::test]
    fn a_short_height_setting_places_the_popup_above_or_below() {
        let _height = HeightPercent::set(30);
        let window = links_window();
        let preview = open(&window, TO_SECOND_FIT);
        let popup = preview.popup_widget().unwrap();
        let overlay = popup.parent().expect("the overlay");
        wait_until(|| popup.width() > 0);

        let page = source_page(&window);
        let bbox = page.document().page_bounds(0, page.crop()).unwrap();
        let (x, y) = TO_SECOND_FIT;
        let point = gtk::graphene::Point::new(
            ((x - bbox.x1) * page.zoom()) as f32,
            ((y - bbox.y1) * page.zoom()) as f32,
        );
        let link = page.compute_point(&overlay, &point).unwrap();
        let bounds = popup.compute_bounds(&overlay).unwrap();
        assert!(
            bounds.x() <= link.x() && link.x() <= bounds.x() + bounds.width(),
            "{bounds:?} is beside {link:?}"
        );
        assert!(
            link.y() < bounds.y() || bounds.y() + bounds.height() < link.y(),
            "{bounds:?} covers {link:?}"
        );
        window.close();
    }

    #[gtk::test]
    fn dismissal_releases_the_popup_renders() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let id = preview.viewport().unwrap().id();
        let document = window.document().clone();
        wait_until(|| document.render_cache().borrow().has_pins(id));

        preview.dismiss();

        assert!(!document.render_cache().borrow().has_pins(id));
        assert!(!document.has_render_interest(id));
        window.close();
    }

    #[gtk::test]
    fn a_render_the_document_also_needs_survives_dismissal() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let popup = preview.viewport().unwrap();
        let main = window.pane().viewport().clone();
        let document = window.document().clone();
        let key = crate::document::RenderJobKey::Page(
            crate::render_cache::PageRenderKey::from_factors(1, 0.123, 1.0),
        );
        document.request_render(key.clone(), &popup, None);
        document.request_render(key.clone(), &main, None);

        preview.dismiss();

        assert!(document.has_render_job(&key));
        assert!(document.has_render_interest(main.id()));
        assert!(!document.has_render_interest(popup.id()));
        window.close();
    }

    #[gtk::test]
    fn a_scrolled_popup_pins_the_visible_tiles() {
        let window = links_window();
        let preview = open(&window, TO_LONG_PAGE);
        let id = preview.viewport().unwrap().id();
        let document = window.document().clone();
        let vadj = preview.scroller().unwrap().vadjustment();
        let page = target_page(&preview);
        wait_until(|| page.uses_tiles());
        let dsf = crate::page::device_scale(&page);
        let visible_rows = |value: f64, tiles: &[crate::render_cache::TileId]| {
            let top = value * dsf;
            let bottom = (value + vadj.page_size()) * dsf;
            !tiles.is_empty()
                && tiles
                    .iter()
                    .all(|tile| f64::from(tile.y) < bottom && f64::from(tile.y + 1024) > top)
        };

        wait_until(|| vadj.value() > 0.0);
        wait_until(|| {
            visible_rows(
                vadj.value(),
                &document.render_cache().borrow().pinned_tiles(id),
            )
        });
        vadj.set_value(0.0);
        wait_until(|| {
            let tiles = document.render_cache().borrow().pinned_tiles(id);
            visible_rows(0.0, &tiles) && tiles.iter().any(|tile| tile.y == 0)
        });
        window.close();
    }

    #[gtk::test]
    fn a_zoom_in_another_pane_keeps_the_preview() {
        let window = links_window();
        window.split_here();
        wait_until(|| window.imp().split_settled() && source_page_mapped(&window));
        let preview = open(&window, TO_LONG_PAGE);
        let other = window.imp().active_pane();
        assert_ne!(other, window.pane());

        other.viewport().zoom_to(other.viewport().zoom() * 1.1);
        assert!(preview.is_open());

        window
            .pane()
            .viewport()
            .zoom_to(window.pane().viewport().zoom() * 1.1);
        assert!(!preview.is_open());
        window.close();
    }

    // XTest events exercise capture handlers, the native scroller, and popup controllers in event order.
    // Touchpad and pinch tests need real hardware.
    mod routing {
        use std::process::{Command, Stdio};

        use super::*;

        // Require X11 and xdotool in CI. Skip these tests elsewhere if either is absent.
        fn available() -> bool {
            let x11 = gtk::gdk::Display::default()
                .is_some_and(|display| display.type_().name() == "GdkX11Display");
            let xdotool = Command::new("xdotool")
                .arg("version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if x11 && xdotool {
                return true;
            }
            assert!(
                std::env::var_os("CI").is_none(),
                "CI needs X11 and xdotool (x11={x11}, xdotool={xdotool})"
            );
            eprintln!("skipped: needs X11 and xdotool (x11={x11}, xdotool={xdotool})");
            false
        }

        // Process GTK events while xdotool waits for event delivery.
        fn xdotool(args: &[&str]) -> String {
            let mut child = Command::new("xdotool")
                .args(args)
                .stdout(Stdio::piped())
                .spawn()
                .expect("xdotool runs");
            wait_until(|| child.try_wait().expect("xdotool status").is_some());
            let output = child.wait_with_output().expect("xdotool output");
            assert!(output.status.success(), "xdotool {args:?} failed");
            String::from_utf8(output.stdout).expect("xdotool prints text")
        }

        struct Screen {
            window: gtk::Widget,
            origin: (f64, f64),
        }

        impl Screen {
            fn new(test: &TestWindow) -> Self {
                let window = gtk::subclass::prelude::ObjectSubclassExt::obj(test.header()).clone();
                let title = format!("scrolex-routing-{}", glib::random_int());
                window.set_title(Some(&title));
                let id = xdotool(&["search", "--sync", "--name", &format!("^{title}$")])
                    .lines()
                    .next()
                    .expect("the test window")
                    .to_string();
                let shell = xdotool(&["getwindowgeometry", "--shell", &id]);
                let value = |key: &str| {
                    shell
                        .lines()
                        .find_map(|line| line.strip_prefix(key))
                        .and_then(|v| v.parse::<f64>().ok())
                        .expect("window geometry")
                };
                let (dx, dy) = window.surface_transform();
                xdotool(&["windowfocus", "--sync", &id]);
                Self {
                    window: window.upcast(),
                    origin: (value("X=") + dx, value("Y=") + dy),
                }
            }

            fn point(&self, widget: &impl IsA<gtk::Widget>, x: f64, y: f64) -> (String, String) {
                let p = widget
                    .compute_point(&self.window, &gtk::graphene::Point::new(x as f32, y as f32))
                    .expect("the widget is in the window");
                (
                    format!("{}", (self.origin.0 + f64::from(p.x())).round()),
                    format!("{}", (self.origin.1 + f64::from(p.y())).round()),
                )
            }

            fn move_to(&self, widget: &impl IsA<gtk::Widget>, x: f64, y: f64) {
                let (x, y) = self.point(widget, x, y);
                xdotool(&["mousemove", "--sync", &x, &y]);
                run_for(Duration::from_millis(50));
            }

            fn wheel(&self, button: &str, times: usize) {
                for _ in 0..times {
                    xdotool(&["click", button]);
                }
                run_for(Duration::from_millis(100));
            }

            fn key(&self, key: &str) {
                xdotool(&["key", "--clearmodifiers", key]);
                run_for(Duration::from_millis(50));
            }

            fn ctrl_wheel(&self, button: &str) {
                xdotool(&["keydown", "ctrl", "click", button, "keyup", "ctrl"]);
                run_for(Duration::from_millis(100));
            }
        }

        // Use production delays to test pointer travel across the gap to the popup.
        fn routed_window() -> Option<(TestWindow, Screen)> {
            if !available() {
                return None;
            }
            let window = links_window();
            preview(&window).set_delays(OPEN_DELAY, CLOSE_DELAY);
            let screen = Screen::new(&window);
            Some((window, screen))
        }

        fn move_to_link(window: &TestWindow, screen: &Screen, (x, y): (f64, f64)) {
            let page = source_page(window);
            let document = page.document();
            document.request_links(0);
            wait_until(|| document.imp().links.borrow().get_link(0, x, y).is_some());
            let bbox = document.page_bounds(0, page.crop()).expect("page 0 bounds");
            let zoom = page.zoom();
            // Start elsewhere to ensure that the move to the link emits a motion event.
            screen.move_to(&page, 2.0, 2.0);
            screen.move_to(&page, (x - bbox.x1) * zoom, (y - bbox.y1) * zoom);
        }

        fn open_routed(window: &TestWindow, screen: &Screen, link: (f64, f64)) -> LinkPreview {
            move_to_link(window, screen, link);
            let preview = preview(window);
            wait_until(|| preview.is_open());
            let popup = preview.popup_widget().unwrap();
            wait_until(|| popup.width() > 0);
            preview
        }

        fn into_popup(screen: &Screen, preview: &LinkPreview) {
            let scroller = preview.scroller().unwrap();
            screen.move_to(
                &scroller,
                f64::from(scroller.width()) / 2.0,
                f64::from(scroller.height()) / 2.0,
            );
        }

        fn main_state(window: &TestWindow) -> (u32, f64, f64, f64) {
            let pane = window.pane();
            (
                pane.viewport().page(),
                pane.viewport().zoom(),
                pane.imp().scrolledwindow.hadjustment().value(),
                pane.imp().vscrolledwindow.vadjustment().value(),
            )
        }

        #[gtk::test]
        fn a_real_hover_opens_and_escape_closes() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let preview = open_routed(&window, &screen, TO_LONG_PAGE);

            screen.key("Escape");

            wait_until(|| !preview.is_open());
            window.close();
        }

        #[gtk::test]
        fn a_held_button_over_a_link_starts_no_preview() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let page = source_page(&window);
            screen.move_to(&page, 5.0, 5.0);
            xdotool(&["mousedown", "1"]);
            move_to_link(&window, &screen, TO_LONG_PAGE);
            let pending = preview(&window).is_pending();
            xdotool(&["mouseup", "1"]);

            assert!(!pending);
            window.close();
        }

        #[gtk::test]
        fn the_wheel_scrolls_the_popup_and_not_the_document() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let preview = open_routed(&window, &screen, TO_SECOND_FIT);
            into_popup(&screen, &preview);
            let before = main_state(&window);
            let vadj = preview.scroller().unwrap().vadjustment();

            screen.wheel("5", 3);

            wait_until(|| vadj.value() > 0.0);
            assert!(preview.is_open());
            assert_eq!(main_state(&window), before);
            window.close();
        }

        #[gtk::test]
        fn the_wheel_at_the_popup_limits_stays_in_the_popup() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let preview = open_routed(&window, &screen, TO_SECOND_FIT);
            into_popup(&screen, &preview);
            let before = main_state(&window);
            let vadj = preview.scroller().unwrap().vadjustment();

            screen.wheel("4", 3);
            assert_eq!(vadj.value(), 0.0);
            assert_eq!(main_state(&window), before, "top limit");

            vadj.set_value(vadj.upper() - vadj.page_size());
            screen.wheel("5", 3);
            assert_eq!(main_state(&window), before, "bottom limit");
            assert!(preview.is_open());
            window.close();
        }

        #[gtk::test]
        fn ctrl_wheel_over_the_popup_zooms_nothing() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let preview = open_routed(&window, &screen, TO_SECOND_FIT);
            into_popup(&screen, &preview);
            let before = main_state(&window);
            let popup_zoom = preview.viewport().unwrap().zoom();
            let vadj = preview.scroller().unwrap().vadjustment();

            screen.ctrl_wheel("4");
            screen.ctrl_wheel("5");

            assert_eq!(main_state(&window), before);
            assert_eq!(preview.viewport().unwrap().zoom(), popup_zoom);
            assert_eq!(vadj.value(), 0.0);
            assert!(preview.is_open());
            window.close();
        }

        #[gtk::test]
        fn the_wheel_over_the_document_closes_the_popup_and_steps() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let preview = open_routed(&window, &screen, TO_SECOND_FIT);
            let popup = preview.popup_widget().unwrap();
            let overlay = popup.parent().unwrap();
            let bounds = popup.compute_bounds(&overlay).unwrap();
            let y = (f64::from(bounds.y() + bounds.height()) + f64::from(overlay.height())) / 2.0;
            screen.move_to(&overlay, f64::from(bounds.x()) + 20.0, y);
            let viewport = window.pane().viewport().clone();
            let page = viewport.page();

            screen.wheel("5", 1);

            wait_until(|| !preview.is_open());
            wait_until(|| viewport.page() != page);
            window.close();
        }

        #[gtk::test]
        fn tab_and_space_turn_previews_off_from_the_keyboard() {
            let Some((window, screen)) = routed_window() else {
                return;
            };
            let preview = open_routed(&window, &screen, TO_LONG_PAGE);

            screen.key("Tab");
            assert!(preview.checkbox().unwrap().has_focus());
            screen.key("space");

            wait_until(|| !preview.is_open());
            assert!(!enabled());
            window.close();
        }
    }
}
