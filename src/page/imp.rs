#![expect(unused_lifetimes)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

use futures::channel::oneshot;
use gtk::cairo::{Context, FontSlant, FontWeight, ImageSurface};
use gtk::gdk::prelude::*;
use gtk::gdk::BUTTON_PRIMARY;
use gtk::glib;
use gtk::glib::clone;
use gtk::glib::subclass::{prelude::*, Signal};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;
use once_cell::sync::Lazy;

use super::Rectangle;
use crate::bg_job::{RenderPool, RenderPriority};
use crate::poppler::{Dest, DestExt, LinkType};

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
    // Pool caps: bbox, visible-preview, visible, preview, prefetch. The visible cap must exceed the
    // number of pages that can be on screen at once: the settle pass queues a visible render for
    // every visible page in one go, and any dropped past the cap would never reschedule (nothing
    // redraws them) and stay stuck in low-res. Fast-scroll flooding, the reason this was once small,
    // is now prevented by the settle-debounce (no visible renders scheduled while scrolling), so a
    // generous cap is safe.
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

// How many pages to prefetch ahead: the threads not busy on visible pages, but never more full
// pages than the cache can hold beyond the visible ones - else completed prefetches evict the
// visible pages and thrash. `capacity` 0 means nothing is cached yet, so fall back to at least one.
fn prefetch_depth(threads: usize, visible: usize, capacity: usize) -> usize {
    let want = threads.saturating_sub(visible);
    if capacity == 0 {
        want.max(1)
    } else {
        want.min(capacity.saturating_sub(visible + 1))
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

    highlighted: RefCell<Rectangle>,
    bbox: RefCell<Rectangle>,
    cursor_guard: Cell<bool>,

    // false until the widget has been mapped and its final device scale factor is in effect.
    // Rendering before then would use a provisional scale factor (the compositor assigns the real
    // one right after map) and be thrown away and re-rendered - expensive on HiDPI. While false,
    // the page paints blank.
    scale_known: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "Page";
    type Type = super::Page;
    type ParentType = DrawingArea;
}

#[glib::derived_properties]
impl ObjectImpl for Page {
    fn constructed(&self) {
        self.parent_constructed();

        self.setup_draw_function();
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

impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}

impl Page {
    fn setup_draw_function(&self) {
        let obj = self.obj();

        obj.set_draw_func(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |_, cr, _width, _height| {
                let Some(poppler_page) = obj.state().doc().and_then(|doc| doc.page(obj.index()))
                else {
                    return;
                };

                // Hold off rendering until the final device scale factor is known (set just after
                // map); otherwise the first render uses a provisional scale and is immediately
                // re-rendered. Paint blank meanwhile - the deferred redraw renders at the real
                // scale.
                if !imp.scale_known.get() {
                    cr.set_source_rgb(1.0, 1.0, 1.0);
                    cr.paint().expect("Failed to fill");
                    return;
                }

                cr.save().expect("Failed to save");

                if obj.state().multithread_rendering() {
                    imp.multithread_render_to_cairo(cr, &poppler_page);
                } else {
                    imp.render_to_cairo(cr, &poppler_page);
                }

                cr.restore().expect("Failed to restore");

                let highlighted = &imp.highlighted.borrow();
                let (w, h) = highlighted.size();

                if w * w > 0.0 && h * h > 0.0 {
                    imp.render_selection_overlay(cr, &poppler_page, highlighted);
                }

                imp.render_search_overlay(cr, &poppler_page);
            }
        ));
    }

    // Mark the device scale factor as known once it has settled after map, so the draw function can
    // start rendering at the final scale.
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
        // scale) is authoritative: the current cached surface is now stale, and
        // the draw's dimension check re-renders it at the new scale.
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
        let Some(poppler_page) = self.poppler_page() else {
            return;
        };
        let page = self.obj().clone();
        let (w, h) = poppler_page.size();

        self.resolve_bbox(
            &poppler_page,
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
                    page.set_size_request((w * page.zoom()) as i32, (h * page.zoom()) as i32);
                }
            ),
        );
    }

    fn poppler_page(&self) -> Option<poppler::Page> {
        let obj = self.obj();
        self.obj()
            .state()
            .doc()
            .and_then(|doc| doc.page(obj.index()))
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
            #[weak(rename_to = imp)]
            self,
            move |gc, seq| {
                let Some((start_x, start_y)) = *mouse_coords.borrow() else {
                    return;
                };
                let Some((end_x, end_y)) = gc.point(seq) else {
                    return;
                };
                let Some(poppler_page) = imp.poppler_page() else {
                    return;
                };

                let Point { x: x1, y: y1 } = undo_zoom_and_crop(&obj, start_x, start_y);
                let Point { x: x2, y: y2 } = undo_zoom_and_crop(&obj, end_x, end_y);
                let highlighted = Rectangle::new(x1, y1, x2, y2);
                imp.highlighted.replace(highlighted);

                let selected = &poppler_page.selected_text(
                    poppler::SelectionStyle::Glyph,
                    &mut highlighted.as_poppler(),
                );

                if let Some(selected) = selected {
                    obj.clipboard().set_text(selected);
                }

                obj.queue_draw();
            }
        ));

        let obj = self.obj().clone();
        gc.connect_end(move |_, _| {
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
                let Some(poppler_page) = imp.poppler_page() else {
                    return;
                };

                let Point { x, y } = undo_zoom_and_crop(&obj, x, y);
                if imp
                    .state
                    .borrow()
                    .imp()
                    .links
                    .borrow_mut()
                    .get_link(&poppler_page, x, y)
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
                let Some(poppler_page) = imp.poppler_page() else {
                    return;
                };

                let Point { x, y } = undo_zoom_and_crop(&obj, x, y);

                if let Some(link_type) =
                    imp.state
                        .borrow()
                        .imp()
                        .links
                        .borrow_mut()
                        .get_link(&poppler_page, x, y)
                {
                    match link_type {
                        LinkType::GotoNamedDest(name) => {
                            if let Some(doc) = obj.state().doc() {
                                let Some(dest) = doc.find_dest(name) else {
                                    return;
                                };

                                let Dest::Xyz(page_num) = dest.to_dest() else {
                                    return;
                                };

                                gc.set_state(gtk::EventSequenceState::Claimed); // stop the event from propagating
                                obj.emit_by_name::<()>("named-link-clicked", &[&page_num]);
                            }
                        }
                        LinkType::Uri(uri) => {
                            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                                uri,
                                gtk::gio::AppLaunchContext::NONE,
                            );
                        }
                        LinkType::Unknown(msg) => {
                            log::warn!("unhandled link: {msg:?}");
                        }
                        LinkType::Invalid => {
                            log::warn!("invalid link: {link_type:?}");
                        }
                    }
                };
            }
        ));
        obj.add_controller(gc);
    }

    fn get_bbox(&self, page: &poppler::Page, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        let bbox = get_bbox(page, true);
        self.state
            .borrow()
            .bbox_cache()
            .borrow_mut()
            .insert(page.index(), bbox);
        bbox
    }

    fn get_cached_bbox(&self, page: &poppler::Page, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        let (w, h) = page.size();
        Rectangle::new(0.0, 0.0, w, h)
    }

    // Resolve the page's bounding box and hand it to `cb`. Computed on the main thread from the
    // page we already hold: the layout is far cheaper than a full render, and resolving it inline
    // sizes the widget at once. A pooled job would lag behind the renders during a fast scroll,
    // leaving the page stuck at its stale size until the box arrived.
    fn resolve_bbox<F>(&self, page: &poppler::Page, crop: bool, cb: F)
    where
        F: FnOnce(&Rectangle) + 'static,
    {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            cb(&bbox);
            return;
        }

        let bbox = get_bbox(page, true);
        self.state
            .borrow()
            .bbox_cache()
            .borrow_mut()
            .insert(page.index(), bbox);
        cb(&bbox);
    }

    fn lookup_bbox(&self, page: &poppler::Page, crop: bool) -> Option<Rectangle> {
        if !crop {
            let (w, h) = page.size();
            return Some(Rectangle::new(0.0, 0.0, w, h));
        }
        self.state
            .borrow()
            .bbox_cache()
            .borrow()
            .get(&page.index())
            .copied()
    }

    fn render_to_cairo(&self, cr: &Context, poppler_page: &poppler::Page) {
        let start = std::time::Instant::now();
        let obj = self.obj();
        let (width, height) = poppler_page.size();
        let scale_factor = obj.scale_factor() as f64;

        let bbox = self.get_bbox(poppler_page, obj.crop());
        let scale = obj.zoom();

        if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
            cr.translate(-bbox.x1 * scale, -bbox.y1 * scale);
        }

        cr.rectangle(0.0, 0.0, width * scale, height * scale);
        //cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.fill().expect("Failed to fill");

        cr.scale(scale, scale);

        poppler_page.render(cr);

        let elapsed = start.elapsed();
        log::debug!(
            "Rendered page {} [on-demand (visible), sync] on main thread in {elapsed:?} (scale_factor={scale_factor})",
            poppler_page.index()
        );

        if elapsed > std::time::Duration::from_millis(100) {
            log::warn!("Rendering took too long: {elapsed:?}. Switching to multithreading mode.");
            obj.state().set_multithread_rendering(true);
        }
    }

    fn render_selection_overlay(
        &self,
        cr: &Context,
        poppler_page: &poppler::Page,
        rect: &Rectangle,
    ) {
        let start = std::time::Instant::now();

        let bbox = self.get_bbox(poppler_page, self.obj().crop());
        let scale = self.obj().zoom();

        let (w, h) = poppler_page.size();
        let mask = ImageSurface::create(gtk::cairo::Format::ARgb32, w as i32, h as i32)
            .expect("Failed to create mask surface");
        let mask_cr = Context::new(&mask).expect("Failed to create mask context");
        poppler_page.render_selection(
            &mask_cr,
            &mut rect.as_poppler(),
            &mut poppler::Rectangle::new(),
            poppler::SelectionStyle::Glyph,
            &mut poppler::Color::new(),
            &mut poppler::Color::new(),
        );

        if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
            cr.translate(-bbox.x1 * scale, -bbox.y1 * scale);
        }
        cr.scale(scale, scale);
        cr.set_source_rgba(0.5, 0.8, 0.9, 0.7);
        cr.mask_surface(&mask, 0.0, 0.0)
            .expect("Failed to mask surface");

        let elapsed = start.elapsed();
        log::trace!("Rendered selection {} in {elapsed:?}", poppler_page.index());
    }

    // Paint match rects for this page: matches yellow, the current match orange. Same zoom/crop
    // transform as the page render, so highlights land on the words.
    fn render_search_overlay(&self, cr: &Context, poppler_page: &poppler::Page) {
        let obj = self.obj();
        let index = obj.index();
        let search = obj.state().search();
        let search = search.borrow();
        let Some(rects) = search.results.get(&index) else {
            return;
        };
        if rects.is_empty() {
            return;
        }

        let bbox = self.get_bbox(poppler_page, obj.crop());
        let scale = obj.zoom();

        cr.save().expect("Failed to save");
        if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
            cr.translate(-bbox.x1 * scale, -bbox.y1 * scale);
        }
        cr.scale(scale, scale);

        for (i, rect) in rects.iter().enumerate() {
            let (w, h) = rect.size();
            cr.rectangle(rect.x1, rect.y1, w, h);
            if search.current == Some((index, i)) {
                cr.set_source_rgba(1.0, 0.55, 0.0, 0.45);
            } else {
                cr.set_source_rgba(1.0, 0.9, 0.0, 0.4);
            }
            cr.fill().expect("Failed to fill");
        }
        cr.restore().expect("Failed to restore");
        // reset to opaque black so a later fill/mask on this context isn't tinted
        cr.set_source_rgb(0.0, 0.0, 0.0);
    }

    fn multithread_render_to_cairo(&self, cr: &Context, poppler_page: &poppler::Page) {
        let obj = self.obj();
        let page_num = poppler_page.index();

        let (width, height) = poppler_page.size();
        let scale = obj.zoom();
        let scale_factor = obj.scale_factor() as f64;
        let (canvas_width, canvas_height) =
            (width * scale * scale_factor, height * scale * scale_factor);
        let expected = (canvas_width as i32, canvas_height as i32);

        let cache = obj.state().render_cache();
        let cached = cache.borrow_mut().get(page_num);
        if let Some(surface) = cached {
            if (surface.width(), surface.height()) == expected {
                log::debug!("draw page {page_num}: cache hit");
                let bbox = self.get_bbox(poppler_page, obj.crop());
                draw_surface(cr, &surface, &bbox, scale);
                self.prefetch_previews(page_num);
                self.prefetch_next(page_num);
                return;
            }
            // dimensions changed (e.g. zoom), the cached surface is stale
            log::debug!("draw page {page_num}: cache stale (zoom/scale changed)");
            cache.borrow_mut().remove(page_num);
        }

        // While scrolling, defer the full render: it's slow and uninterruptible, so rendering pages
        // that are flying past would saturate the workers and starve the previews. The preview
        // below stands in; the settle refresh redraws the visible pages once motion stops, and this
        // path then schedules their full renders.
        if !obj.state().scrolling() {
            // schedule the full render unless one is already queued for this page
            let is_new = obj.state().render_inflight().borrow_mut().insert(page_num);
            if is_new {
                self.schedule_render(page_num, scale, scale_factor, RenderPriority::Visible);
            }
        }

        // remember that this widget is the one waiting for page_num, so the
        // render repaints it when it lands
        obj.state()
            .render_waiters()
            .borrow_mut()
            .insert(page_num, obj.downgrade());

        // show a low-res preview (upscaled) if we have one, otherwise white
        let bbox = self.get_cached_bbox(poppler_page, obj.crop());
        let preview = obj.state().preview_cache().borrow_mut().get(page_num);
        if let Some(preview) = preview {
            log::debug!("draw page {page_num}: cache miss, showing preview");
            draw_preview(cr, &preview, &bbox, scale, width);
        } else {
            log::debug!("draw page {page_num}: cache miss (loading placeholder)");
            let (w, h) = bbox.size();
            draw_loading_placeholder(cr, w * scale, h * scale);
        }

        // prefetch a wider window of previews and queue this page's own preview at the highest
        // render priority so a blurry stand-in appears before any full render (never white on a
        // fast scroll)
        self.prefetch_previews(page_num);
        self.schedule_preview_if_needed(page_num, RenderPriority::VisiblePreview);
        self.prefetch_next(page_num);
    }

    // Full-render the next pages in the scroll direction, so reading on lands on a cached page. A
    // no-op while scrolling (a fling would only pile up soon-stale prefetch) and skips pages already
    // cached or queued - so from a screenful of visible pages this reaches just past the last one in
    // the direction of travel. Nice-to-have: the lowest render priority, run only once everything on
    // screen is done.
    fn prefetch_next(&self, current: i32) {
        let obj = self.obj();
        let state = obj.state();
        if state.scrolling() {
            return;
        }
        let Some(doc) = state.doc() else {
            return;
        };
        let n_pages = doc.n_pages();
        let dir = if state.scroll_forward() { 1 } else { -1 };
        let scale = obj.zoom();
        let scale_factor = obj.scale_factor() as f64;
        let cache = state.render_cache();
        let inflight = state.render_inflight();

        let visible = state.visible_page_count().max(1) as usize;
        let capacity = cache.borrow().page_capacity();
        let ahead = prefetch_depth(state.render_threads(), visible, capacity) as i32;

        // farthest first so the LIFO queue pops the nearest ahead-page first
        for d in (1..=ahead).rev() {
            let page_num = current + dir * d;
            if page_num < 0 || page_num >= n_pages {
                continue;
            }
            if cache.borrow().contains(page_num) {
                continue;
            }
            if inflight.borrow_mut().insert(page_num) {
                self.schedule_render(page_num, scale, scale_factor, RenderPriority::Prefetch);
            }
        }
    }

    fn schedule_render(
        &self,
        page_num: i32,
        scale: f64,
        scale_factor: f64,
        priority: RenderPriority,
    ) {
        let obj = self.obj();
        let uri = obj.uri();
        log::trace!("Scheduling render of page {page_num}");

        let (resp_sender, resp_receiver) = oneshot::channel::<RenderedPage>();
        let obj_clone = obj.clone();
        let uri_check = uri.clone();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let state = obj_clone.state();
            state.render_inflight().borrow_mut().remove(&page_num);

            // Request was dropped (evicted from the queue as over-cap). Once settled, redraw any
            // widget still waiting for this page so it reschedules - otherwise a page whose render
            // was dropped stays stuck on its preview with nothing to trigger a retry.
            let Ok(rendered) = result else {
                if !state.scrolling() {
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
                }
                return;
            };

            // the document may have changed while the render was in flight
            if obj_clone.uri() != uri_check {
                return;
            }

            let surface = rendered.into_surface(scale_factor);
            state.render_cache().borrow_mut().insert(page_num, surface);

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
                priority,
                Box::new(move |doc| {
                    request_render(
                        doc,
                        &uri_job,
                        scale,
                        scale_factor,
                        page_num,
                        priority,
                        resp_sender,
                    );
                }),
            );
        });
    }

    // Prefetch low-res previews over a wide symmetric window (they're cheap and tiny), so scrolling
    // either way shows blurry pages instead of blank ones.
    fn prefetch_previews(&self, current: i32) {
        let obj = self.obj();
        if !obj.state().preview_enabled() {
            return;
        }
        let Some(doc) = obj.state().doc() else {
            return;
        };
        let n_pages = doc.n_pages();

        // Walk outward from the visible page, interleaving both directions, and push so the nearest
        // pages end up on top of the LIFO queue (rendered first). Pages already cached - typically
        // the side scrolled from - are skipped, so effort tracks the direction of travel.
        let mut candidates = Vec::with_capacity(2 * PREVIEW_WINDOW as usize);
        for d in (1..=PREVIEW_WINDOW).rev() {
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
        let scale = obj.state().preview_scale();

        let (resp_sender, resp_receiver) = oneshot::channel::<RenderedPage>();
        let obj_clone = obj.clone();
        let uri_check = uri.clone();
        glib::spawn_future_local(async move {
            let result = resp_receiver.await;
            let state = obj_clone.state();
            state.preview_inflight().borrow_mut().remove(&page_num);

            let Ok(rendered) = result else {
                return;
            };
            if obj_clone.uri() != uri_check {
                return;
            }

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

            let surface = rendered.into_surface(1.0);
            state.preview_cache().borrow_mut().insert(page_num, surface);

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
                priority,
                Box::new(move |doc| {
                    request_render(doc, &uri_job, scale, 1.0, page_num, priority, resp_sender);
                }),
            );
        });
    }

    pub fn render_surface(&self, cr: &Context, surface: &ImageSurface, bbox: &Rectangle) {
        draw_surface(cr, surface, bbox, self.obj().zoom());
    }
}

