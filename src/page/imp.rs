// Page widget rendering, preview scheduling, and document interaction.
#![expect(unused_lifetimes)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

use futures::channel::oneshot;
use gtk::cairo::{FontSlant, FontWeight};
use gtk::gdk::prelude::*;
use gtk::gdk::{MemoryFormat, MemoryTexture, BUTTON_PRIMARY, RGBA};
use gtk::glib;
use gtk::glib::clone;
use gtk::glib::subclass::{prelude::*, Signal};
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use once_cell::sync::Lazy;

use super::Rectangle;
use crate::bg_job::{RenderPool, RenderPriority};
use crate::links::{LinkAction, LinkRequest, LinkTarget};
use crate::selection::PageSelection;

// Max bytes in one page buffer. A whole page is rendered at once, so the buffer grows with the
// scale squared. render_scale keeps it under this.
pub(crate) const MAX_PAGE_BYTES: f64 = 128.0 * 1024.0 * 1024.0;
// Max pixels per axis. GPUs commonly refuse a texture wider or taller than this. A long thin page
// can pass this while still under MAX_PAGE_BYTES.
const MAX_TEXTURE_DIM: f64 = 16384.0;
// Fixed device-pixel grid for viewport rendering. A 1024-square BGRx texture is 4 MiB, which keeps
// the usual viewport to a small serial batch while bounding every allocation independently.
const TILE_SIZE: i32 = 1024;
const TILE_GUTTER: i32 = 1;

// Low-resolution previews rendered ahead of the visible page and shown (upscaled) while the full
// render is pending, so aggressive scrolling shows blurry pages rather than blank ones. The render
// scale adapts per document toward two budgets at once (see adapt_preview_scale): PREVIEW_TARGET_MS
// keeps a single preview fast, and the preview-cache budget spread over the resident window keeps
// each preview small enough that the whole window stays cached without thrashing. Stored in page
// units (device scale 1) so they survive zoom and are rescaled at draw time.
pub(crate) const PREVIEW_INITIAL_SCALE: f64 = 0.25;
const PREVIEW_MIN_SCALE: f64 = 0.1;
const PREVIEW_MAX_SCALE: f64 = 0.5;
// Per-preview render-time budget the adaptive scale steers toward.
const PREVIEW_TARGET_MS: u128 = 40;
// Pages either side of the visible one to keep previewed. Symmetric so scrolling back has as much
// runway as forward; already-cached pages are skipped, so effort tracks the direction of travel.
const PREVIEW_WINDOW: i32 = 32;
const MAX_INFLIGHT_PREVIEWS: usize = 12;
// A preview slower than this even at PREVIEW_MIN_SCALE means shrinking won't help (decode-bound
// scans, where a low-res render is no cheaper than the full one).
const PREVIEW_SLOW_MS: u128 = 250;
// Consecutive slow-at-min-scale previews before giving up on the document; shrugs off one-off outliers.
const PREVIEW_SLOW_STREAK_LIMIT: u32 = 5;

thread_local!(
    // Pool caps: visible-preview, visible, preview, prefetch. Fast-scroll flooding is bounded by the
    // wanted-range filter (out-of-view full renders dropped on pop), so caps can be generous.
    static RENDER_QUEUE: Lazy<RenderPool> = Lazy::new(|| {
        RenderPool::new(
            crate::config::DEFAULT_RENDER_THREADS,
            8,
            8,
            MAX_INFLIGHT_PREVIEWS,
            8,
        )
    });
);

// Resize the render pool. The pool starts at DEFAULT_RENDER_THREADS; the window applies the
// configured count at startup and whenever the setting changes.
pub(crate) fn set_render_threads(n: usize) {
    RENDER_QUEUE.with(|queue| queue.set_size(n));
}

pub(crate) fn set_wanted_pages(
    document: crate::document::DocumentRenderId,
    viewport: crate::viewport::ViewportId,
    range: Option<(i32, i32)>,
) {
    RENDER_QUEUE.with(|queue| queue.set_wanted(document.raw(), viewport.raw(), range));
}

// Remove one viewport from queued full renders. Previews survive zoom.
pub(crate) fn clear_full_renders(viewport: crate::viewport::ViewportId) {
    RENDER_QUEUE.with(|queue| queue.clear_full(viewport.raw()));
}

pub(crate) fn clear_document_renders(document: crate::document::DocumentRenderId) {
    RENDER_QUEUE.with(|queue| queue.clear_all_document(document.raw()));
}

// How many pages to prefetch ahead: the threads not busy on visible pages, but never more full
// pages than the cache can hold beyond the visible ones - else completed prefetches evict the
// visible pages and thrash. At deep zoom `page_bytes` alone fills the budget, which yields 0: pages
// that big are neither cacheable nor worth rendering unseen.
fn prefetch_depth(threads: usize, visible: usize, page_bytes: usize, budget: usize) -> usize {
    let want = threads.saturating_sub(visible);
    if page_bytes == 0 {
        return want.max(1);
    }
    want.min((budget / page_bytes).saturating_sub(visible + 1))
}

// Bytes a full render of a page this size allocates, truncated as the renderer truncates. In f64: an
// extreme page size wraps integer maths, and a wrapped product reads as under the cap.
fn page_buffer_bytes(page_pt: (f64, f64), scale: f64, dsf: f64) -> f64 {
    let width = (page_pt.0 * scale * dsf).trunc().max(0.0);
    let height = (page_pt.1 * scale * dsf).trunc().max(0.0);
    width * height * 4.0
}

// Surface scale includes fractional display density.
pub(crate) fn device_scale(widget: &impl IsA<gtk::Widget>) -> f64 {
    let widget = widget.as_ref();
    widget
        .native()
        .and_then(|native| native.surface())
        .map_or_else(
            || f64::from(widget.scale_factor()),
            |surface| surface.scale(),
        )
        .max(1.0)
}

// Scale to render this page at: the zoom, or the biggest scale that still fits the caps. A big page
// at deep zoom is then rendered small and upscaled - soft, but every zoom stays reachable.
fn render_scale(page_pt: (f64, f64), zoom: f64, dsf: f64) -> f64 {
    let (w, h) = (page_pt.0 * dsf, page_pt.1 * dsf);
    let (area, longest) = (w * h * 4.0, w.max(h));
    if !area.is_finite() || area <= 0.0 || !longest.is_finite() {
        return zoom;
    }
    zoom.min((MAX_PAGE_BYTES / area).sqrt())
        .min(MAX_TEXTURE_DIM / longest)
}

fn render_dimensions(page_pt: (f64, f64), scale: f64, dsf: f64) -> (i32, i32) {
    (
        ((page_pt.0 * scale * dsf) as i32).max(1),
        ((page_pt.1 * scale * dsf) as i32).max(1),
    )
}

// Log each scale pair once for display diagnostics.
fn log_device_scale(page: &super::Page) {
    thread_local! {
        static LAST: Cell<(i32, u64)> = const { Cell::new((0, 0)) };
    }
    let Some(surface) = page.native().and_then(|native| native.surface()) else {
        return;
    };
    let scale = surface.scale();
    let current = (page.scale_factor(), scale.to_bits());
    if LAST.with(|last| last.replace(current)) != current {
        log::info!(
            "device scale: widget_scale_factor={} surface_scale={scale}",
            current.0
        );
    }
}

fn tile_regions(
    page_px: (i32, i32),
    visible_px: (f64, f64, f64, f64),
) -> Vec<crate::mupdf_render::PixelRect> {
    let (page_w, page_h) = page_px;
    let (x0, y0, x1, y1) = visible_px;
    let x0 = x0.floor().clamp(0.0, page_w as f64) as i32;
    let y0 = y0.floor().clamp(0.0, page_h as f64) as i32;
    let x1 = x1.ceil().clamp(0.0, page_w as f64) as i32;
    let y1 = y1.ceil().clamp(0.0, page_h as f64) as i32;
    if x0 >= x1 || y0 >= y1 {
        return Vec::new();
    }

    let first_x = x0 / TILE_SIZE;
    let first_y = y0 / TILE_SIZE;
    let last_x = (x1 - 1) / TILE_SIZE;
    let last_y = (y1 - 1) / TILE_SIZE;
    let mut regions = Vec::new();
    for y in first_y..=last_y {
        for x in first_x..=last_x {
            let left = x * TILE_SIZE;
            let top = y * TILE_SIZE;
            regions.push(crate::mupdf_render::PixelRect::new(
                left,
                top,
                (left + TILE_SIZE).min(page_w),
                (top + TILE_SIZE).min(page_h),
            ));
        }
    }
    regions
}

