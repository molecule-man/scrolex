// Page widget rendering, preview scheduling, and document interaction.
#![expect(unused_lifetimes)]

use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
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
use crate::links::LinkTarget;
use crate::selection::PageSelection;

// Max bytes in one page buffer. A whole page is rendered at once, so the buffer grows with the
// scale squared. render_scale keeps it under this.
pub(crate) const MAX_PAGE_BYTES: f64 = 128.0 * 1024.0 * 1024.0;
// Max pixels per axis. GPUs commonly refuse a texture wider or taller than this. A long thin page
// can pass this while still under MAX_PAGE_BYTES.
const MAX_TEXTURE_DIM: f64 = 16384.0;

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

pub(crate) fn set_wanted_pages(client: u64, range: Option<(i32, i32)>) {
    RENDER_QUEUE.with(|queue| queue.set_wanted(client, range));
}

// Drop a window's queued full renders (zoom invalidates their scale; previews survive).
pub(crate) fn clear_full_renders(client: u64) {
    RENDER_QUEUE.with(|queue| queue.clear_full(client));
}

// Drop all of a window's queued renders, previews included (document switch / window close).
pub(crate) fn clear_all_renders(client: u64) {
    RENDER_QUEUE.with(|queue| queue.clear_all(client));
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
    #[property(get, set)]
    state: RefCell<crate::state::State>,

    #[property(get, set)]
    pub(crate) binding: RefCell<Option<glib::Binding>>,

    #[property(get, set)]
    index: Cell<i32>,

    bbox: RefCell<Rectangle>,
    cursor_guard: Cell<bool>,

    // false until the widget has been mapped and its final device scale factor is in effect.
    // Rendering before then would use a provisional scale factor (the compositor assigns the real
    // one right after map) and be thrown away and re-rendered - expensive on HiDPI. While false,
    // the page paints blank.
    scale_known: Cell<bool>,

    // last snapshot's (page index, paint, zoom); see note_paint
    painted: Cell<Option<(i32, Paint, f64)>>,
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
        self.setup_state_listeners();
        self.setup_text_selection();
        self.setup_link_handling();

        self.obj().set_size_request(600, 800);
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![Signal::builder("named-link-clicked")
                .param_types([i32::static_type()])
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
                snapshot.append_color(&white(), &graphene::Rect::new(0.0, 0.0, w, h));
            }
            self.note_paint(page.index, Paint::Blank);
            return;
        }

        if self.obj().state().multithread_rendering() {
            self.multithread_snapshot(snapshot, &page);
        } else {
            self.render_snapshot(snapshot, &page);
        }

        self.snapshot_selection_overlay(snapshot, &page);
        self.snapshot_search_overlay(snapshot, &page);
    }
}

impl Page {
    // Mark the device scale factor as known once it has settled after map, so rendering starts at
    // the final scale.
    fn setup_scale_tracking(&self) {
        let obj = self.obj();

        // The compositor assigns the surface's scale factor right after map, so
        // defer one main-loop iteration before allowing the first render; by
        // then the scale-factor notification (higher priority than idle) has
        // been applied. Recycled list widgets keep the flag set across remaps.
        obj.connect_map(|page| {
            // recycled list widgets keep the flag set across remaps; only the
            // genuine first map needs to defer
            if page.imp().scale_known.get() {
                return;
            }
            glib::idle_add_local_once(clone!(
                #[weak]
                page,
                move || {
                    page.imp().scale_known.set(true);
                    page.queue_draw();
                }
            ));
        });

        // A scale-factor change (e.g. moving to a monitor with a different
        // scale) is authoritative: the current cached texture is now stale, and
        // the snapshot's dimension check re-renders it at the new scale.
        obj.connect_scale_factor_notify(|page| {
            page.imp().scale_known.set(true);
            page.queue_draw();
        });
    }