pub fn draw_surface(cr: &Context, surface: &ImageSurface, bbox: &Rectangle, scale: f64) {
    // Snap the paste position to a whole device pixel; a fractional offset resamples and blurs the
    // 1:1 surface (crop's bbox margins would otherwise land it off-grid).
    let (device_scale, _) = surface.device_scale();
    let snap = |v: f64| (v * device_scale).round() / device_scale;
    cr.set_source_surface(surface, snap(-bbox.x1 * scale), snap(-bbox.y1 * scale))
        .unwrap();
    let (w, h) = bbox.size();
    cr.rectangle(0.0, 0.0, w * scale, h * scale);
    cr.clip();
    cr.paint().unwrap();

    // Release the surface data
    cr.set_source_rgb(0.0, 0.0, 0.0);
}

// Draw a low-res preview surface upscaled to fill the same area a full render at `scale` would
// occupy (blurry stand-in while the full render lands). The preview is a full-page render, so its
// render scale is recovered from the full page width (not the cropped bbox) and its device scale;
// a cache holding previews rendered at different (adaptive) scales still upscales each correctly.
fn draw_preview(
    cr: &Context,
    preview: &ImageSurface,
    bbox: &Rectangle,
    scale: f64,
    page_width: f64,
) {
    let (device_scale, _) = preview.device_scale();
    let preview_scale = if page_width > 0.0 {
        preview.width() as f64 / (page_width * device_scale)
    } else {
        scale
    };
    let upscale = scale / preview_scale;
    cr.save().unwrap();
    cr.scale(upscale, upscale);
    draw_surface(cr, preview, bbox, preview_scale);
    cr.restore().unwrap();
    cr.set_source_rgb(0.0, 0.0, 0.0);
}