fn raster_region(
    region: crate::mupdf_render::PixelRect,
    page_px: (i32, i32),
) -> crate::mupdf_render::PixelRect {
    crate::mupdf_render::PixelRect::new(
        (region.x0 - TILE_GUTTER).max(0),
        (region.y0 - TILE_GUTTER).max(0),
        (region.x1 + TILE_GUTTER).min(page_px.0),
        (region.y1 + TILE_GUTTER).min(page_px.1),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum FallbackSource {
    Render,
    Preview,
    None,
}

fn fallback_source(render_width: Option<i32>, preview_width: Option<i32>) -> FallbackSource {
    match (render_width, preview_width) {
        (Some(render), Some(preview)) if preview > render => FallbackSource::Preview,
        (Some(_), _) => FallbackSource::Render,
        (None, Some(_)) => FallbackSource::Preview,
        (None, None) => FallbackSource::None,
    }
}

fn needs_visible_preview(
    render_width: Option<i32>,
    has_preview: bool,
    preview_target_width: i32,
) -> bool {
    !has_preview && render_width.is_none_or(|width| width < preview_target_width)
}

// Preview prefetch half-width, bounded so both directions fit the preview cache - else big-page docs
// schedule previews that evict each other, thrashing the cache and render pool. Full window until
// the cache has sized its first preview (`capacity` 0).
fn preview_window(capacity: usize) -> i32 {
    if capacity == 0 {
        PREVIEW_WINDOW
    } else {
        (capacity as i32 / 2).clamp(1, PREVIEW_WINDOW)
    }
}

#[derive(Default, glib::Properties)]
#[properties(wrapper_type = super::Page)]
pub struct Page {
    #[property(get = Self::viewport, set, construct_only, type = crate::viewport::Viewport)]
    viewport: RefCell<Option<crate::viewport::Viewport>>,

    #[property(get, set)]
    pub(crate) binding: RefCell<Option<glib::Binding>>,

    #[property(get, set)]
    index: Cell<i32>,

    bbox: RefCell<Rectangle>,
    cursor_guard: Cell<bool>,

    // False prevents a discarded render at a provisional display scale.
    scale_known: Cell<bool>,
    surface_scale_connection: RefCell<Option<(gtk::gdk::Surface, glib::SignalHandlerId)>>,

    // last snapshot's (page index, paint, zoom); see note_paint
    painted: Cell<Option<(i32, Paint, f64)>>,

    // Whether this widget currently needs viewport regions instead of a whole-page texture.
    tiled: Cell<bool>,
}

// What a snapshot drew, from best-looking to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paint {
    Sharp,
    // capped render, upscaled: the best this zoom can look
    Upscaled,
    StaleRender,
    Preview,
    Placeholder,
    Blank,
}

impl Paint {
    fn fidelity(self) -> u8 {
        match self {
            Paint::Sharp => 5,
            Paint::Upscaled => 4,
            Paint::StaleRender => 3,
            Paint::Preview => 2,
            Paint::Placeholder => 1,
            Paint::Blank => 0,
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "Page";
    type Type = super::Page;
    type ParentType = gtk::Widget;
}

#[glib::derived_properties]
impl ObjectImpl for Page {
    fn constructed(&self) {
        self.parent_constructed();

        self.setup_scale_tracking();
        self.setup_viewport_listeners();
        self.setup_text_selection();
        self.setup_link_handling();

        self.obj().connect_unmap(|page| page.imp().unpin_render());

        self.obj().set_size_request(600, 800);
    }

    fn dispose(&self) {
        if let Some((surface, connection)) = self.surface_scale_connection.take() {
            surface.disconnect(connection);
        }
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![Signal::builder("internal-link-clicked")
                .param_types([LinkRequest::static_type()])
                .build()]
        })
    }
}

impl WidgetImpl for Page {
    fn snapshot(&self, snapshot: &gtk::Snapshot) {
        let Some(page) = self.page_info() else {
            return;
        };

        // Hold off rendering until the final device scale factor is known (set just after map);
        // otherwise the first render uses a provisional scale and is immediately re-rendered. Paint
        // blank meanwhile - the deferred redraw renders at the real scale.
        if !self.scale_known.get() {
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            if w > 0.0 && h > 0.0 {
                snapshot.append_color(&page_background(), &graphene::Rect::new(0.0, 0.0, w, h));
            }
            self.note_paint(page.index, Paint::Blank);
            return;
        }

        if self.obj().document().multithread_rendering() {
            self.multithread_snapshot(snapshot, &page);
        } else {
            self.render_snapshot(snapshot, &page);
        }

        self.snapshot_selection_overlay(snapshot, &page);
        self.snapshot_search_overlay(snapshot, &page);
    }
}

impl Page {
    pub(super) fn uses_tiles(&self) -> bool {
        self.tiled.get()
    }

    pub(super) fn unpin_render(&self) {
        let obj = self.obj();
        obj.document()
            .render_cache()
            .borrow_mut()
            .unpin_page(obj.viewport().id(), obj.index());
        self.tiled.set(false);
    }

    // Track the display scale after map.
    fn setup_scale_tracking(&self) {
        let obj = self.obj();

        // The compositor assigns the surface scale after map.
        // Scale notifications run before idle callbacks, so one idle cycle exposes the assigned scale.
        obj.connect_map(|page| {
            page.imp().track_surface_scale();

            if page.imp().scale_known.get() {
                return;
            }
            glib::idle_add_local_once(clone!(
                #[weak]
                page,
                move || {
                    page.imp().display_scale_changed();
                }
            ));
        });

        obj.connect_scale_factor_notify(|page| page.imp().display_scale_changed());
    }

    fn display_scale_changed(&self) {
        let page = self.obj();
        self.scale_known.set(true);
        log_device_scale(&page);
        // The next snapshot rejects cached textures that use a different display scale.
        page.queue_draw();
    }

    fn track_surface_scale(&self) {
        let page = self.obj();
        let Some(surface) = page.native().and_then(|native| native.surface()) else {
            return;
        };
        let mut connection = self.surface_scale_connection.borrow_mut();
        if connection
            .as_ref()
            .is_some_and(|(tracked, _)| tracked == &surface)
        {
            return;
        }
        if let Some((tracked, id)) = connection.take() {
            tracked.disconnect(id);
        }
        let id = surface.connect_scale_notify(clone!(
            #[weak]
            page,
            move |_| {
                page.imp().display_scale_changed();
            }
        ));
        *connection = Some((surface, id));
    }

    fn setup_viewport_listeners(&self) {
        let obj = self.obj().clone();
        obj.property_expression("viewport")
            .chain_property::<crate::viewport::Viewport>("crop")
            .watch(gtk::Widget::NONE, move || obj.imp().resize());

        let obj = self.obj().clone();
        obj.property_expression("viewport")
            .chain_property::<crate::viewport::Viewport>("zoom")
            .watch(gtk::Widget::NONE, move || obj.imp().resize());
    }

    fn document(&self) -> crate::document::Document {
        self.viewport().document()
    }

    fn viewport(&self) -> crate::viewport::Viewport {
        self.viewport
            .borrow()
            .clone()
            .expect("a page has a viewport")
    }

    pub(super) fn resize(&self) {
        let Some(info) = self.page_info() else {
            return;
        };
        let page = self.obj().clone();
        let (w, h) = (info.width, info.height);

        self.resolve_bbox(
            &info,
            page.crop(),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move |bbox| {
                    let bbox = if page.crop() {
                        *bbox
                    } else {
                        Rectangle::new(0.0, 0.0, w, h)
                    };

                    imp.bbox.replace(bbox);
                    let (w, h) = bbox.size();
                    let (rw, rh) = ((w * page.zoom()) as i32, (h * page.zoom()) as i32);
                    if page.width_request() != rw {
                        log::debug!(
                            target: "scrolex::pan",
                            "page {} resize: width {} -> {rw}",
                            page.index(),
                            page.width_request(),
                        );
                    }
                    page.set_size_request(rw, rh);
                }
            ),
        );
    }

    // This widget's page index and size (points), from MuPDF. None until a document is loaded or if
    // the page can't be read.
    fn page_info(&self) -> Option<PageInfo> {
        let index = self.obj().index();
        let size = self.document().page_size(index).or_else(|| {
            crate::mupdf_render::page_size(&self.obj().uri(), index)
                .map(|(width, height)| crate::mupdf_render::PageSize { width, height })
        })?;
        Some(PageInfo {
            index,
            width: size.width,
            height: size.height,
        })
    }

    fn setup_text_selection(&self) {
        let obj = self.obj();
        let mouse_coords = Rc::new(RefCell::new(None));
        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        // indicates that we have "borrowed" global page cursor
        let cursor = Rc::new(Cell::new(false));

        gc.connect_pressed(clone!(
            #[strong]
            mouse_coords,
            #[strong(rename_to = page)]
            obj,
            #[weak(rename_to = imp)]
            self,
            #[strong]
            cursor,
            move |_gc, _n_press, x, y| {
                page.viewport().clear_selection();
                mouse_coords.replace(Some((x, y)));
                if !imp.cursor_guard.get() {
                    page.set_cursor_from_name(Some("text"));
                    imp.cursor_guard.set(true);
                    cursor.set(true);
                }
            }
        ));

        let obj = self.obj().clone();
        gc.connect_update(clone!(
            #[strong]
            mouse_coords,
            move |gc, seq| {
                let Some((start_x, start_y)) = *mouse_coords.borrow() else {
                    return;
                };
                let Some((end_x, end_y)) = gc.point(seq) else {
                    return;
                };

                let Point { x: x1, y: y1 } = undo_zoom_and_crop(&obj, start_x, start_y);
                let Point { x: x2, y: y2 } = undo_zoom_and_crop(&obj, end_x, end_y);

                let selection =
                    crate::selection::selection(&obj.uri(), obj.index(), (x1, y1), (x2, y2));
                match selection {
                    Some(sel) if !sel.rects.is_empty() => {
                        obj.viewport().set_selection(Some(PageSelection {
                            page: obj.index(),
                            rects: sel.rects.into_iter().map(Rectangle::from).collect(),
                            text: sel.text,
                        }));
                    }
                    _ => obj.viewport().clear_selection(),
                }
            }
        ));

        let obj = self.obj().clone();
        gc.connect_end(move |_, _| {
            // Primary selection only (middle-click paste); the clipboard needs an explicit copy.
            if let Some(text) = obj.viewport().selected_text() {
                obj.primary_clipboard().set_text(&text);
            }
            if Cell::get(&cursor) {
                cursor.set(false);
                obj.set_cursor(None);
                obj.imp().cursor_guard.set(false);
            }
        });

        self.obj().add_controller(gc);
    }

    fn setup_link_handling(&self) {
        let obj = self.obj();
        let motion_controller = gtk::EventControllerMotion::new();

        // indicates that we have "borrowed" global page cursor
        let cursor = Cell::new(false);
        let tooltip_visible = Rc::new(Cell::new(false));

        motion_controller.connect_motion(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            #[strong]
            tooltip_visible,
            move |_, x, y| {
                let target = imp.link_at(&obj, x, y);
                let visible = matches!(target, Some(LinkTarget::Location(_)));
                if tooltip_visible.replace(visible) != visible {
                    obj.set_tooltip_text(
                        visible.then_some("Middle click opens beside. Right click opens a menu."),
                    );
                }
                if target.is_some() {
                    if !imp.cursor_guard.get() {
                        obj.set_cursor_from_name(Some("pointer"));
                        imp.cursor_guard.set(true);
                        cursor.set(true);
                    }
                    return;
                }

                if Cell::get(&cursor) {
                    obj.set_cursor(None);
                    imp.cursor_guard.set(false);
                    cursor.set(false);
                }
            }
        ));
        motion_controller.connect_leave(clone!(
            #[strong]
            obj,
            #[strong]
            tooltip_visible,
            move |_| {
                if tooltip_visible.replace(false) {
                    obj.set_tooltip_text(None);
                }
            }
        ));
        obj.add_controller(motion_controller);

        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        gc.connect_pressed(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |gc, _n_press, x, y| {
                if let Some(link_target) = imp.link_at(&obj, x, y) {
                    match link_target {
                        LinkTarget::Location(location) => {
                            gc.set_state(gtk::EventSequenceState::Claimed);
                            let action = if gc
                                .current_event_state()
                                .contains(gtk::gdk::ModifierType::CONTROL_MASK)
                            {
                                LinkAction::OpenInNewTab
                            } else {
                                LinkAction::Open
                            };
                            Self::emit_link(&obj, location, action);
                        }
                        LinkTarget::Uri(uri) => Self::open_uri(&uri),
                    }
                };
            }
        ));
        obj.add_controller(gc);

        let middle_start = Rc::new(RefCell::new(None::<(f64, f64, LinkTarget)>));
        let middle = gtk::GestureClick::builder().button(2).build();
        middle.connect_pressed(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            #[strong]
            middle_start,
            move |_, _, x, y| {
                let target = imp.link_at(&obj, x, y);
                middle_start.replace(target.map(|target| (x, y, target)));
            }
        ));
        middle.connect_released(clone!(
            #[strong]
            obj,
            #[strong]
            middle_start,
            move |gesture, _, x, y| {
                let Some((start_x, start_y, target)) = middle_start.take() else {
                    return;
                };
                if (x - start_x).hypot(y - start_y) > 8.0 {
                    return;
                }
                match target {
                    LinkTarget::Location(location) => {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        Self::emit_link(&obj, location, LinkAction::OpenBeside);
                    }
                    LinkTarget::Uri(uri) => Self::open_uri(&uri),
                }
            }
        ));
        obj.add_controller(middle);

        let context = gtk::GestureClick::builder().button(3).build();
        context.connect_pressed(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |gesture, _, x, y| {
                let target = imp.link_at(&obj, x, y);
                let Some(LinkTarget::Location(location)) = target else {
                    return;
                };
                gesture.set_state(gtk::EventSequenceState::Claimed);
                let popover = gtk::Popover::new();
                let actions = gtk::Box::new(gtk::Orientation::Vertical, 0);
                for (label, action) in [
                    ("Open Link", LinkAction::Open),
                    ("Open Link Beside", LinkAction::OpenBeside),
                    ("Open Link in New Tab", LinkAction::OpenInNewTab),
                ] {
                    let button = gtk::Button::builder()
                        .label(label)
                        .has_frame(false)
                        .halign(gtk::Align::Fill)
                        .build();
                    button.connect_clicked(clone!(
                        #[strong]
                        obj,
                        #[weak]
                        popover,
                        move |_| {
                            popover.popdown();
                            Self::emit_link(&obj, location, action);
                        }
                    ));
                    actions.append(&button);
                }
                popover.set_child(Some(&actions));
                popover.set_parent(&obj);
                popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                    x.round() as i32,
                    y.round() as i32,
                    1,
                    1,
                )));
                popover.connect_closed(|popover| popover.unparent());
                popover.popup();
            }
        ));
        obj.add_controller(context);
    }

    fn link_at(&self, obj: &super::Page, x: f64, y: f64) -> Option<LinkTarget> {
        let point = undo_zoom_and_crop(obj, x, y);
        self.document()
            .imp()
            .links
            .borrow_mut()
            .get_link(&obj.uri(), obj.index(), point.x, point.y)
            .cloned()
    }

    fn emit_link(obj: &super::Page, location: crate::links::DocumentLocation, action: LinkAction) {
        let request = LinkRequest {
            source_page: obj.index(),
            location,
            action,
        };
        obj.emit_by_name::<()>("internal-link-clicked", &[&request]);
    }

    fn open_uri(uri: &str) {
        let _ = gtk::gio::AppInfo::launch_default_for_uri(uri, gtk::gio::AppLaunchContext::NONE);
    }

    fn get_bbox(&self, page: &PageInfo, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        let bbox = get_bbox(&self.obj().uri(), page, true);
        self.document()
            .bbox_cache()
            .borrow_mut()
            .insert(page.index, bbox);
        bbox
    }

    fn get_cached_bbox(&self, page: &PageInfo, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        Rectangle::new(0.0, 0.0, page.width, page.height)
    }

    // Resolve the page's bounding box and hand it to `cb`. Computed inline on the main thread and
    // cached per page: crop resolves it from a low-res render (cheaper than a full render, and it
    // sizes the widget at once). A pooled job would lag behind the renders during a fast scroll,
    // leaving the page stuck at its stale size until the box arrived.
    fn resolve_bbox<F>(&self, page: &PageInfo, crop: bool, cb: F)
    where
        F: FnOnce(&Rectangle) + 'static,
    {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            cb(&bbox);
            return;
        }

        let bbox = get_bbox(&self.obj().uri(), page, true);
        self.document()
            .bbox_cache()
            .borrow_mut()
            .insert(page.index, bbox);
        cb(&bbox);
    }

    fn lookup_bbox(&self, page: &PageInfo, crop: bool) -> Option<Rectangle> {
        if !crop {
            return Some(Rectangle::new(0.0, 0.0, page.width, page.height));
        }
        self.document()
            .bbox_cache()
            .borrow()
            .get(&page.index)
            .copied()
    }

    // Log when a page draws something different than last time. Getting worse while the zoom stays
    // the same is a flicker the reader sees. A new zoom throws the texture away on purpose, so the
    // zoom is compared too.
    fn note_paint(&self, page_num: i32, paint: Paint) {
        let zoom = self.obj().zoom();
        let prev = self.painted.replace(Some((page_num, paint, zoom)));
        let Some((prev_page, prev_paint, prev_zoom)) = prev else {
            return;
        };
        if prev_page != page_num || prev_paint == paint {
            return;
        }
        let regression = prev_zoom == zoom && paint.fidelity() < prev_paint.fidelity();
        log::debug!(
            target: "scrolex::flicker",
            "page {page_num}: {prev_paint:?} -> {paint:?} at zoom {zoom}{}",
            if regression { " DEGRADED" } else { "" },
        );
    }

    fn render_snapshot(&self, snapshot: &gtk::Snapshot, page: &PageInfo) {
        let start = std::time::Instant::now();
        let obj = self.obj();
        let dsf = device_scale(&*obj);

        let scale = obj.zoom();
        let render_scale = render_scale((page.width, page.height), scale, dsf);

        if render_scale < scale {
            // Viewport regions are rendered off the UI thread; entering this path also keeps later
            // page renders asynchronous so snapshots do not alternate rendering modes.
            obj.document().set_multithread_rendering(true);
            self.multithread_snapshot(snapshot, page);
            return;
        }
        self.tiled.set(false);
        obj.document()
            .render_cache()
            .borrow_mut()
            .unpin_page(obj.viewport().id(), page.index);
        let bbox = self.get_bbox(page, obj.crop());

        match render_page_texture(
            &obj.uri(),
            page.index,
            render_scale,
            dsf,
            Some((page.width, page.height)),
        ) {
            Some(texture) => {
                self.append_render(
                    snapshot,
                    texture.upcast_ref(),
                    page,
                    &bbox,
                    scale,
                    render_scale,
                );
            }
            None => {
                append_page_background(snapshot, &bbox, scale);
                self.note_paint(page.index, Paint::Blank);
            }
        }

        let elapsed = start.elapsed();
        log::debug!(
            "Rendered page {} [on-demand (visible), sync] on main thread in {elapsed:?} (device_scale={dsf})",
            page.index
        );

        if obj.document().record_main_thread_render(elapsed) {
            log::warn!(
                "Two of the last three main-thread renders exceeded 100 ms. Latest: {elapsed:?}. Switching to multithreading mode."
            );
            obj.document().set_multithread_rendering(true);
        }
    }

    // Draw a render: 1:1 with device pixels, or stretched if the caps held its scale below the zoom.
    fn append_render(
        &self,
        snapshot: &gtk::Snapshot,
        texture: &gtk::gdk::Texture,
        page: &PageInfo,
        bbox: &Rectangle,
        zoom: f64,
        render_scale: f64,
    ) {
        if render_scale < zoom {
            self.append_scaled_page_texture(snapshot, texture, page, bbox, zoom);
            self.note_paint(page.index, Paint::Upscaled);
        } else {
            self.append_page_texture(snapshot, texture, bbox, zoom);
            self.note_paint(page.index, Paint::Sharp);
        }
    }

    // Present the page texture 1:1 with device pixels, cropped to `bbox` (see page_footprint).
    fn append_page_texture(
        &self,
        snapshot: &gtk::Snapshot,
        texture: &gtk::gdk::Texture,
        bbox: &Rectangle,
        scale: f64,
    ) {
        let dsf = device_scale(&*self.obj());
        let ((ox, oy), (fw, fh)) =
            page_footprint((texture.width(), texture.height()), bbox, scale, dsf);
        let (bw, bh) = bbox.size();
        snapshot.push_clip(&graphene::Rect::new(
            0.0,
            0.0,
            (bw * scale) as f32,
            (bh * scale) as f32,
        ));
        snapshot.save();
        snapshot.translate(&graphene::Point::new(ox as f32, oy as f32));
        snapshot.append_texture(
            texture,
            &graphene::Rect::new(0.0, 0.0, fw as f32, fh as f32),
        );
        snapshot.restore();
        snapshot.pop();
    }

    // Stretch a texture of any scale over the page extent, cropped to `bbox`. Used for stand-ins
    // while a render is pending, and for capped renders.
    fn append_scaled_page_texture(
        &self,
        snapshot: &gtk::Snapshot,
        texture: &gtk::gdk::Texture,
        page: &PageInfo,
        bbox: &Rectangle,
        scale: f64,
    ) {
        let (bw, bh) = bbox.size();
        let clip = graphene::Rect::new(0.0, 0.0, (bw * scale) as f32, (bh * scale) as f32);
        snapshot.push_clip(&clip);
        snapshot.save();
        snapshot.translate(&graphene::Point::new(
            (-bbox.x1 * scale) as f32,
            (-bbox.y1 * scale) as f32,
        ));
        let full = graphene::Rect::new(
            0.0,
            0.0,
            (page.width * scale) as f32,
            (page.height * scale) as f32,
        );
        snapshot.append_texture(texture, &full);
        snapshot.restore();
        snapshot.pop();
    }

    // Fill this page's selection rects, using the same zoom/crop transform as the page render so they
    // land on the words.
    fn snapshot_selection_overlay(&self, snapshot: &gtk::Snapshot, page: &PageInfo) {
        let obj = self.obj();
        let selection = obj.viewport().selection();
        let selection = selection.borrow();
        let Some(selection) = selection
            .as_ref()
            .filter(|selection| selection.page == obj.index())
        else {
            return;
        };

        let bbox = self.get_bbox(page, obj.crop());
        let scale = obj.zoom();

        snapshot.save();
        overlay_transform(snapshot, &bbox, scale);
        let color = RGBA::new(0.5, 0.8, 0.9, 0.5);
        for rect in &selection.rects {
            let (w, h) = rect.size();
            snapshot.append_color(
                &color,
                &graphene::Rect::new(rect.x1 as f32, rect.y1 as f32, w as f32, h as f32),
            );
        }
        snapshot.restore();
    }

    // Paint match rects for this page: matches yellow, the current match orange. Same zoom/crop
    // transform as the page render, so highlights land on the words.
    fn snapshot_search_overlay(&self, snapshot: &gtk::Snapshot, page: &PageInfo) {
        let obj = self.obj();
        let index = obj.index();
        let search = obj.document().search();
        let search = search.borrow();
        let Some(matches) = search.results.get(&index) else {
            return;
        };
        if matches.is_empty() {
            return;
        }

        let bbox = self.get_bbox(page, obj.crop());
        let scale = obj.zoom();

        snapshot.save();
        overlay_transform(snapshot, &bbox, scale);

        // Each match may span multiple lines (one rect each); the current match is orange, others
        // yellow.
        for (i, rects) in matches.iter().enumerate() {
            let color = if obj.viewport().current_search_result() == Some((index, i)) {
                RGBA::new(1.0, 0.55, 0.0, 0.45)
            } else {
                RGBA::new(1.0, 0.9, 0.0, 0.4)
            };
            for rect in rects {
                let (w, h) = rect.size();
                snapshot.append_color(
                    &color,
                    &graphene::Rect::new(rect.x1 as f32, rect.y1 as f32, w as f32, h as f32),
                );
            }
        }
        snapshot.restore();
    }

    fn visible_tile_regions(
        &self,
        page: &PageInfo,
        bbox: &Rectangle,
        scale: f64,
        dsf: f64,
    ) -> Vec<crate::mupdf_render::PixelRect> {
        let obj = self.obj();
        let (bw, bh) = bbox.size();
        let mut visible = Rectangle::new(0.0, 0.0, bw * scale, bh * scale);
        let mut found_viewport = false;
        let mut ancestor = obj.parent();
        while let Some(widget) = ancestor {
            ancestor = widget.parent();
            let Ok(scroller) = widget.downcast::<gtk::ScrolledWindow>() else {
                continue;
            };
            let Some(origin) = obj.compute_point(&scroller, &graphene::Point::new(0.0, 0.0)) else {
                continue;
            };
            found_viewport = true;
            visible.x1 = visible.x1.max(-f64::from(origin.x()));
            visible.y1 = visible.y1.max(-f64::from(origin.y()));
            visible.x2 = visible
                .x2
                .min(f64::from(scroller.width()) - f64::from(origin.x()));
            visible.y2 = visible
                .y2
                .min(f64::from(scroller.height()) - f64::from(origin.y()));
        }

        if !found_viewport {
            let logical_tile = TILE_SIZE as f64 / dsf;
            visible.x2 = visible.x2.min(logical_tile);
            visible.y2 = visible.y2.min(logical_tile);
        }

        let (ox, oy) = page_offset(bbox, scale, dsf);
        tile_regions(
            render_dimensions((page.width, page.height), scale, dsf),
            (
                (visible.x1 - ox) * dsf,
                (visible.y1 - oy) * dsf,
                (visible.x2 - ox) * dsf,
                (visible.y2 - oy) * dsf,
            ),
        )
    }

    fn append_tile_texture(
        &self,
        snapshot: &gtk::Snapshot,
        texture: &gtk::gdk::Texture,
        region: crate::mupdf_render::PixelRect,
        page_px: (i32, i32),
        bbox: &Rectangle,
        density: (f64, f64),
    ) {
        let (scale, dsf) = density;
        let (ox, oy) = page_offset(bbox, scale, dsf);
        let pixels = raster_region(region, page_px);
        snapshot.push_clip(&graphene::Rect::new(
            (ox + region.x0 as f64 / dsf) as f32,
            (oy + region.y0 as f64 / dsf) as f32,
            ((region.x1 - region.x0) as f64 / dsf) as f32,
            ((region.y1 - region.y0) as f64 / dsf) as f32,
        ));
        snapshot.append_texture(
            texture,
            &graphene::Rect::new(
                (ox + pixels.x0 as f64 / dsf) as f32,
                (oy + pixels.y0 as f64 / dsf) as f32,
                (texture.width() as f64 / dsf) as f32,
                (texture.height() as f64 / dsf) as f32,
            ),
        );
        snapshot.pop();
    }

    fn tiled_snapshot(
        &self,
        snapshot: &gtk::Snapshot,
        page: &PageInfo,
        bbox: &Rectangle,
        scale: f64,
        dsf: f64,
    ) {
        let obj = self.obj();
        let page_num = page.index;
        let render = crate::render_cache::PageRenderKey::from_factors(page_num, scale, dsf);
        let page_px = render_dimensions((page.width, page.height), scale, dsf);
        let regions = self.visible_tile_regions(page, bbox, scale, dsf);
        let mut ready = Vec::new();
        let mut missing = Vec::new();
        let visible_ids: Vec<_> = regions
            .iter()
            .map(|region| crate::render_cache::TileId {
                render,
                x: region.x0,
                y: region.y0,
            })
            .collect();
        {
            let cache = obj.document().render_cache();
            let mut cache = cache.borrow_mut();
            cache.pin_tiles(obj.viewport().id(), render, &visible_ids);
            for (region, id) in regions.into_iter().zip(visible_ids.iter().copied()) {
                match cache.get_tile(id) {
                    Some(texture) => ready.push((region, texture)),
                    None => missing.push(region),
                }
            }
        }

        let full = obj
            .document()
            .render_cache()
            .borrow_mut()
            .get_latest(page_num);
        let preview = obj
            .document()
            .preview_cache()
            .borrow_mut()
            .get_latest(page_num);
        let source = fallback_source(
            full.as_ref().map(|texture| texture.width()),
            preview.as_ref().map(|texture| texture.width()),
        );
        let fallback = match source {
            FallbackSource::Render => full.as_ref(),
            FallbackSource::Preview => preview.as_ref(),
            FallbackSource::None => None,
        };
        if missing.is_empty() {
            // The cached render node can briefly outlive its viewport regions while GTK collects a
            // queued redraw. A solid page node keeps uncovered edges opaque until that redraw.
            append_page_background(snapshot, bbox, scale);
        } else if let Some(texture) = fallback {
            self.append_scaled_page_texture(snapshot, texture, page, bbox, scale);
        } else {
            let (w, h) = bbox.size();
            append_loading_placeholder(snapshot, w * scale, h * scale);
        }

        let (bw, bh) = bbox.size();
        snapshot.push_clip(&graphene::Rect::new(
            0.0,
            0.0,
            (bw * scale) as f32,
            (bh * scale) as f32,
        ));
        for (region, texture) in ready {
            self.append_tile_texture(snapshot, &texture, region, page_px, bbox, (scale, dsf));
        }
        snapshot.pop();

        if missing.is_empty() {
            self.note_paint(page_num, Paint::Sharp);
            self.prefetch_previews(page_num);
            return;
        }
        self.schedule_tile_render(page_num, scale, dsf, page_px, missing);
        self.note_paint(
            page_num,
            match source {
                FallbackSource::Render => Paint::StaleRender,
                FallbackSource::Preview => Paint::Preview,
                FallbackSource::None => Paint::Placeholder,
            },
        );

        let preview_target_width = ((page.width * obj.document().preview_scale()) as i32).max(1);
        if needs_visible_preview(
            full.as_ref().map(|texture| texture.width()),
            preview.is_some(),
            preview_target_width,
        ) {
            self.schedule_preview_if_needed(page_num, RenderPriority::VisiblePreview);
        }
    }

    fn multithread_snapshot(&self, snapshot: &gtk::Snapshot, page: &PageInfo) {
        let obj = self.obj();
        let page_num = page.index;

        let (width, height) = (page.width, page.height);
        let scale = obj.zoom();
        let dsf = device_scale(&*obj);
        let render_scale = render_scale((width, height), scale, dsf);
        let cached_bbox = self.get_cached_bbox(page, obj.crop());
        if render_scale < scale {
            self.tiled.set(true);
            self.tiled_snapshot(snapshot, page, &cached_bbox, scale, dsf);
            return;
        }
        self.tiled.set(false);
        let render = crate::render_cache::PageRenderKey::from_factors(page_num, render_scale, dsf);
        obj.document()
            .render_cache()
            .borrow_mut()
            .pin_page(obj.viewport().id(), render);
        let expected = render_dimensions((width, height), render_scale, dsf);
        let page_bytes = page_buffer_bytes((width, height), render_scale, dsf);

        let cache = obj.document().render_cache();
        let cached = {
            let mut cache = cache.borrow_mut();
            cache.get(render).or_else(|| cache.get_latest(page_num))
        };
        let stale_render = if let Some(texture) = cached {
            if (texture.width(), texture.height()) == expected {
                log::debug!("draw page {page_num}: cache hit");
                let bbox = self.get_bbox(page, obj.crop());
                self.append_render(snapshot, &texture, page, &bbox, scale, render_scale);
                self.prefetch_next(page_num, page_bytes as usize);
                self.prefetch_previews(page_num);
                return;
            }
            log::debug!("draw page {page_num}: cache stale");
            Some(texture)
        } else {
            None
        };

        // Flung-past pages are dropped at the queue (see set_wanted_pages), so this doesn't saturate
        // the workers mid-scroll.
        self.schedule_render(page_num, render_scale, dsf, RenderPriority::Visible);

        let preview = obj
            .document()
            .preview_cache()
            .borrow_mut()
            .get_latest(page_num);
        let source = fallback_source(
            stale_render.as_ref().map(|texture| texture.width()),
            preview.as_ref().map(|texture| texture.width()),
        );
        let fallback = match source {
            FallbackSource::Render => {
                log::debug!("draw page {page_num}: showing scaled full render");
                stale_render.as_ref()
            }
            FallbackSource::Preview => {
                log::debug!("draw page {page_num}: showing preview");
                preview.as_ref()
            }
            FallbackSource::None => None,
        };
        if let Some(texture) = fallback {
            self.append_scaled_page_texture(snapshot, texture, page, &cached_bbox, scale);
            self.note_paint(
                page_num,
                match source {
                    FallbackSource::Preview => Paint::Preview,
                    _ => Paint::StaleRender,
                },
            );
        } else {
            log::debug!("draw page {page_num}: cache miss (loading placeholder)");
            let (w, h) = cached_bbox.size();
            append_loading_placeholder(snapshot, w * scale, h * scale);
            self.note_paint(page_num, Paint::Placeholder);
        }

        // Request a stand-in for this page. Start look-ahead work after the full render lands.
        let preview_target_width = ((page.width * obj.document().preview_scale()) as i32).max(1);
        if needs_visible_preview(
            stale_render.as_ref().map(|texture| texture.width()),
            preview.is_some(),
            preview_target_width,
        ) {
            self.schedule_preview_if_needed(page_num, RenderPriority::VisiblePreview);
        }
    }

    // Full-render pages ahead in the scroll direction so reading on lands on a cached page. Skips
    // cached/queued pages; lowest priority, dropped at the queue if the scroll leaves its range.
    // `page_bytes` is the current page's render size, which bounds the depth.
    fn prefetch_next(&self, current: i32, page_bytes: usize) {
        let obj = self.obj();
        let viewport = obj.viewport();
        let document = viewport.document();
        let n_pages = document.n_pages();
        if n_pages == 0 {
            return;
        }
        let dir = if viewport.scroll_forward() { 1 } else { -1 };
        let scale = obj.zoom();
        let dsf = device_scale(&*obj);
        let cache = document.render_cache();

        let visible = viewport.visible_page_count().max(1) as usize;
        let budget = cache.borrow().budget_bytes();
        let ahead = prefetch_depth(document.render_threads(), visible, page_bytes, budget) as i32;

        // farthest first so the LIFO queue pops the nearest ahead-page first
        for d in (1..=ahead).rev() {
            let page_num = current + dir * d;
            if page_num < 0 || page_num >= n_pages {
                continue;
            }
            self.schedule_render(page_num, scale, dsf, RenderPriority::Prefetch);
        }
    }

    // Queue one page and scale. Equal requests share the job and keep separate viewport interests.
    fn schedule_render(&self, page_num: i32, scale: f64, dsf: f64, priority: RenderPriority) {
        let obj = self.obj();
        let uri = obj.uri();
        // Page size (points) from the main-thread doc, so the worker sizes its pixel buffer to
        // exactly what the render cache expects (see mupdf_render::render_page_pixels).
        let page_pt = crate::mupdf_render::page_size(&uri, page_num);

        // Apply the buffer cap before the cache and job lookups.
        let scale = match page_pt {
            Some(size) => render_scale(size, scale, dsf),
            None => scale,
        };
        let render = crate::render_cache::PageRenderKey::from_factors(page_num, scale, dsf);
        if obj.document().render_cache().borrow().contains(render) {
            return;
        }

        let key = crate::document::RenderJobKey::Page(render);
        let waiter = matches!(priority, RenderPriority::Visible).then_some(&*obj);
        let Some(demand) = obj
            .document()
            .request_render(key.clone(), &obj.viewport(), waiter)
        else {
            return;
        };

        log::trace!("Scheduling render of page {page_num}");

        let (resp_sender, resp_receiver) = oneshot::channel::<RenderedPixels>();
        let document = obj.document();
        let doc_epoch = document.doc_epoch();
        let completion_demand = demand.clone();
        let completion_document = document.clone();
        let completion_key = key.clone();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let Some((rendered, waiters)) = accept_render(
                &completion_document,
                &completion_key,
                &completion_demand,
                doc_epoch,
                result,
            ) else {
                return;
            };

            let texture = rendered.into_texture();
            completion_document
                .render_cache()
                .borrow_mut()
                .insert(render, texture.upcast());
            finish_render(&completion_document, page_num, waiters);
        });

        let uri_job = uri.clone();
        let worker_demand = demand.clone();
        RENDER_QUEUE.with(move |queue| {
            queue.submit(
                &uri,
                document.id().raw(),
                demand,
                page_num,
                priority,
                Box::new(move || {
                    if worker_demand.is_empty() {
                        return;
                    }
                    request_render(
                        &uri_job,
                        scale,
                        dsf,
                        page_num,
                        priority,
                        page_pt,
                        resp_sender,
                    );
                }),
            );
        });
    }

    // Queue one viewport batch. One worker records and replays the page serially, then publishes the
    // complete batch; region identities remain independent in the cache and painter.
    fn schedule_tile_render(
        &self,
        page_num: i32,
        scale: f64,
        dsf: f64,
        page_px: (i32, i32),
        regions: Vec<crate::mupdf_render::PixelRect>,
    ) {
        if regions.is_empty() {
            return;
        }
        let obj = self.obj();
        let uri = obj.uri();
        let render = crate::render_cache::PageRenderKey::from_factors(page_num, scale, dsf);
        let mut tiles: Vec<_> = regions
            .iter()
            .map(|region| crate::render_cache::TileId {
                render,
                x: region.x0,
                y: region.y0,
            })
            .collect();
        tiles.sort_unstable();
        let key = crate::document::RenderJobKey::Tiles(tiles);
        let Some(demand) = obj
            .document()
            .request_render(key.clone(), &obj.viewport(), Some(&obj))
        else {
            return;
        };

        let document = obj.document();
        let doc_epoch = document.doc_epoch();
        let (resp_sender, resp_receiver) = oneshot::channel::<Vec<RenderedRegion>>();
        let completion_demand = demand.clone();
        let completion_document = document.clone();
        let completion_key = key.clone();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let Some((rendered, waiters)) = accept_render(
                &completion_document,
                &completion_key,
                &completion_demand,
                doc_epoch,
                result,
            ) else {
                return;
            };

            let textures = rendered
                .into_iter()
                .map(|region| {
                    let id = crate::render_cache::TileId {
                        render,
                        x: region.x,
                        y: region.y,
                    };
                    (id, region.pixels.into_texture().upcast())
                })
                .collect();
            completion_document
                .render_cache()
                .borrow_mut()
                .insert_tile_batch(textures);
            finish_render(&completion_document, page_num, waiters);
        });

        let uri_job = uri.clone();
        let worker_demand = demand.clone();
        RENDER_QUEUE.with(move |queue| {
            queue.submit(
                &uri,
                document.id().raw(),
                demand,
                page_num,
                RenderPriority::Visible,
                Box::new(move || {
                    if worker_demand.is_empty() {
                        return;
                    }
                    request_region_render(
                        &uri_job,
                        scale,
                        dsf,
                        page_num,
                        page_px,
                        regions,
                        resp_sender,
                    );
                }),
            );
        });
    }

    // Prefetch low-res previews over a symmetric window (they're cheap and tiny), so scrolling
    // either way shows blurry pages instead of blank ones.
    fn prefetch_previews(&self, current: i32) {
        let obj = self.obj();
        if !obj.document().preview_enabled() {
            return;
        }
        let n_pages = obj.document().n_pages();
        if n_pages == 0 {
            return;
        }
        let window = preview_window(obj.document().preview_cache().borrow().page_capacity());

        // Walk outward from the visible page, interleaving both directions, and push so the nearest
        // pages end up on top of the LIFO queue (rendered first). Pages already cached - typically
        // the side scrolled from - are skipped, so effort tracks the direction of travel.
        let mut candidates = Vec::with_capacity(2 * window as usize);
        for d in (1..=window).rev() {
            candidates.push(current + d);
            candidates.push(current - d);
        }
        for page_num in candidates {
            if page_num >= 0 && page_num < n_pages {
                self.schedule_preview_if_needed(page_num, RenderPriority::Preview);
            }
        }
    }

    // Queue this page's preview unless it's cached, already queued, or the preview budget of
    // in-flight jobs is full. `priority` is VisiblePreview for the page on screen (render its blur
    // before anything else) and Preview for the look-ahead window.
    fn schedule_preview_if_needed(&self, page_num: i32, priority: RenderPriority) {
        let obj = self.obj();
        let document = obj.document();
        if !document.preview_enabled() || document.preview_cache().borrow().contains_page(page_num)
        {
            return;
        }
        if document.preview_inflight().borrow().len() >= MAX_INFLIGHT_PREVIEWS {
            return;
        }
        if document.preview_inflight().borrow_mut().insert(page_num) {
            self.schedule_preview(page_num, priority);
        }
    }

    fn schedule_preview(&self, page_num: i32, priority: RenderPriority) {
        let obj = self.obj();
        let uri = obj.uri();
        let document = obj.document();
        let demand = crate::bg_job::RenderDemand::from_client(obj.viewport().id().raw());
        let scale = obj.document().preview_scale();
        let page_pt = crate::mupdf_render::page_size(&uri, page_num);

        let (resp_sender, resp_receiver) = oneshot::channel::<RenderedPixels>();
        let obj_clone = obj.clone();
        // Previews survive viewport zoom. A document load invalidates them through doc_epoch.
        let doc_epoch = obj.document().doc_epoch();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let document = obj_clone.document();

            if document.doc_epoch() != doc_epoch {
                return;
            }
            document.preview_inflight().borrow_mut().remove(&page_num);

            let Ok(rendered) = result else {
                return;
            };

            // decode-bound documents (e.g. scanned images) don't get cheaper as the scale shrinks:
            // once several previews in a row are slow at the floor they never will pay off - stop
            // making new ones. A one-off slow page just bumps the streak; a cheap preview clears it.
            // Keep the already-rendered previews cached either way - they're valid placeholders.
            let cur_scale = document.preview_scale();
            if rendered.render_ms > PREVIEW_SLOW_MS && cur_scale <= PREVIEW_MIN_SCALE {
                let streak = document.preview_slow_streak() + 1;
                document.set_preview_slow_streak(streak);
                if streak >= PREVIEW_SLOW_STREAK_LIMIT {
                    log::debug!(
                        "preview page {page_num} took {}ms (>{PREVIEW_SLOW_MS}) at min scale, {streak}x in a row; disabling previews",
                        rendered.render_ms
                    );
                    document.set_preview_enabled(false);
                    document.preview_inflight().borrow_mut().clear();
                    return;
                }
            } else {
                document.set_preview_slow_streak(0);
            }

            // steer the scale for future previews toward the time and memory budgets, based on what
            // this render (at cur_scale) actually cost
            let bytes = (rendered.stride * rendered.height).max(0) as usize;
            let new_scale = adapt_preview_scale(cur_scale, rendered.render_ms, bytes);
            if new_scale != cur_scale {
                log::debug!(
                    "preview scale {cur_scale:.3} -> {new_scale:.3} (page {page_num}: {}ms, {}KB)",
                    rendered.render_ms,
                    bytes / 1024
                );
                document.set_preview_scale(new_scale);
            }

            let texture = rendered.into_texture();
            document.preview_cache().borrow_mut().insert(
                crate::render_cache::PageRenderKey::from_factors(page_num, scale, 1.0),
                texture.upcast(),
            );

            // Repaint full-render waiters. Keep them for the full-render result.
            for widget in document
                .render_waiters(page_num)
                .into_iter()
                .filter_map(|widget| widget.upgrade())
            {
                if widget.index() == page_num {
                    widget.queue_draw();
                }
            }
        });

        let uri_job = uri.clone();
        let worker_demand = demand.clone();
        RENDER_QUEUE.with(move |queue| {
            queue.submit(
                &uri,
                document.id().raw(),
                demand,
                page_num,
                priority,
                Box::new(move || {
                    if worker_demand.is_empty() {
                        return;
                    }
                    request_render(
                        &uri_job,
                        scale,
                        1.0,
                        page_num,
                        priority,
                        page_pt,
                        resp_sender,
                    );
                }),
            );
        });
    }
}