    fn setup_state_listeners(&self) {
        let obj = self.obj().clone();
        obj.property_expression("state")
            .chain_property::<crate::state::State>("crop")
            .watch(gtk::Widget::NONE, move || obj.imp().resize());

        let obj = self.obj().clone();
        obj.property_expression("state")
            .chain_property::<crate::state::State>("zoom")
            .watch(gtk::Widget::NONE, move || obj.imp().resize());
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
        let (width, height) = crate::mupdf_render::page_size(&self.obj().uri(), index)?;
        Some(PageInfo {
            index,
            width,
            height,
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
                page.state().clear_selection();
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
                        obj.state().set_selection(Some(PageSelection {
                            page: obj.index(),
                            rects: sel.rects.into_iter().map(Rectangle::from).collect(),
                            text: sel.text,
                        }));
                    }
                    _ => obj.state().clear_selection(),
                }
            }
        ));

        let obj = self.obj().clone();
        gc.connect_end(move |_, _| {
            // Primary selection only (middle-click paste); the clipboard needs an explicit copy.
            if let Some(text) = obj.state().selected_text() {
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

        motion_controller.connect_motion(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |_, x, y| {
                let Point { x, y } = undo_zoom_and_crop(&obj, x, y);
                if imp
                    .state
                    .borrow()
                    .imp()
                    .links
                    .borrow_mut()
                    .get_link(&obj.uri(), obj.index(), x, y)
                    .is_some()
                {
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
        obj.add_controller(motion_controller);

        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        gc.connect_pressed(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |gc, _n_press, x, y| {
                let Point { x, y } = undo_zoom_and_crop(&obj, x, y);

                if let Some(link_target) = imp.state.borrow().imp().links.borrow_mut().get_link(
                    &obj.uri(),
                    obj.index(),
                    x,
                    y,
                ) {
                    match link_target {
                        LinkTarget::Page(page_num) => {
                            gc.set_state(gtk::EventSequenceState::Claimed); // stop the event from propagating
                            obj.emit_by_name::<()>("named-link-clicked", &[page_num]);
                        }
                        LinkTarget::Uri(uri) => {
                            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                                uri,
                                gtk::gio::AppLaunchContext::NONE,
                            );
                        }
                    }
                };
            }
        ));
        obj.add_controller(gc);
    }

    fn get_bbox(&self, page: &PageInfo, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        let bbox = get_bbox(&self.obj().uri(), page, true);
        self.state
            .borrow()
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
        self.state
            .borrow()
            .bbox_cache()
            .borrow_mut()
            .insert(page.index, bbox);
        cb(&bbox);
    }

    fn lookup_bbox(&self, page: &PageInfo, crop: bool) -> Option<Rectangle> {
        if !crop {
            return Some(Rectangle::new(0.0, 0.0, page.width, page.height));
        }
        self.state
            .borrow()
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
        let scale_factor = obj.scale_factor() as f64;

        let bbox = self.get_bbox(page, obj.crop());
        let scale = obj.zoom();
        let render_scale = render_scale((page.width, page.height), scale, scale_factor);

        match render_page_texture(
            &obj.uri(),
            page.index,
            render_scale,
            scale_factor,
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
                append_white(snapshot, &bbox, scale);
                self.note_paint(page.index, Paint::Blank);
            }
        }

        let elapsed = start.elapsed();
        log::debug!(
            "Rendered page {} [on-demand (visible), sync] on main thread in {elapsed:?} (scale_factor={scale_factor})",
            page.index
        );

        if elapsed > std::time::Duration::from_millis(100) {
            log::warn!("Rendering took too long: {elapsed:?}. Switching to multithreading mode.");
            obj.state().set_multithread_rendering(true);
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
        let dsf = self.obj().scale_factor().max(1) as f64;
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
        let selection = obj.state().selection();
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
        let search = obj.state().search();
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
            let color = if search.current == Some((index, i)) {
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

    fn multithread_snapshot(&self, snapshot: &gtk::Snapshot, page: &PageInfo) {
        let obj = self.obj();
        let page_num = page.index;

        let (width, height) = (page.width, page.height);
        let scale = obj.zoom();
        let scale_factor = obj.scale_factor() as f64;
        let render_scale = render_scale((width, height), scale, scale_factor);
        let expected = render_dimensions((width, height), render_scale, scale_factor);
        let page_bytes = page_buffer_bytes((width, height), render_scale, scale_factor);

        let cache = obj.state().render_cache();
        let cached = cache.borrow_mut().get(page_num);
        let stale_render = if let Some(texture) = cached {
            if (texture.width(), texture.height()) == expected {
                log::debug!("draw page {page_num}: cache hit");
                let bbox = self.get_bbox(page, obj.crop());
                self.append_render(snapshot, &texture, page, &bbox, scale, render_scale);
                self.prefetch_previews(page_num);
                self.prefetch_next(page_num, page_bytes as usize);
                return;
            }
            log::debug!("draw page {page_num}: cache stale");
            Some(texture)
        } else {
            None
        };

        // Flung-past pages are dropped at the queue (see set_wanted_pages), so this doesn't saturate
        // the workers mid-scroll.
        self.schedule_render(
            page_num,
            render_scale,
            scale_factor,
            RenderPriority::Visible,
        );

        // remember that this widget is the one waiting for page_num, so the
        // render repaints it when it lands
        obj.state()
            .render_waiters()
            .borrow_mut()
            .insert(page_num, obj.downgrade());

        let bbox = self.get_cached_bbox(page, obj.crop());
        let preview = obj.state().preview_cache().borrow_mut().get(page_num);
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
            self.append_scaled_page_texture(snapshot, texture, page, &bbox, scale);
            self.note_paint(
                page_num,
                match source {
                    FallbackSource::Preview => Paint::Preview,
                    _ => Paint::StaleRender,
                },
            );
        } else {
            log::debug!("draw page {page_num}: cache miss (loading placeholder)");
            let (w, h) = bbox.size();
            append_loading_placeholder(snapshot, w * scale, h * scale);
            self.note_paint(page_num, Paint::Placeholder);
        }

        // Prefetch low-resolution textures for surrounding pages. Request one for this page only
        // when no preview is cached and the completed texture is below the preview target.
        self.prefetch_previews(page_num);
        let preview_target_width = ((page.width * obj.state().preview_scale()) as i32).max(1);
        if needs_visible_preview(
            stale_render.as_ref().map(|texture| texture.width()),
            preview.is_some(),
            preview_target_width,
        ) {
            self.schedule_preview_if_needed(page_num, RenderPriority::VisiblePreview);
        }
        self.prefetch_next(page_num, page_bytes as usize);
    }

    // Full-render pages ahead in the scroll direction so reading on lands on a cached page. Skips
    // cached/queued pages; lowest priority, dropped at the queue if the scroll leaves its range.
    // `page_bytes` is the current page's render size, which bounds the depth.
    fn prefetch_next(&self, current: i32, page_bytes: usize) {
        let obj = self.obj();
        let state = obj.state();
        let n_pages = state.n_pages();
        if n_pages == 0 {
            return;
        }
        let dir = if state.scroll_forward() { 1 } else { -1 };
        let scale = obj.zoom();
        let scale_factor = obj.scale_factor() as f64;
        let cache = state.render_cache();

        let visible = state.visible_page_count().max(1) as usize;
        let budget = cache.borrow().budget_bytes();
        let ahead = prefetch_depth(state.render_threads(), visible, page_bytes, budget) as i32;

        // farthest first so the LIFO queue pops the nearest ahead-page first
        for d in (1..=ahead).rev() {
            let page_num = current + dir * d;
            if page_num < 0 || page_num >= n_pages {
                continue;
            }
            self.schedule_render(page_num, scale, scale_factor, RenderPriority::Prefetch);
        }
    }

    // Queue a full render of `page_num`. Skipped if one is in flight, or if the cache holds the page
    // at the capped scale already - callers pass the zoom, so only this point knows that scale. The
    // marker records the epoch, so the completion can tell whether the slot is still its own.
    fn schedule_render(
        &self,
        page_num: i32,
        scale: f64,
        scale_factor: f64,
        priority: RenderPriority,
    ) {
        let obj = self.obj();
        // Cheap bail before reading page bounds: a page redrawn while its render runs comes back here
        // on every snapshot.
        if obj
            .state()
            .render_inflight()
            .borrow()
            .contains_key(&page_num)
        {
            return;
        }

        let uri = obj.uri();
        // Page size (points) from the main-thread doc, so the worker sizes its pixel buffer to
        // exactly what the render cache expects (see mupdf_render::render_page_pixels).
        let page_pt = crate::mupdf_render::page_size(&uri, page_num);

        // Capped and skipped before the marker goes in. A page marked in flight with no render to
        // release it would stay wedged.
        let scale = match page_pt {
            Some(size) => render_scale(size, scale, scale_factor),
            None => scale,
        };
        let pixel_scale = scale * scale_factor;
        if obj
            .state()
            .render_cache()
            .borrow()
            .contains_at_scale(page_num, pixel_scale)
        {
            return;
        }

        let epoch = obj.state().render_epoch();
        match obj.state().render_inflight().borrow_mut().entry(page_num) {
            Entry::Occupied(_) => return,
            Entry::Vacant(slot) => {
                slot.insert(epoch);
            }
        }

        let client = obj.state().render_client_id();
        log::trace!("Scheduling render of page {page_num}");

        let (resp_sender, resp_receiver) = oneshot::channel::<RenderedPage>();
        let obj_clone = obj.clone();
        let doc_epoch = obj.state().doc_epoch();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let state = obj_clone.state();

            // This window loaded/reloaded a document since this was scheduled? That path already
            // cleared the inflight/waiter entries, so bail before touching the current ones: mutating
            // them here would drop the live render's marker/waiter. Per-window, so another window's
            // load never invalidates this render.
            if state.doc_epoch() != doc_epoch {
                return;
            }
            // release the slot this render holds, freeing the page for a render at the current scale
            {
                let inflight = state.render_inflight();
                let mut inflight = inflight.borrow_mut();
                if inflight.get(&page_num) == Some(&epoch) {
                    inflight.remove(&page_num);
                }
            }

            // Zoom moved on while this rendered, or the request was dropped (over-cap, or out of the
            // wanted range): discard the pixels and redraw the widget still on this page so it
            // reschedules at the current scale. The index check means a scrolled-off page stays
            // dropped.
            let rendered = match result {
                Ok(rendered) if state.render_epoch() == epoch => rendered,
                _ => {
                    if let Some(widget) = state
                        .render_waiters()
                        .borrow()
                        .get(&page_num)
                        .and_then(glib::WeakRef::upgrade)
                    {
                        if widget.index() == page_num {
                            widget.queue_draw();
                        }
                    }
                    return;
                }
            };

            let texture = rendered.into_texture();
            state
                .render_cache()
                .borrow_mut()
                .insert(page_num, texture.upcast(), pixel_scale);

            log::debug!(
                "memory: rss={:.0}MB preview_scale={:.3} render_cache={:?} preview_cache={:?}",
                current_rss_mb(),
                state.preview_scale(),
                state.render_cache().borrow(),
                state.preview_cache().borrow(),
            );

            // repaint whichever widget is currently waiting to show this page
            // (not necessarily the one that requested the render)
            if let Some(widget) = state
                .render_waiters()
                .borrow_mut()
                .remove(&page_num)
                .and_then(|weak| weak.upgrade())
            {
                if widget.index() == page_num {
                    widget.queue_draw();
                }
            }
        });

        let uri_job = uri.clone();
        RENDER_QUEUE.with(move |queue| {
            queue.submit(
                &uri,
                client,
                page_num,
                priority,
                Box::new(move || {
                    request_render(
                        &uri_job,
                        scale,
                        scale_factor,
                        page_num,
                        priority,
                        page_pt,
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
        if !obj.state().preview_enabled() {
            return;
        }
        let n_pages = obj.state().n_pages();
        if n_pages == 0 {
            return;
        }
        let window = preview_window(obj.state().preview_cache().borrow().page_capacity());

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
        let state = obj.state();
        if !state.preview_enabled() || state.preview_cache().borrow().contains(page_num) {
            return;
        }
        if state.preview_inflight().borrow().len() >= MAX_INFLIGHT_PREVIEWS {
            return;
        }
        if state.preview_inflight().borrow_mut().insert(page_num) {
            self.schedule_preview(page_num, priority);
        }
    }

    fn schedule_preview(&self, page_num: i32, priority: RenderPriority) {
        let obj = self.obj();
        let uri = obj.uri();
        let client = obj.state().render_client_id();
        let scale = obj.state().preview_scale();
        let page_pt = crate::mupdf_render::page_size(&uri, page_num);

        let (resp_sender, resp_receiver) = oneshot::channel::<RenderedPage>();
        let obj_clone = obj.clone();
        // Previews survive a zoom (they're rescaled at draw), so only a document load invalidates
        // them - check doc_epoch, not render_epoch. Per-window, so another window's load can't wedge
        // this preview's inflight marker.
        let doc_epoch = obj.state().doc_epoch();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let state = obj_clone.state();

            if state.doc_epoch() != doc_epoch {
                return;
            }
            state.preview_inflight().borrow_mut().remove(&page_num);

            let Ok(rendered) = result else {
                return;
            };

            // decode-bound documents (e.g. scanned images) don't get cheaper as the scale shrinks:
            // once several previews in a row are slow at the floor they never will pay off - stop
            // making new ones. A one-off slow page just bumps the streak; a cheap preview clears it.
            // Keep the already-rendered previews cached either way - they're valid placeholders.
            let cur_scale = state.preview_scale();
            if rendered.render_ms > PREVIEW_SLOW_MS && cur_scale <= PREVIEW_MIN_SCALE {
                let streak = state.preview_slow_streak() + 1;
                state.set_preview_slow_streak(streak);
                if streak >= PREVIEW_SLOW_STREAK_LIMIT {
                    log::debug!(
                        "preview page {page_num} took {}ms (>{PREVIEW_SLOW_MS}) at min scale, {streak}x in a row; disabling previews",
                        rendered.render_ms
                    );
                    state.set_preview_enabled(false);
                    state.preview_inflight().borrow_mut().clear();
                    return;
                }
            } else {
                state.set_preview_slow_streak(0);
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
                state.set_preview_scale(new_scale);
            }

            let texture = rendered.into_texture();
            state
                .preview_cache()
                .borrow_mut()
                .insert(page_num, texture.upcast(), scale);

            // repaint the waiting widget, but leave the waiter registered so the
            // full render still repaints it when it lands
            if let Some(widget) = state
                .render_waiters()
                .borrow()
                .get(&page_num)
                .and_then(glib::WeakRef::upgrade)
            {
                if widget.index() == page_num {
                    widget.queue_draw();
                }
            }
        });

        let uri_job = uri.clone();
        RENDER_QUEUE.with(move |queue| {
            queue.submit(
                &uri,
                client,
                page_num,
                priority,
                Box::new(move || {
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

fn white() -> RGBA {
    RGBA::new(1.0, 1.0, 1.0, 1.0)
}

// Fallback when a page can't be rendered.
fn append_white(snapshot: &gtk::Snapshot, bbox: &Rectangle, scale: f64) {
    let (w, h) = bbox.size();
    snapshot.append_color(
        &white(),
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
    let snap = |v: f64| (v * dsf).round() / dsf;
    (
        (snap(-bbox.x1 * scale), snap(-bbox.y1 * scale)),
        (tex_px.0 as f64 / dsf, tex_px.1 as f64 / dsf),
    )
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
    let target_bytes = crate::state::PREVIEW_TARGET_BYTES as f64;

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
    let cr = snapshot.append_cairo(&graphene::Rect::new(0.0, 0.0, width as f32, height as f32));
    cr.rectangle(0.0, 0.0, width, height);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.fill().expect("Failed to fill");

    let label = "Loading …";
    let font_size = (width.min(height) * 0.06).clamp(14.0, 40.0);
    cr.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
    cr.set_font_size(font_size);
    if let Ok(extents) = cr.text_extents(label) {
        let x = (width - extents.width()) / 2.0 - extents.x_bearing();
        let y = (height - extents.height()) / 2.0 - extents.y_bearing();
        cr.move_to(x, y);
        cr.set_source_rgb(0.6, 0.6, 0.6);
        let _ = cr.show_text(label);
    }
}

// A rendered page as raw pixels. Rendering happens on a background thread, and GDK textures aren't
// `Send`, so the pixels cross the thread boundary as a plain buffer and the texture is built on the
// main thread.
#[derive(Debug)]
struct RenderedPage {
    data: Box<[u8]>,
    width: i32,
    height: i32,
    stride: i32,
    render_ms: u128,
}

impl RenderedPage {
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
    resp_sender: oneshot::Sender<RenderedPage>,
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
            "Rendered page {page_num} [{}] on background thread in {render_ms}ms (scale_factor={device_scale_factor})",
            priority.label()
        );
        let _ = resp_sender.send(RenderedPage {
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
        "Rendered page {page_num} [{}] on background thread in {render_ms}ms (scale_factor={device_scale_factor})",
        priority.label()
    );

    // Send the raw buffer; the texture is built from it on the main thread.
    let rendered = match pixels {
        Some(px) => RenderedPage {
            data: px.data.into_boxed_slice(),
            width: px.width,
            height: px.height,
            stride: px.stride,
            render_ms,
        },
        None => {
            log::warn!("mupdf render failed for page {page_num}; showing blank");
            white_rendered_page(page_pt, scale, device_scale_factor, render_ms)
        }
    };
    // ignore send failure: the receiver is gone if the page's widget was
    // dropped or its render superseded
    let _ = resp_sender.send(rendered);
}

// Blank white page for a failed render: dimensions and stride match a real render at this scale, so
// the render cache's dimension check passes instead of looping on the miss.
fn white_rendered_page(
    page_pt: Option<(f64, f64)>,
    scale: f64,
    dsf: f64,
    render_ms: u128,
) -> RenderedPage {
    let (w, h) = page_pt.unwrap_or((1.0, 1.0));
    let width = ((w * scale * dsf) as i32).max(1);
    let height = ((h * scale * dsf) as i32).max(1);
    let stride = gtk::cairo::Format::Rgb24
        .stride_for_width(width as u32)
        .expect("stride");
    // Rgb24 with every byte 0xff is white (BGRx: B=G=R=255).
    let data = vec![0xffu8; (stride * height) as usize].into_boxed_slice();
    RenderedPage {
        data,
        width,
        height,
        stride,
        render_ms,
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

// Grow the content box by a 5pt margin, enforce a half-page minimum in each axis, and clamp to the
// page. Pure geometry so the crop behaviour is tested without a rendering backend.
fn apply_crop(content: Rectangle, width: f64, height: f64) -> Rectangle {
    let x1 = content.x1 - 5.0;
    let y1 = content.y1 - 5.0;
    let mut x2 = content.x2 + 5.0;
    let mut y2 = content.y2 + 5.0;
    if x2 - x1 < width / 2.0 {
        x2 = x1 + width / 2.0;
    }
    if y2 - y1 < height / 2.0 {
        y2 = y1 + height / 2.0;
    }
    Rectangle::new(x1.max(0.0), y1.max(0.0), x2.min(width), y2.min(height))
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
    fn page_footprint_maps_texture_1to1_at_integer_scale() {
        // dsf=1, no crop: origin at 0, footprint == texture pixels (the 1:1 sharpness invariant)
        let full = Rectangle::new(0.0, 0.0, 400.0, 600.0);
        assert_eq!(
            page_footprint((800, 1200), &full, 1.0, 1.0),
            ((0.0, 0.0), (800.0, 1200.0))
        );
    }

    #[test]
    fn page_footprint_snaps_crop_offset_to_device_grid() {
        // fractional crop margins would land the 1:1 texture off-grid and blur it; snap them
        let cropped = Rectangle::new(3.3, 2.6, 400.0, 600.0);
        // dsf=1 -> whole-pixel grid
        assert_eq!(
            page_footprint((800, 1200), &cropped, 1.0, 1.0).0,
            (-3.0, -3.0)
        );
        // dsf=2 -> half-pixel grid, footprint halves
        assert_eq!(
            page_footprint((800, 1200), &cropped, 1.0, 2.0),
            ((-3.5, -2.5), (400.0, 600.0))
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

    // The crop math is pure geometry over a content box (whatever backend produced it), so it's
    // tested directly. Page is 250x50.
    #[test]
    fn apply_crop_adds_margin() {
        let r = apply_crop(Rectangle::new(50.0, 15.0, 200.0, 40.0), 250.0, 50.0);
        assert!((r.x1 - 45.0).abs() < EPSILON);
        assert!((r.y1 - 10.0).abs() < EPSILON);
        assert!((r.x2 - 205.0).abs() < EPSILON);
        assert!((r.y2 - 45.0).abs() < EPSILON);
    }

    #[test]
    fn apply_crop_enforces_half_page_min() {
        // tiny content grows to at least half the page in each axis
        let r = apply_crop(Rectangle::new(9.5, 6.0, 20.0, 8.0), 250.0, 50.0);
        assert!((r.x1 - 4.5).abs() < EPSILON);
        assert!((r.y1 - 1.0).abs() < EPSILON);
        assert!((r.x2 - 129.5).abs() < EPSILON); // 4.5 + 250/2
        assert!((r.y2 - 26.0).abs() < EPSILON); // 1.0 + 50/2
    }

    #[test]
    fn apply_crop_clamps_to_page() {
        // margins pushing past the edges clamp back to [0,w] x [0,h]
        let r = apply_crop(Rectangle::new(2.0, 2.0, 248.0, 48.0), 250.0, 50.0);
        assert!((r.x1 - 0.0).abs() < EPSILON);
        assert!((r.y1 - 0.0).abs() < EPSILON);
        assert!((r.x2 - 250.0).abs() < EPSILON);
        assert!((r.y2 - 50.0).abs() < EPSILON);
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
        let uri = format!("file://{}", path.display());

        let state = crate::state::State::new();
        state.set_uri(uri);
        state.set_n_pages(2);
        let page = crate::page::Page::new(&state);

        // page 1 is 2000x3000pt. At zoom 10 a whole-page buffer would be ~2.4GB, so it renders
        // capped instead of not at all.
        page.imp()
            .schedule_render(1, 10.0, 1.0, RenderPriority::Prefetch);
        assert!(state.render_inflight().borrow().contains_key(&1));

        // already cached at the capped scale: don't render it again on every draw
        state.render_inflight().borrow_mut().clear();
        let capped = render_scale((2000.0, 3000.0), 10.0, 1.0);
        let (w, h) = render_dimensions((2000.0, 3000.0), capped, 1.0);
        let bytes = glib::Bytes::from_owned(vec![255u8; (w * h * 4) as usize]);
        let texture = MemoryTexture::new(w, h, MemoryFormat::B8g8r8x8, &bytes, (w * 4) as usize);
        state
            .render_cache()
            .borrow_mut()
            .insert(1, texture.upcast(), capped);
        page.imp()
            .schedule_render(1, 10.0, 1.0, RenderPriority::Prefetch);
        assert!(state.render_inflight().borrow().is_empty());
    }

    #[gtk::test]
    fn test_render() {
        // MuPDF opens by path, so write the fixture to a temp file, then render page 0 and assert
        // it produced a non-blank surface (exact pixels are backend-specific, so no snapshot).
        let dir = std::env::temp_dir().join("scrolex_test_render");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.pdf");
        std::fs::write(&path, SMALL_RENDERABLE_PDF).unwrap();
        let uri = format!("file://{}", path.display());

        let surface = crate::mupdf_render::render_page_surface(&uri, 0, 1.0, 1.0, None)
            .expect("mupdf should render the fixture");
        assert!(surface.width() > 0 && surface.height() > 0);

        let mut colored = false;
        surface
            .with_data(|d| {
                colored = d
                    .chunks_exact(4)
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
        let uri = format!("file://{path}");
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