// Steer the preview render scale toward two budgets at once, from what the last preview render (at
// `cur_scale`) actually cost: a per-preview time budget (keep the stand-in fast) and a per-preview
// size budget (keep the whole window resident so previews don't thrash their cache). Both costs
// grow ~scale^2, so each budget maps to a scale by the same square-root correction; we take the
// tighter of the two and clamp to the usable range.
fn adapt_preview_scale(cur_scale: f64, render_ms: u128, bytes: usize) -> f64 {
    // Pages we want kept warm at once: the full symmetric window plus the visible page. The cache
    // budget divided by this is the per-preview size ceiling; tying it to the budget and window
    // keeps a single source of truth if either changes.
    const RESIDENT_PREVIEWS: usize = (2 * PREVIEW_WINDOW + 1) as usize;
    let target_bytes = (crate::state::PREVIEW_CACHE_BUDGET / RESIDENT_PREVIEWS) as f64;

    let render_ms = render_ms.max(1) as f64;
    let bytes = bytes.max(1) as f64;

    let scale_time = cur_scale * (PREVIEW_TARGET_MS as f64 / render_ms).sqrt();
    let scale_mem = cur_scale * (target_bytes / bytes).sqrt();

    scale_time
        .min(scale_mem)
        .clamp(PREVIEW_MIN_SCALE, PREVIEW_MAX_SCALE)
}