fn page_background() -> RGBA {
    let paper = crate::mupdf_render::page_background_rgb();
    RGBA::new(
        f32::from(paper[0]) / 255.0,
        f32::from(paper[1]) / 255.0,
        f32::from(paper[2]) / 255.0,
        1.0,
    )
}

// Fallback when a page can't be rendered.
fn append_page_background(snapshot: &gtk::Snapshot, bbox: &Rectangle, scale: f64) {
    let (w, h) = bbox.size();
    snapshot.append_color(
        &page_background(),
        &graphene::Rect::new(0.0, 0.0, (w * scale) as f32, (h * scale) as f32),
    );
}

// Crop offset snapped to the device-pixel grid + 1:1 logical footprint (texture px / device scale).
// Off-grid or scaled placement makes the GPU resample the texture and blur it.
fn page_footprint(
    tex_px: (i32, i32),
    bbox: &Rectangle,
    scale: f64,
    dsf: f64,
) -> ((f64, f64), (f64, f64)) {
    (
        page_offset(bbox, scale, dsf),
        (tex_px.0 as f64 / dsf, tex_px.1 as f64 / dsf),
    )
}

fn page_offset(bbox: &Rectangle, scale: f64, dsf: f64) -> (f64, f64) {
    let snap = |v: f64| (v * dsf).round() / dsf;
    (snap(-bbox.x1 * scale), snap(-bbox.y1 * scale))
}

// Zoom + crop offset, so overlay rects in page points land on the render.
fn overlay_transform(snapshot: &gtk::Snapshot, bbox: &Rectangle, scale: f64) {
    if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
        snapshot.translate(&graphene::Point::new(
            (-bbox.x1 * scale) as f32,
            (-bbox.y1 * scale) as f32,
        ));
    }
    snapshot.scale(scale as f32, scale as f32);
}

// Synchronous main-thread render, used before the background pipeline engages.
fn render_page_texture(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    page_pt: Option<(f64, f64)>,
) -> Option<MemoryTexture> {
    if let Some(cfg) = crate::emulate::config() {
        let (data, width, height, stride) =
            crate::emulate::pixels(cfg, page_num, scale, dsf, false);
        return Some(texture_from_raw(data, width, height, stride));
    }
    let px = crate::mupdf_render::render_page_pixels(uri, page_num, scale, dsf, page_pt)?;
    Some(texture_from_raw(px.data, px.width, px.height, px.stride))
}

// Pixels are cairo Rgb24 (BGRx); the x8 format ignores the padding byte.
fn texture_from_raw(data: Vec<u8>, width: i32, height: i32, stride: i32) -> MemoryTexture {
    let bytes = glib::Bytes::from_owned(data);
    MemoryTexture::new(
        width,
        height,
        MemoryFormat::B8g8r8x8,
        &bytes,
        stride as usize,
    )
}