fn draw_loading_placeholder(cr: &Context, width: f64, height: f64) {
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

    // reset to opaque black so a later fill/mask on this context isn't tinted grey
    cr.set_source_rgb(0.0, 0.0, 0.0);
}

// A rendered page as raw pixels. Rendering happens on a background thread, and
// `ImageSurface` is not `Send`, so the pixels cross the thread boundary as a
// plain buffer and the surface is rebuilt on the main thread.
#[derive(Debug)]
struct RenderedPage {
    data: Box<[u8]>,
    width: i32,
    height: i32,
    stride: i32,
    render_ms: u128,
}

impl RenderedPage {
    fn into_surface(self, device_scale_factor: f64) -> ImageSurface {
        let surface = ImageSurface::create_for_data(
            self.data,
            gtk::cairo::Format::Rgb24,
            self.width,
            self.height,
            self.stride,
        )
        .expect("Failed to create image surface");
        surface.set_device_scale(device_scale_factor, device_scale_factor);
        surface
    }
}

fn request_render(
    doc: &poppler::Document,
    uri: &str,
    scale: f64,
    device_scale_factor: f64,
    page_num: i32,
    priority: RenderPriority,
    resp_sender: oneshot::Sender<RenderedPage>,
) {
    let Some(page) = doc.page(page_num) else {
        log::warn!("render skipped: page {page_num} not found in document");
        return;
    };

    let start = std::time::Instant::now();
    let surface = crate::image_page::render_image_page(uri, &page, scale, device_scale_factor)
        .unwrap_or_else(|| render_surface(&page, scale, device_scale_factor));
    let (width, height, stride) = (surface.width(), surface.height(), surface.stride());
    let render_ms = start.elapsed().as_millis();
    log::debug!(
        "Rendered page {page_num} [{}] on background thread in {render_ms}ms (scale_factor={device_scale_factor})",
        priority.label()
    );

    let mut buffer = vec![0u8; (stride * height) as usize];
    surface
        .with_data(|data| {
            buffer.copy_from_slice(data);
        })
        .expect("Failed to extract surface data");
    surface.finish();
    // ignore send failure: the receiver is gone if the page's widget was
    // dropped or its render superseded
    let _ = resp_sender.send(RenderedPage {
        data: buffer.into_boxed_slice(),
        width,
        height,
        stride,
        render_ms,
    });
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

pub fn render_surface(page: &poppler::Page, scale: f64, device_scale_factor: f64) -> ImageSurface {
    let (width, height) = page.size();
    let scale_factor = device_scale_factor * scale;
    let (canvas_width, canvas_height) = (width * scale_factor, height * scale_factor);

    let surface = ImageSurface::create(
        gtk::cairo::Format::Rgb24,
        //gtk::cairo::Format::ARgb32,
        canvas_width as i32,
        canvas_height as i32,
    )
    .expect("Couldn't create a surface!");
    surface.set_device_scale(device_scale_factor, device_scale_factor);
    let cr = Context::new(&surface).expect("Couldn't create a context!");
    cr.rectangle(0.0, 0.0, canvas_width, canvas_height);
    cr.scale(scale, scale);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.fill().expect("Failed to fill");
    page.render(&cr);

    //let mut old_rect = poppler::Rectangle::new();
    //let mut rect = poppler::Rectangle::new();
    //rect.set_x1(0.0);
    //rect.set_y1(0.0);
    //rect.set_x2(width);
    //rect.set_y2(height / 2.0);
    //page.render_selection(
    //    &cr,
    //    &mut rect,
    //    &mut old_rect,
    //    poppler::SelectionStyle::Glyph,
    //    &mut poppler::Color::new(),
    //    &mut poppler::Color::new(),
    //);

    surface
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

fn get_bbox(page: &poppler::Page, crop: bool) -> Rectangle {
    let (width, height) = page.size();
    let mut bbox = poppler::Rectangle::default();
    bbox.set_x1(0.0);
    bbox.set_y1(0.0);
    bbox.set_x2(width);
    bbox.set_y2(height);

    if crop {
        let mut poppler_bbox = poppler::Rectangle::default();
        page.get_bounding_box(&mut poppler_bbox);

        bbox.set_x1(poppler_bbox.x1() - 5.0);
        bbox.set_x2(poppler_bbox.x2() + 5.0);

        bbox.set_y1(poppler_bbox.y1() - 5.0);
        bbox.set_y2(poppler_bbox.y2() + 5.0);

        if bbox.x2() - bbox.x1() < width / 2.0 {
            bbox.set_x2(bbox.x1() + width / 2.0);
        }
        if bbox.y2() - bbox.y1() < height / 2.0 {
            bbox.set_y2(bbox.y1() + height / 2.0);
        }

        bbox.set_x1(bbox.x1().max(0.0));
        bbox.set_y1(bbox.y1().max(0.0));
        bbox.set_x2(bbox.x2().min(width));
        bbox.set_y2(bbox.y2().min(height));
    }

    //Rectangle::from_poppler(&bbox, height)
    Rectangle::new(bbox.x1(), bbox.y1(), bbox.x2(), bbox.y2())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::env;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;

    #[test]
    fn prefetch_depth_bounds() {
        // spare threads run ahead, capped so visible pages + 1 headroom stay in the cache
        assert_eq!(prefetch_depth(11, 3, 8), 4); // min(11-3, 8-4)
        assert_eq!(prefetch_depth(4, 1, 8), 3); // threads-bound, cache has room
        assert_eq!(prefetch_depth(11, 3, 0), 8); // capacity unknown: thread-bound
        assert_eq!(prefetch_depth(11, 7, 8), 0); // no room left: don't evict visible pages
        assert_eq!(prefetch_depth(2, 3, 8), 0); // more visible than threads
    }

    const EPSILON: f64 = 0.0001;
    const SMALL_PDF: &[u8] = b"%PDF-1.2 \n\
9 0 obj\n<<\n>>\nstream\nBT/ 32 Tf(  YOUR TEXT HERE   )' ET\nendstream\nendobj\n\
10 0 obj\n<<\n/Subtype /Link\n/Rect [ {BBOX} ]\n/Contents (Your Annotation Text)\n\
/C [ 1 1 0 ]\n>>\nendobj\n\
4 0 obj\n<<\n/Type /Page\n/Parent 5 0 R\n/Contents 9 0 R\n/Annots [10 0 R ]\n>>\nendobj\n\
5 0 obj\n<<\n/Kids [4 0 R ]\n/Count 1\n/Type /Pages\n/MediaBox [ 0 0 250 50 ]\n>>\nendobj\n\
3 0 obj\n<<\n/Pages 5 0 R\n/Type /Catalog\n>>\nendobj\n\
trailer\n<<\n/Root 3 0 R\n>>\n\
%%EOF";

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
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "0 0 240 40");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, false);
        assert!((bbox.x1 - 0.0).abs() < EPSILON);
        assert!((bbox.y1 - 0.0).abs() < EPSILON);
        assert!((bbox.x2 - 250.0).abs() < EPSILON);
        assert!((bbox.y2 - 50.0).abs() < EPSILON);
    }

    #[test]
    fn test_get_bbox_with_crop() {
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "10 6.5 238 41.5");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, true);

        // [ 10 6.5 238 41.5 ]
        // corresponds to this bbox in poppler:
        // { x1: 9.5, y1: 8.0, x2: 238.5, y2: 44.0}
        // notice strange y2 and y1. Poppler uses left-bottom as origin.
        // 0.5 pixels for the border I guess.

        assert!((bbox.x1 - 4.5).abs() < EPSILON); // 10.0 - 0.5 - 5
        assert!((bbox.y1 - 3.0).abs() < EPSILON); // 50 - (41.5 + 0.5 + 5)
        assert!((bbox.x2 - 243.5).abs() < EPSILON); // 238.0 + 0.5 + 5
        assert!((bbox.y2 - 49.0).abs() < EPSILON); // 50 - (6.5 - 0.5 - 5)
    }

    #[test]
    fn test_get_bbox_with_big_margins() {
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "10 34 20 43.5");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, true);

        assert!((bbox.x1 - 4.5).abs() < EPSILON); // 10.0 - 0.5 - 5
        assert!((bbox.y1 - 1.0).abs() < EPSILON);
        assert!((bbox.x2 - 129.5).abs() < EPSILON); // 4.5 + 250 / 2
        assert!((bbox.y2 - 26.0).abs() < EPSILON);
    }

    #[gtk::test]
    fn test_render() {
        let doc = poppler::Document::from_data(SMALL_RENDERABLE_PDF, None).unwrap();

        let state = crate::state::State::new();
        let page = crate::page::Page::new(&state);
        page.state().set_doc(&doc);
        page.bind(&crate::page::PageNumber::new(0));

        let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::Rgb24, 80, 12).unwrap();
        let cr = gtk::cairo::Context::new(&surface).unwrap();

        page.imp().render_to_cairo(&cr, &doc.page(0).unwrap());
        let mut buffer = vec![0u8; (surface.stride() * surface.height()) as usize];
        surface
            .with_data(|data| {
                buffer.copy_from_slice(data);
            })
            .expect("Failed to extract surface data");
        surface.finish();

        assert_snapshot("test_render", &buffer);
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
                        let doc = poppler::Document::from_file(&uri, None).unwrap();
                        for p in chunk {
                            if let Some(page) = doc.page(p) {
                                std::hint::black_box(render_surface(&page, scale, 1.0));
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

    // Memory probe: isolates the two costs of the N-threads/N-documents render pool - the resident
    // poppler Documents (one per thread) versus the glibc malloc arena high-water mark left by
    // transient render allocations. For a given THREADS it renders PAGES across that many threads
    // (each with its own Document, matching the real pool), keeps all threads/documents alive, and
    // reports VmRSS after rendering and again after malloc_trim(0). The gap that malloc_trim
    // reclaims is the arena bloat (fixable without dropping parallelism); the residual scaling with
    // THREADS is the per-document cost. Run per thread count in separate processes so arenas don't
    // carry over:
    //   PDF_PATH=/abs/file.pdf THREADS=4 cargo test --release bench_render_memory -- --ignored --nocapture
    // Optional env: PAGE_NUMBER, PAGES, MALLOC_ARENA_MAX (set before running).
    #[test]
    #[ignore]
    fn bench_render_memory() {
        use std::sync::{mpsc, Arc, Mutex};

        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        let rss_mb = current_rss_mb;

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
        let threads: usize = env::var("THREADS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4);
        // re-render the same pages this many times to imitate a long scrolling
        // session churning allocations, so growth-with-churn (a leak) shows up
        // distinct from the one-pass working set
        let passes: usize = env::var("PASSES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let pages: Vec<i32> = (start..start + count).collect();

        let baseline = rss_mb();

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        let exit_rx = Arc::new(Mutex::new(exit_rx));

        std::thread::scope(|s| {
            for t in 0..threads {
                let uri = uri.clone();
                let chunk: Vec<i32> = pages.iter().copied().skip(t).step_by(threads).collect();
                let done_tx = done_tx.clone();
                let exit_rx = exit_rx.clone();
                s.spawn(move || {
                    // each thread keeps its own Document, like the real pool
                    let doc = poppler::Document::from_file(&uri, None).unwrap();
                    for _ in 0..passes {
                        for p in &chunk {
                            if let Some(page) = doc.page(*p) {
                                std::hint::black_box(render_surface(&page, 1.0, 1.0));
                            }
                        }
                    }
                    // report done, then hold the Document resident until released
                    done_tx.send(()).unwrap();
                    let _ = exit_rx.lock().unwrap().recv();
                    std::hint::black_box(&doc);
                });
            }

            for _ in 0..threads {
                done_rx.recv().unwrap();
            }

            let after_render = rss_mb();
            unsafe { malloc_trim(0) };
            let after_trim = rss_mb();

            println!(
                "threads={threads:<2} passes={passes} pages={} baseline={baseline:.0}MB after_render={after_render:.0}MB \
                 after_trim={after_trim:.0}MB reclaimed_by_trim={:.0}MB resident_over_baseline={:.0}MB",
                pages.len(),
                after_render - after_trim,
                after_trim - baseline,
            );

            for _ in 0..threads {
                exit_tx.send(()).unwrap();
            }
        });
    }

    #[test]
    fn test_render_surface() {
        let doc = poppler::Document::from_data(SMALL_RENDERABLE_PDF, None).unwrap();
        let surface = render_surface(&doc.page(0).unwrap(), 1.0, 1.0);

        let mut buffer = vec![0u8; (surface.stride() * surface.height()) as usize];
        surface
            .with_data(|data| {
                buffer.copy_from_slice(data);
            })
            .expect("Failed to extract surface data");
        surface.finish();

        assert_snapshot("test_render_surface", &buffer);
    }

    fn assert_snapshot(snapshot_name: &str, data: &[u8]) {
        let snapshot_dir = Path::new(".snapshots");
        let snapshot_file_path = snapshot_dir.join(format!("{snapshot_name}.snap"));

        if env::var("UPDATE_SNAP").is_ok() {
            let compressed_data = compress_data(data);

            fs::create_dir_all(snapshot_dir).expect("Failed to create snapshot directory");
            fs::write(&snapshot_file_path, compressed_data).expect("Failed to write snapshot file");

            println!("Snapshot updated.");
        } else {
            let compressed_snapshot =
                fs::read(&snapshot_file_path).expect("Failed to read snapshot file");

            let decompressed_snapshot =
                decompress_data(&compressed_snapshot).expect("Failed to decompress snapshot");

            let diff = decompressed_snapshot
                .iter()
                .zip(data.iter())
                .fold(0, |acc, (a, b)| acc + (*a as i32 - *b as i32).abs());

            assert_eq!(diff, 0)
        }
    }

    fn compress_data(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(9));
        encoder.write_all(data).expect("Failed to compress data");
        encoder.finish().expect("Failed to finish compression")
    }

    fn decompress_data(compressed_data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        let mut decoder = GzDecoder::new(compressed_data);
        let mut decompressed_data = Vec::new();
        decoder.read_to_end(&mut decompressed_data)?;
        Ok(decompressed_data)
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