// Steer the preview render scale toward two budgets at once, from what the last preview render (at
// `cur_scale`) actually cost: a per-preview time budget (keep the stand-in fast) and a per-preview
// size budget (keep the whole window resident so previews don't thrash their cache). Both costs
// grow ~scale^2, so each budget maps to a scale by the same square-root correction; we take the
// tighter of the two and clamp to the usable range.
fn adapt_preview_scale(cur_scale: f64, render_ms: u128, bytes: usize) -> f64 {
    // Per-preview size ceiling: the cache budget is this times the resident-preview count, so
    // steering each preview toward this size keeps that many resident.
    let target_bytes = crate::document::PREVIEW_TARGET_BYTES as f64;

    let render_ms = render_ms.max(1) as f64;
    let bytes = bytes.max(1) as f64;

    let scale_time = cur_scale * (PREVIEW_TARGET_MS as f64 / render_ms).sqrt();
    let scale_mem = cur_scale * (target_bytes / bytes).sqrt();

    scale_time
        .min(scale_mem)
        .clamp(PREVIEW_MIN_SCALE, PREVIEW_MAX_SCALE)
}

// Cairo node because it draws text; rare enough to stay off the scroll hot path.
fn append_loading_placeholder(snapshot: &gtk::Snapshot, width: f64, height: f64) {
    let paper = crate::mupdf_render::page_background_rgb().map(|v| f64::from(v) / 255.0);
    let text = crate::mupdf_render::loading_text_rgb().map(|v| f64::from(v) / 255.0);
    let cr = snapshot.append_cairo(&graphene::Rect::new(0.0, 0.0, width as f32, height as f32));
    cr.rectangle(0.0, 0.0, width, height);
    cr.set_source_rgb(paper[0], paper[1], paper[2]);
    cr.fill().expect("Failed to fill");

    let label = "Loading …";
    let font_size = (width.min(height) * 0.06).clamp(14.0, 40.0);
    cr.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
    cr.set_font_size(font_size);
    if let Ok(extents) = cr.text_extents(label) {
        let x = (width - extents.width()) / 2.0 - extents.x_bearing();
        let y = (height - extents.height()) / 2.0 - extents.y_bearing();
        cr.move_to(x, y);
        cr.set_source_rgb(text[0], text[1], text[2]);
        let _ = cr.show_text(label);
    }
}

// Raw rendered pixels cross the thread boundary as a plain buffer because GDK textures are not
// `Send`; the main thread creates the texture.
#[derive(Debug)]
struct RenderedPixels {
    data: Box<[u8]>,
    width: i32,
    height: i32,
    stride: i32,
    render_ms: u128,
}

struct RenderedRegion {
    x: i32,
    y: i32,
    pixels: RenderedPixels,
}

impl RenderedPixels {
    fn into_texture(self) -> MemoryTexture {
        texture_from_raw(self.data.into_vec(), self.width, self.height, self.stride)
    }
}

fn request_render(
    uri: &str,
    scale: f64,
    device_scale_factor: f64,
    page_num: i32,
    priority: RenderPriority,
    page_pt: Option<(f64, f64)>,
    resp_sender: oneshot::Sender<RenderedPixels>,
) {
    let start = std::time::Instant::now();
    if let Some(cfg) = crate::emulate::config() {
        let (data, width, height, stride) = crate::emulate::pixels(
            cfg,
            page_num,
            scale,
            device_scale_factor,
            priority.is_preview(),
        );
        let render_ms = start.elapsed().as_millis();
        log::debug!(
            "Rendered page {page_num} [{}] on background thread in {render_ms}ms (device_scale={device_scale_factor})",
            priority.label()
        );
        let _ = resp_sender.send(RenderedPixels {
            data: data.into_boxed_slice(),
            width,
            height,
            stride,
            render_ms,
        });
        return;
    }
    let pixels =
        crate::mupdf_render::render_page_pixels(uri, page_num, scale, device_scale_factor, page_pt);
    let render_ms = start.elapsed().as_millis();
    log::debug!(
        "Rendered page {page_num} [{}] on background thread in {render_ms}ms (device_scale={device_scale_factor})",
        priority.label()
    );

    // Send the raw buffer; the texture is built from it on the main thread.
    let rendered = match pixels {
        Some(px) => RenderedPixels {
            data: px.data.into_boxed_slice(),
            width: px.width,
            height: px.height,
            stride: px.stride,
            render_ms,
        },
        None => {
            log::warn!("mupdf render failed for page {page_num}; showing blank");
            blank_rendered_page(page_pt, scale, device_scale_factor, render_ms)
        }
    };
    // ignore send failure: the receiver is gone if the page's widget was
    // dropped or its render superseded
    let _ = resp_sender.send(rendered);
}

fn request_region_render(
    uri: &str,
    scale: f64,
    device_scale_factor: f64,
    page_num: i32,
    page_px: (i32, i32),
    regions: Vec<crate::mupdf_render::PixelRect>,
    resp_sender: oneshot::Sender<Vec<RenderedRegion>>,
) {
    let start = std::time::Instant::now();
    let raster_regions: Vec<_> = regions
        .iter()
        .copied()
        .map(|region| raster_region(region, page_px))
        .collect();
    let pixels = if let Some(cfg) = crate::emulate::config() {
        std::thread::sleep(std::time::Duration::from_millis(cfg.full_ms));
        let page_width = ((cfg.page_pt.0 * scale * device_scale_factor) as i32).max(1);
        let page_height = ((cfg.page_pt.1 * scale * device_scale_factor) as i32).max(1);
        Some(
            raster_regions
                .iter()
                .map(|region| {
                    let (data, width, height, stride) = crate::emulate::region_pixels(
                        page_num,
                        page_width,
                        page_height,
                        region.x0,
                        region.y0,
                        region.x1 - region.x0,
                        region.y1 - region.y0,
                    );
                    crate::mupdf_render::PagePixels {
                        data,
                        width,
                        height,
                        stride,
                    }
                })
                .collect(),
        )
    } else {
        crate::mupdf_render::render_page_regions(
            uri,
            page_num,
            scale,
            device_scale_factor,
            &raster_regions,
        )
    };
    let render_ms = start.elapsed().as_millis();
    log::debug!(
        "Rendered {} regions of page {page_num} [{}] serially on background thread in {render_ms}ms (device_scale={device_scale_factor})",
        regions.len(),
        RenderPriority::Visible.label(),
    );

    let rendered = match pixels {
        Some(pixels) => regions
            .into_iter()
            .zip(pixels)
            .map(|(region, px)| RenderedRegion {
                x: region.x0,
                y: region.y0,
                pixels: RenderedPixels {
                    data: px.data.into_boxed_slice(),
                    width: px.width,
                    height: px.height,
                    stride: px.stride,
                    render_ms,
                },
            })
            .collect(),
        None => {
            log::warn!("mupdf region render failed for page {page_num}; showing blank");
            regions
                .into_iter()
                .zip(raster_regions)
                .map(|(region, pixels)| RenderedRegion {
                    x: region.x0,
                    y: region.y0,
                    pixels: blank_rendered_region(pixels, render_ms),
                })
                .collect()
        }
    };
    let _ = resp_sender.send(rendered);
}

fn blank_rendered_region(
    region: crate::mupdf_render::PixelRect,
    render_ms: u128,
) -> RenderedPixels {
    let width = region.x1 - region.x0;
    let height = region.y1 - region.y0;
    let stride = gtk::cairo::Format::Rgb24
        .stride_for_width(width as u32)
        .expect("stride");
    RenderedPixels {
        data: solid_page_data(stride, height, crate::mupdf_render::page_background_rgb()),
        width,
        height,
        stride,
        render_ms,
    }
}

// Blank page for a failed render: dimensions and stride match a real render at this scale, so
// the render cache's dimension check passes instead of looping on the miss.
fn blank_rendered_page(
    page_pt: Option<(f64, f64)>,
    scale: f64,
    dsf: f64,
    render_ms: u128,
) -> RenderedPixels {
    let (w, h) = page_pt.unwrap_or((1.0, 1.0));
    let width = ((w * scale * dsf) as i32).max(1);
    let height = ((h * scale * dsf) as i32).max(1);
    let stride = gtk::cairo::Format::Rgb24
        .stride_for_width(width as u32)
        .expect("stride");
    let data = solid_page_data(stride, height, crate::mupdf_render::page_background_rgb());
    RenderedPixels {
        data,
        width,
        height,
        stride,
        render_ms,
    }
}

fn solid_page_data(stride: i32, height: i32, color: [u8; 3]) -> Box<[u8]> {
    let mut data = vec![0xffu8; (stride * height) as usize];
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel[..3].copy_from_slice(&[color[2], color[1], color[0]]);
    }
    data.into_boxed_slice()
}

// Accept a result only for the same document epoch and active job.
fn accept_render<T, E>(
    document: &crate::document::Document,
    key: &crate::document::RenderJobKey,
    demand: &crate::bg_job::RenderDemand,
    doc_epoch: u64,
    result: Result<T, E>,
) -> Option<(T, Vec<glib::WeakRef<crate::page::Page>>)> {
    if document.doc_epoch() != doc_epoch {
        return None;
    }
    let job = document.take_render_job(key, demand)?;
    let waiters = job
        .interests
        .into_values()
        .filter_map(|interest| interest.widget)
        .collect();
    match result {
        Ok(rendered) => Some((rendered, waiters)),
        Err(_) => {
            redraw_waiters(key.page(), waiters);
            None
        }
    }
}

// Log cache state and repaint each valid waiter.
fn finish_render(
    document: &crate::document::Document,
    page_num: i32,
    waiters: Vec<glib::WeakRef<crate::page::Page>>,
) {
    log::debug!(
        "memory: rss={:.0}MB preview_scale={:.3} render_cache={:?} preview_cache={:?}",
        current_rss_mb(),
        document.preview_scale(),
        document.render_cache().borrow(),
        document.preview_cache().borrow(),
    );
    redraw_waiters(page_num, waiters);
}

fn redraw_waiters(page_num: i32, waiters: Vec<glib::WeakRef<crate::page::Page>>) {
    for widget in waiters.into_iter().filter_map(|widget| widget.upgrade()) {
        if widget.index() == page_num {
            widget.queue_draw();
        }
    }
}

// Resident set size in MB, read from /proc (Linux). Used only for diagnostic logging, so a read
// failure just reports 0.
fn current_rss_mb() -> f64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0.0;
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            if let Ok(kb) = rest.trim().trim_end_matches("kB").trim().parse::<f64>() {
                return kb / 1024.0;
            }
        }
    }
    0.0
}

struct Point {
    x: f64,
    y: f64,
}

fn undo_zoom_and_crop(page: &super::Page, x: f64, y: f64) -> Point {
    let mut x = x / page.zoom();
    let mut y = y / page.zoom();

    if page.crop() {
        x += page.imp().bbox.borrow().x1;
        y += page.imp().bbox.borrow().y1;
    }

    Point { x, y }
}

// A page's index and size in points - the page facts the widget needs, sourced from MuPDF instead
// of holding a live page object.
struct PageInfo {
    index: i32,
    width: f64,
    height: f64,
}

pub(crate) fn crop_box(uri: &str, index: i32, width: f64, height: f64) -> Rectangle {
    get_bbox(
        uri,
        &PageInfo {
            index,
            width,
            height,
        },
        true,
    )
}

fn get_bbox(uri: &str, page: &PageInfo, crop: bool) -> Rectangle {
    if !crop {
        return Rectangle::new(0.0, 0.0, page.width, page.height);
    }
    // MuPDF's content bbox is page-local top-left points, same convention as our Rectangle. Fall
    // back to the full page if it can't be resolved.
    match crate::mupdf_render::content_bbox(uri, page.index) {
        Some((x1, y1, x2, y2)) => {
            apply_crop(Rectangle::new(x1, y1, x2, y2), page.width, page.height)
        }
        None => Rectangle::new(0.0, 0.0, page.width, page.height),
    }
}

// Crop the left and right margins only. Grow the content box by a 5pt margin, enforce a half-page
// minimum width, and clamp to the page. Pure geometry so the crop behaviour is tested without a
// rendering backend.
fn apply_crop(content: Rectangle, width: f64, height: f64) -> Rectangle {
    let x1 = content.x1 - 5.0;
    let mut x2 = content.x2 + 5.0;
    if x2 - x1 < width / 2.0 {
        x2 = x1 + width / 2.0;
    }
    Rectangle::new(x1.max(0.0), 0.0, x2.min(width), height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn prefetch_depth_bounds() {
        // 8 pages fit the budget in these cases; spare threads run ahead, capped so the visible
        // pages + 1 headroom stay in the cache
        let mb = 1024 * 1024;
        assert_eq!(prefetch_depth(11, 3, mb, 8 * mb), 4); // min(11-3, 8-4)
        assert_eq!(prefetch_depth(4, 1, mb, 8 * mb), 3); // threads-bound, cache has room
        assert_eq!(prefetch_depth(11, 3, 0, 8 * mb), 8); // page size unknown: thread-bound
        assert_eq!(prefetch_depth(11, 7, mb, 8 * mb), 0); // no room left: don't evict visible pages
        assert_eq!(prefetch_depth(2, 3, mb, 8 * mb), 0); // more visible than threads

        // deep zoom: one page fills the budget, so nothing is rendered ahead
        assert_eq!(prefetch_depth(11, 1, 100 * mb, 64 * mb), 0);
    }

    #[test]
    fn page_buffer_bytes_stays_measurable_at_extreme_sizes() {
        // A4 at 100%, device scale 2
        assert_eq!(page_buffer_bytes((595.0, 842.0), 1.0, 2.0), 8_015_840.0);
        // sizes no integer product could hold must still read as over the cap, never wrap under it
        assert!(page_buffer_bytes((f64::MAX, f64::MAX), 10.0, 3.0) > MAX_PAGE_BYTES);
        assert!(page_buffer_bytes((1e9, 1e9), 10.0, 3.0) > MAX_PAGE_BYTES);
        // degenerate sizes measure as nothing rather than going negative
        assert_eq!(page_buffer_bytes((0.0, 0.0), 1.0, 1.0), 0.0);
        assert_eq!(page_buffer_bytes((-5.0, 10.0), 1.0, 1.0), 0.0);
    }

    #[test]
    fn render_scale_follows_the_zoom_until_the_buffer_cap() {
        let a4 = (595.0, 842.0);
        assert_eq!(render_scale(a4, 1.0, 1.0), 1.0);
        assert_eq!(render_scale(a4, 4.0, 2.0), 4.0);

        // deep zoom on a normal page, and plain 100% on a huge canvas: both render below the zoom
        for (page_pt, zoom, dsf) in [(a4, 10.0, 2.0), ((6120.0, 7920.0), 1.0, 1.0)] {
            let scale = render_scale(page_pt, zoom, dsf);
            assert!(scale < zoom, "{page_pt:?} at {zoom}x{dsf}: {scale}");
            assert!(page_buffer_bytes(page_pt, scale, dsf) <= MAX_PAGE_BYTES);
        }

        // a long thin page hits the axis limit, not the byte cap
        let banner = (200.0, 40000.0);
        let scale = render_scale(banner, 10.0, 1.0);
        assert!(render_dimensions(banner, scale, 1.0).1 <= MAX_TEXTURE_DIM as i32);

        // degenerate sizes: no divide by zero, zoom passes through
        assert_eq!(render_scale((0.0, 0.0), 3.0, 1.0), 3.0);
        assert_eq!(render_scale((f64::NAN, 100.0), 3.0, 1.0), 3.0);
        assert_eq!(render_scale((f64::MAX, f64::MAX), 3.0, 1.0), 3.0);
    }

    #[test]
    fn render_dimensions_match_the_full_render_buffer() {
        assert_eq!(render_dimensions((595.0, 842.0), 1.0, 2.0), (1190, 1684));
        assert_eq!(render_dimensions((0.0, 0.0), 1.0, 1.0), (1, 1));
    }

    #[test]
    fn tile_regions_follow_a_page_anchored_pixel_grid() {
        let regions = tile_regions((2500, 1800), (900.5, 700.0, 2200.2, 1700.0));
        assert_eq!(
            regions,
            vec![
                crate::mupdf_render::PixelRect::new(0, 0, 1024, 1024),
                crate::mupdf_render::PixelRect::new(1024, 0, 2048, 1024),
                crate::mupdf_render::PixelRect::new(2048, 0, 2500, 1024),
                crate::mupdf_render::PixelRect::new(0, 1024, 1024, 1800),
                crate::mupdf_render::PixelRect::new(1024, 1024, 2048, 1800),
                crate::mupdf_render::PixelRect::new(2048, 1024, 2500, 1800),
            ]
        );
    }

    #[test]
    fn tile_regions_clip_to_the_page_and_reject_an_empty_view() {
        assert_eq!(
            tile_regions((1500, 900), (1024.0, 0.0, 5000.0, 900.0)),
            vec![crate::mupdf_render::PixelRect::new(1024, 0, 1500, 900)]
        );
        assert!(tile_regions((1500, 900), (1600.0, 0.0, 1700.0, 100.0)).is_empty());
    }

    #[test]
    fn tile_rasters_include_a_clipped_sampling_gutter() {
        assert_eq!(
            raster_region(
                crate::mupdf_render::PixelRect::new(1024, 1024, 2048, 1800),
                (2500, 1800),
            ),
            crate::mupdf_render::PixelRect::new(1023, 1023, 2049, 1800),
        );
        assert_eq!(
            raster_region(
                crate::mupdf_render::PixelRect::new(0, 0, 1024, 1024),
                (2500, 1800),
            ),
            crate::mupdf_render::PixelRect::new(0, 0, 1025, 1025),
        );
    }

    #[test]
    fn fallback_uses_the_higher_resolution_texture() {
        assert_eq!(
            fallback_source(Some(1000), Some(250)),
            FallbackSource::Render
        );
        assert_eq!(
            fallback_source(Some(100), Some(250)),
            FallbackSource::Preview
        );
        assert_eq!(fallback_source(Some(100), None), FallbackSource::Render);
        assert_eq!(fallback_source(None, Some(250)), FallbackSource::Preview);
        assert_eq!(fallback_source(None, None), FallbackSource::None);
    }

    #[test]
    fn coarse_render_requests_a_missing_visible_preview() {
        assert!(needs_visible_preview(Some(100), false, 250));
        assert!(!needs_visible_preview(Some(300), false, 250));
        assert!(!needs_visible_preview(Some(100), true, 250));
        assert!(needs_visible_preview(None, false, 250));
    }

    #[test]
    fn preview_window_fits_cache() {
        // both directions must fit: 2 * window <= capacity, so no scheduled preview evicts another
        assert_eq!(preview_window(0), PREVIEW_WINDOW); // unknown size: full window
        assert_eq!(preview_window(43), 21); // big pages: clamp to capacity/2 (no thrash)
        assert_eq!(preview_window(1), 1); // room for almost nothing, still make progress
        assert_eq!(preview_window(1000), PREVIEW_WINDOW); // tiny pages: capped at full window
    }

    #[test]
    fn page_footprint_maps_texture_one_to_one() {
        // Texture dimensions preserve one-to-one placement after render_dimensions truncates each axis.
        let a4 = (595.0, 842.0);
        for dsf in [1.0, 1.5, 2.0, 2.25] {
            let tex = render_dimensions(a4, 1.0, dsf);
            let bbox = Rectangle::new(0.0, 0.0, a4.0, a4.1);
            let ((ox, oy), (fw, fh)) = page_footprint(tex, &bbox, 1.0, dsf);
            assert_eq!((ox, oy), (0.0, 0.0), "dsf={dsf}");
            assert!((fw * dsf - f64::from(tex.0)).abs() < EPSILON, "dsf={dsf}");
            assert!((fh * dsf - f64::from(tex.1)).abs() < EPSILON, "dsf={dsf}");
        }
    }

    #[test]
    fn page_footprint_snaps_crop_offset_to_device_grid() {
        // Off-grid crop margins blur the texture.
        let cropped = Rectangle::new(3.3, 2.6, 400.0, 600.0);
        assert_eq!(
            page_footprint((800, 1200), &cropped, 1.0, 1.0).0,
            (-3.0, -3.0)
        );
        assert_eq!(
            page_footprint((800, 1200), &cropped, 1.0, 2.0),
            ((-3.5, -2.5), (400.0, 600.0))
        );
        assert_eq!(
            page_footprint((800, 1200), &cropped, 1.0, 1.5).0,
            (-5.0 / 1.5, -4.0 / 1.5)
        );
    }

    const EPSILON: f64 = 0.0001;

    const SMALL_RENDERABLE_PDF: &[u8] = b"%PDF-1.1
%\xc2\xa5\xc2\xb1\xc3\xab

1 0 obj
  << /Type /Catalog
     /Pages 2 0 R
  >>
endobj

2 0 obj
  << /Type /Pages
     /Kids [3 0 R]
     /Count 1
     /MediaBox [0 0 80 12]
  >>
endobj

3 0 obj
  <<  /Type /Page
      /Parent 2 0 R
      /Resources
       << /Font
           << /F1
               << /Type /Font
                  /Subtype /Type1
                  /BaseFont /Times-Roman
               >>
           >>
       >>
      /Contents 4 0 R
  >>
endobj

4 0 obj
  << /Length 55 >>
stream
  BT
    /F1 18 Tf
    0 0 Td
    (Hello World) Tj
  ET
endstream
endobj

xref
0 5
0000000000 65535 f
0000000018 00000 n
0000000077 00000 n
0000000178 00000 n
0000000457 00000 n
trailer
  <<  /Root 1 0 R
      /Size 5
  >>
startxref
565
%%EOF";

    #[test]
    fn test_get_bbox_no_crop() {
        // crop=false returns the full page without consulting the render backend (uri unused).
        let page = PageInfo {
            index: 0,
            width: 250.0,
            height: 50.0,
        };
        let bbox = get_bbox("", &page, false);
        assert!((bbox.x1 - 0.0).abs() < EPSILON);
        assert!((bbox.y1 - 0.0).abs() < EPSILON);
        assert!((bbox.x2 - 250.0).abs() < EPSILON);
        assert!((bbox.y2 - 50.0).abs() < EPSILON);
    }

    // note_paint compares these to decide DEGRADED, so the order must stay this way.
    #[test]
    fn paint_fidelity_ranks_sharp_highest_and_blank_lowest() {
        let ranked = [
            Paint::Blank,
            Paint::Placeholder,
            Paint::Preview,
            Paint::StaleRender,
            Paint::Sharp,
        ];
        for pair in ranked.windows(2) {
            assert!(
                pair[0].fidelity() < pair[1].fidelity(),
                "{:?} should rank below {:?}",
                pair[0],
                pair[1],
            );
        }
    }

    #[test]
    fn failed_render_buffer_uses_page_color_in_bgrx_order() {
        let data = solid_page_data(8, 2, [0x12, 0x34, 0x56]);
        assert_eq!(
            data.as_ref(),
            [
                0x56, 0x34, 0x12, 0xff, 0x56, 0x34, 0x12, 0xff, 0x56, 0x34, 0x12, 0xff, 0x56, 0x34,
                0x12, 0xff,
            ]
        );
    }

    // The crop math is pure geometry over a content box (whatever backend produced it), so it's
    // tested directly. Page is 250x50.
    #[test]
    fn apply_crop_adds_margin() {
        let r = apply_crop(Rectangle::new(50.0, 15.0, 200.0, 40.0), 250.0, 50.0);
        assert!((r.x1 - 45.0).abs() < EPSILON);
        assert!((r.x2 - 205.0).abs() < EPSILON);
    }

    #[test]
    fn apply_crop_keeps_full_height() {
        let r = apply_crop(Rectangle::new(50.0, 15.0, 200.0, 40.0), 250.0, 50.0);
        assert!((r.y1 - 0.0).abs() < EPSILON);
        assert!((r.y2 - 50.0).abs() < EPSILON);
    }

    #[test]
    fn apply_crop_enforces_half_page_min() {
        // tiny content grows to at least half the page width
        let r = apply_crop(Rectangle::new(9.5, 6.0, 20.0, 8.0), 250.0, 50.0);
        assert!((r.x1 - 4.5).abs() < EPSILON);
        assert!((r.x2 - 129.5).abs() < EPSILON); // 4.5 + 250/2
    }

    #[test]
    fn apply_crop_clamps_to_page() {
        // margins pushing past the edges clamp back to [0,w]
        let r = apply_crop(Rectangle::new(2.0, 2.0, 248.0, 48.0), 250.0, 50.0);
        assert!((r.x1 - 0.0).abs() < EPSILON);
        assert!((r.x2 - 250.0).abs() < EPSILON);
    }

    // Two pages, the second far larger. At a zoom the first page renders fine, the second is capped.
    const MIXED_SIZE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n\
4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 2000 3000] >>\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    #[gtk::test]
    fn schedule_render_caps_the_scale_of_an_oversized_page() {
        let dir = std::env::temp_dir().join("scrolex_schedule_cap_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mixed.pdf");
        std::fs::write(&path, MIXED_SIZE_PDF).unwrap();
        let uri = crate::test_support::file_uri(&path);

        let document = crate::document::Document::new();
        document.set_uri(uri);
        document.set_n_pages(2);
        let page = crate::page::Page::new(&crate::viewport::Viewport::new(&document));

        // page 1 is 2000x3000pt. At zoom 10 a whole-page buffer would be ~2.4GB, so it renders
        // capped instead of not at all.
        page.imp()
            .schedule_render(1, 10.0, 1.0, RenderPriority::Prefetch);
        let capped = render_scale((2000.0, 3000.0), 10.0, 1.0);
        let key = crate::document::RenderJobKey::Page(
            crate::render_cache::PageRenderKey::from_factors(1, capped, 1.0),
        );
        assert!(document.has_render_job(&key));

        // already cached at the capped scale: don't render it again on every draw
        document.clear_render_jobs();
        let (w, h) = render_dimensions((2000.0, 3000.0), capped, 1.0);
        let bytes = glib::Bytes::from_owned(vec![255u8; (w * h * 4) as usize]);
        let texture = MemoryTexture::new(w, h, MemoryFormat::B8g8r8x8, &bytes, (w * 4) as usize);
        document.render_cache().borrow_mut().insert(
            crate::render_cache::PageRenderKey::from_factors(1, capped, 1.0),
            texture.upcast(),
        );
        page.imp()
            .schedule_render(1, 10.0, 1.0, RenderPriority::Prefetch);
        assert!(!document.has_render_job(&key));
    }

    #[gtk::test]
    fn test_render() {
        // MuPDF opens by path, so write the fixture to a temp file, then render page 0 and assert
        // it produced a non-blank surface (exact pixels are backend-specific, so no snapshot).
        let dir = std::env::temp_dir().join("scrolex_test_render");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.pdf");
        std::fs::write(&path, SMALL_RENDERABLE_PDF).unwrap();
        let uri = crate::test_support::file_uri(&path);

        let surface = crate::mupdf_render::render_page_surface(&uri, 0, 1.0, 1.0, None)
            .expect("mupdf should render the fixture");
        assert!(surface.width() > 0 && surface.height() > 0);

        let mut colored = false;
        surface
            .with_data(|d| {
                colored = d
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255)
            })
            .unwrap();
        assert!(colored, "rendered surface is blank white");
    }

    // Throughput probe: measures how many pages/sec the renderer sustains at
    // various thread counts. Ignored by default (needs a real PDF); run with:
    //   PDF_PATH=/abs/file.pdf cargo test --release bench_render_throughput -- --ignored --nocapture
    // Optional env: PAGE_NUMBER (start page), PAGES (how many to render).
    #[test]
    #[ignore]
    fn bench_render_throughput() {
        let path = env::var("PDF_PATH").expect("PDF_PATH not set");
        let uri = crate::test_support::file_uri(&path);
        let start: i32 = env::var("PAGE_NUMBER")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let count: i32 = env::var("PAGES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40);
        let scale: f64 = env::var("SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0);
        let pages: Vec<i32> = (start..start + count).collect();

        for threads in [1usize, 2, 4, 8] {
            let t0 = std::time::Instant::now();
            std::thread::scope(|s| {
                for t in 0..threads {
                    let uri = uri.clone();
                    let chunk: Vec<i32> = pages.iter().copied().skip(t).step_by(threads).collect();
                    if chunk.is_empty() {
                        continue;
                    }
                    s.spawn(move || {
                        for p in chunk {
                            if let Some(surface) =
                                crate::mupdf_render::render_page_surface(&uri, p, scale, 1.0, None)
                            {
                                std::hint::black_box(surface);
                            }
                        }
                    });
                }
            });
            let dt = t0.elapsed();
            println!(
                "scale={scale} threads={threads:<2} pages={} time={dt:>8.2?} throughput={:.1} pages/s",
                pages.len(),
                pages.len() as f64 / dt.as_secs_f64()
            );
        }
    }

    #[test]
    fn preview_scale_shrinks_for_slow_renders() {
        // a vector page rendered well over budget at 0.25 should drop toward hitting the time
        // budget (cost ~scale^2, so sqrt(40/160) = 0.5x)
        let scale = adapt_preview_scale(0.25, 160, 100_000);
        assert!((scale - 0.125).abs() < EPSILON, "got {scale}");
    }

    #[test]
    fn preview_scale_floors_at_min_for_very_slow_renders() {
        let scale = adapt_preview_scale(0.25, 5_000, 100_000);
        assert!((scale - PREVIEW_MIN_SCALE).abs() < EPSILON, "got {scale}");
    }

    #[test]
    fn preview_scale_caps_at_max_when_both_budgets_are_slack() {
        // cheap and small: time budget wants a big scale, memory budget allows it -> clamp to max
        let scale = adapt_preview_scale(0.25, 8, 50_000);
        assert!((scale - PREVIEW_MAX_SCALE).abs() < EPSILON, "got {scale}");
    }

    #[test]
    fn preview_scale_memory_budget_caps_a_cheap_but_fat_render() {
        // fast render (time budget alone would push to max) but a large surface: the memory budget
        // must pull the scale below max so the resident window still fits the cache
        let scale = adapt_preview_scale(0.25, 4, 100_000);
        assert!(
            scale < PREVIEW_MAX_SCALE,
            "memory budget should bind: got {scale}"
        );
        // sqrt((20MB/65) / 100KB) * 0.25 ~= 0.449
        assert!((scale - 0.449).abs() < 0.01, "got {scale}");
    }

    #[test]
    fn preview_scale_handles_zero_measurements() {
        // a render measured as 0ms / 0 bytes must not divide by zero; both budgets read as slack
        let scale = adapt_preview_scale(0.25, 0, 0);
        assert!((scale - PREVIEW_MAX_SCALE).abs() < EPSILON, "got {scale}");
    }
}
