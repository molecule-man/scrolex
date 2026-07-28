// Public state API for document loading, persistence, and rendering coordination.
mod imp;
use futures::channel::oneshot;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::clone;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::*;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::{env, fs};

use crate::page;

// Per-preview size the adaptive preview scaler steers toward. The preview cache's byte budget is
// this times the configured number of resident previews (config::preview_cache_pages), so the cache
// holds about that many previews regardless of the adaptive scale.
pub(crate) const PREVIEW_TARGET_BYTES: usize = 20 * 1024 * 1024 / 65;

// Ceiling on one page's pixel buffer. Pages are rendered whole, so the buffer grows with the square
// of the zoom; the zoom bound derived from this cap (see zoom_ceiling) is what keeps deep zoom from
// allocating buffers in the gigabytes.
pub(crate) const MAX_PAGE_BYTES: f64 = 128.0 * 1024.0 * 1024.0;
// Absolute zoom bounds, so a postage-stamp page can't reach unusable zoom levels and zooming out
// can't shrink a page to nothing.
const MAX_ZOOM: f64 = 10.0;
const MIN_ZOOM: f64 = 0.05;

// Preview cache byte budget for a given number of resident previews.
pub(crate) fn preview_cache_budget(pages: usize) -> usize {
    pages * PREVIEW_TARGET_BYTES
}

// Zoom at which one page's pixel buffer reaches MAX_PAGE_BYTES, given the page size in points and
// the device scale factor (4 bytes per device pixel). HiDPI screens hit it at a lower zoom because
// the bound is on rendered pixels, not on nominal zoom.
fn zoom_ceiling(page_pt: (f64, f64), device_scale: i32) -> f64 {
    let (w, h) = page_pt;
    let dsf = f64::from(device_scale.max(1));
    let bytes_at_1x = w * h * dsf * dsf * 4.0;
    if !bytes_at_1x.is_finite() || bytes_at_1x <= 0.0 {
        return MAX_ZOOM;
    }
    (MAX_PAGE_BYTES / bytes_at_1x)
        .sqrt()
        .clamp(MIN_ZOOM, MAX_ZOOM)
}

type MaxPageSize = Option<(f64, f64)>;

fn document_size_bytes(f: &gtk::gio::File) -> i64 {
    f.query_info(
        "standard::size",
        gtk::gio::FileQueryInfoFlags::NONE,
        gtk::gio::Cancellable::NONE,
    )
    .map(|info| info.size())
    .unwrap_or(-1)
}

glib::wrapper! {
    pub struct State(ObjectSubclass<imp::State>);
}

impl State {
    pub(crate) fn new() -> Self {
        // the preview-cache budget and other builder-instance setup live in State's `constructed`,
        // which runs here too
        glib::Object::builder()
            .property("zoom", 1.0)
            .property("crop", false)
            .property("animate_scroll", true)
            .property("page", 0_u32)
            .build()
    }

    // Every zoom change goes through here: the ceiling depends on this document's page size and the
    // device scale, and setting the property directly skips it.
    pub(crate) fn zoom_to(&self, zoom: f64) {
        let bounded = zoom.clamp(MIN_ZOOM, self.max_zoom());
        if bounded != self.zoom() {
            self.set_zoom(bounded);
        }
    }

    pub(crate) fn max_zoom(&self) -> f64 {
        match self.imp().page_size_pt.get() {
            Some(page_pt) => zoom_ceiling(page_pt, self.imp().device_scale.get()),
            None => MAX_ZOOM,
        }
    }

    // Widen the document's page size to cover `page_pt`: the load-time sample only sees the first
    // pages, and the bound has to hold for the biggest page actually drawn. Re-clamps the zoom on
    // idle, since callers are mid-snapshot and lowering the zoom resizes every page widget.
    pub(crate) fn observe_page_size(&self, page_pt: (f64, f64)) {
        let area = page_pt.0 * page_pt.1;
        if !area.is_finite() || area <= 0.0 {
            return;
        }
        if self
            .imp()
            .page_size_pt
            .get()
            .is_some_and(|(w, h)| w * h >= area)
        {
            return;
        }

        self.imp().page_size_pt.set(Some(page_pt));
        if self.zoom() > self.max_zoom() {
            glib::idle_add_local_once(clone!(
                #[weak(rename_to = state)]
                self,
                move || state.zoom_to(state.zoom())
            ));
        }
    }

    // Device pixels per point of the window showing this document; moving to a differently scaled
    // monitor changes the zoom ceiling, so re-apply the current zoom against it.
    pub(crate) fn set_device_scale(&self, scale: i32) {
        self.imp().device_scale.set(scale);
        self.zoom_to(self.zoom());
    }

    pub(crate) fn jump_list_add(&self, page: u32) {
        self.set_prev_page(page);
        self.imp().jump_stack.borrow_mut().push(page);
    }

    pub(crate) fn jump_list_pop(&self) -> Option<u32> {
        let page = self.imp().jump_stack.borrow_mut().pop();
        self.set_prev_page(self.imp().jump_stack.borrow().peek().unwrap_or_default());
        page
    }

    pub fn load(&self, f: &gtk::gio::File) {
        if self.n_pages() > 0 {
            if let Err(err) = self.save() {
                log::warn!("could not save state before load: {err}");
            }
        }

        let seq = self.imp().load_seq.get().wrapping_add(1);
        self.imp().load_seq.set(seq);

        let uri = f.uri();
        let size_bytes = document_size_bytes(f);
        self.emit_by_name::<()>("load-started", &[]);

        // Stage and open the document off the main thread so a heavy file doesn't freeze the UI.
        // A failed open leaves the current document (and its in-flight render markers) intact,
        // since nothing below the commit runs until the open succeeds. Staging fetches a remote
        // file exactly once; those bytes are the ones committed for rendering - no re-fetch.
        let (tx, rx) =
            oneshot::channel::<Option<(crate::mupdf_render::Candidate, i32, MaxPageSize)>>();
        let uri_probe = uri.clone();
        std::thread::spawn(move || {
            let probed = crate::mupdf_render::stage_candidate(&uri_probe).and_then(|candidate| {
                match candidate.probe() {
                    Some((n_pages, max_page_size)) if n_pages > 0 => {
                        Some((candidate, n_pages, max_page_size))
                    }
                    _ => None,
                }
            });
            let _ = tx.send(probed);
        });

        glib::spawn_future_local(clone!(
            #[weak(rename_to = state)]
            self,
            async move {
                let probed = rx.await.ok().flatten();
                if state.imp().load_seq.get() != seq {
                    return; // a newer load superseded this one
                }
                let Some((candidate, n_pages, max_page_size)) = probed else {
                    state.emit_by_name::<()>(
                        "load-failed",
                        &[&"could not open document".to_string()],
                    );
                    return;
                };
                state.commit_load(&uri, candidate, n_pages, max_page_size, size_bytes);
            }
        ));
    }

    fn commit_load(
        &self,
        uri: &str,
        candidate: crate::mupdf_render::Candidate,
        n_pages: i32,
        max_page_size: MaxPageSize,
        size_bytes: i64,
    ) {
        // Committed to the new document: force every thread to reopen (the same path may have
        // changed on disk), publish the validated bytes for the render workers, then reset
        // per-document state.
        // Reloads are process-wide for a URI. Multiple windows displaying the same URI are not
        // version-isolated if that file changes on disk.
        crate::mupdf_render::invalidate();
        candidate.commit();
        // Drop this window's queued renders and wanted-range entry for the outgoing document so they
        // neither run stale nor linger in the shared pool.
        let client = self.render_client_id();
        crate::page::clear_all_renders(client);
        crate::page::set_wanted_pages(client, None);
        // invalidate this window's in-flight renders (their content/scale is about to change)
        self.imp()
            .doc_epoch
            .set(self.imp().doc_epoch.get().wrapping_add(1));
        self.imp().bbox_cache.borrow_mut().clear();
        self.imp().links.borrow_mut().clear();
        self.imp().search.borrow_mut().clear();
        self.imp().render_cache.borrow_mut().clear();
        self.imp().render_inflight.borrow_mut().clear();
        self.imp().render_waiters.borrow_mut().clear();
        self.imp().preview_cache.borrow_mut().clear();
        self.imp().preview_inflight.borrow_mut().clear();
        self.imp().preview_enabled.set(true);
        self.imp().preview_slow_streak.set(0);
        self.imp()
            .preview_scale
            .set(crate::page::PREVIEW_INITIAL_SCALE);

        self.emit_by_name::<()>("before-load", &[]);

        let state_path = get_state_file_path(uri).unwrap();

        self.imp().jump_stack.borrow_mut().reset();
        self.set_prev_page(0);
        self.set_uri(uri);
        self.set_n_pages(n_pages);
        self.imp().page_size_pt.set(max_page_size);
        self.zoom_to(1.0);
        self.set_crop(false);
        self.set_page(0);
        self.set_multithread_rendering(false);

        if state_path.exists() {
            for line in fs::read_to_string(&state_path).unwrap().lines() {
                match line.split_once('=') {
                    Some(("zoom", value)) => {
                        let zoom = value.parse().unwrap_or(1.0);
                        if zoom > 0.0 {
                            self.zoom_to(zoom);
                        }
                    }
                    Some(("page", value)) => {
                        let page = value.parse().unwrap_or(0);
                        self.set_page(page);
                    }
                    Some(("crop", value)) => {
                        let crop = value.parse().unwrap_or(false);
                        self.set_crop(crop);
                    }
                    _ => {}
                }
            }
        }

        log::info!(
            "Loaded document: {n_pages} pages, {size_bytes} bytes, largest sampled page {max_page_size:?} pt, \
             start page {}, zoom {}, crop {}",
            self.page(),
            self.zoom(),
            self.crop(),
        );

        self.emit_by_name::<()>("loaded", &[]);
    }

    pub fn save(&self) -> io::Result<()> {
        let state_path = get_state_file_path(&self.uri()).unwrap();
        let state_dir = state_path.parent().unwrap();

        if !state_dir.exists() {
            fs::create_dir_all(state_dir)?;
        }

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&state_path)?;

        writeln!(file, "zoom={}", self.zoom())?;
        writeln!(file, "page={}", self.page())?;
        writeln!(file, "crop={}", self.crop())?;

        file.flush()
    }

    pub(crate) fn bbox_cache(&self) -> Rc<RefCell<HashMap<i32, page::Rectangle>>> {
        self.imp().bbox_cache.clone()
    }

    pub(crate) fn search(&self) -> Rc<RefCell<crate::search::Search>> {
        self.imp().search.clone()
    }

    pub(crate) fn render_cache(&self) -> Rc<RefCell<crate::render_cache::RenderCache>> {
        self.imp().render_cache.clone()
    }

    pub(crate) fn render_inflight(&self) -> Rc<RefCell<HashMap<i32, u64>>> {
        self.imp().render_inflight.clone()
    }

    pub(crate) fn render_waiters(&self) -> Rc<RefCell<HashMap<i32, glib::WeakRef<page::Page>>>> {
        self.imp().render_waiters.clone()
    }

    pub(crate) fn preview_cache(&self) -> Rc<RefCell<crate::render_cache::RenderCache>> {
        self.imp().preview_cache.clone()
    }

    pub(crate) fn set_preview_cache_pages(&self, pages: usize) {
        self.imp()
            .preview_cache
            .borrow_mut()
            .set_budget(preview_cache_budget(pages));
    }

    pub(crate) fn render_epoch(&self) -> u64 {
        self.imp().render_epoch.get()
    }

    pub(crate) fn render_client_id(&self) -> u64 {
        self.imp().render_client_id.get()
    }

    pub(crate) fn doc_epoch(&self) -> u64 {
        self.imp().doc_epoch.get()
    }

    pub(crate) fn set_render_cache_mb(&self, mb: usize) {
        self.imp()
            .render_cache
            .borrow_mut()
            .set_budget(mb * 1024 * 1024);
    }

    pub(crate) fn preview_inflight(&self) -> Rc<RefCell<HashSet<i32>>> {
        self.imp().preview_inflight.clone()
    }

    pub(crate) fn preview_enabled(&self) -> bool {
        self.imp().preview_enabled.get()
    }

    pub(crate) fn set_preview_enabled(&self, enabled: bool) {
        self.imp().preview_enabled.set(enabled);
    }

    pub(crate) fn preview_slow_streak(&self) -> u32 {
        self.imp().preview_slow_streak.get()
    }

    pub(crate) fn set_preview_slow_streak(&self, streak: u32) {
        self.imp().preview_slow_streak.set(streak);
    }

    pub(crate) fn preview_scale(&self) -> f64 {
        self.imp().preview_scale.get()
    }

    pub(crate) fn set_preview_scale(&self, scale: f64) {
        self.imp().preview_scale.set(scale);
    }

    pub(crate) fn scroll_forward(&self) -> bool {
        self.imp().scroll_forward.get()
    }

    pub(crate) fn set_scroll_forward(&self, forward: bool) {
        self.imp().scroll_forward.set(forward);
    }

    pub(crate) fn render_threads(&self) -> usize {
        self.imp().render_threads.get()
    }

    pub(crate) fn set_render_threads(&self, n: usize) {
        self.imp().render_threads.set(n);
    }

    pub(crate) fn visible_page_count(&self) -> i32 {
        self.imp().visible_page_count.get()
    }

    pub(crate) fn set_visible_page_count(&self, count: i32) {
        self.imp().visible_page_count.set(count);
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

fn get_state_file_path(uri: &str) -> Result<PathBuf, env::VarError> {
    let mut state_path = env::var("XDG_STATE_HOME")
        .or_else(|_| env::var("HOME").map(|home| format!("{home}/.local/state")))
        .map(PathBuf::from)?;

    state_path.push("pdf-viewer");
    state_path.push(uri);
    state_path.set_extension("ini");

    Ok(state_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 4 bytes per device pixel
    fn page_bytes_at(page_pt: (f64, f64), device_scale: i32, zoom: f64) -> f64 {
        let dsf = f64::from(device_scale);
        (page_pt.0 * zoom * dsf).floor() * (page_pt.1 * zoom * dsf).floor() * 4.0
    }

    #[test]
    fn zoom_ceiling_bounds_the_page_buffer() {
        let a4 = (595.44, 842.16);
        for device_scale in [1, 2, 3] {
            let ceiling = zoom_ceiling(a4, device_scale);
            assert!(page_bytes_at(a4, device_scale, ceiling) <= MAX_PAGE_BYTES);
            // a HiDPI screen reaches the same pixel count at a lower nominal zoom
            assert!(ceiling <= zoom_ceiling(a4, device_scale - 1));
        }
        // the reported case: 1000% on an A4 page at scale factor 2 is not reachable
        assert!(zoom_ceiling(a4, 2) < 10.0);
    }

    #[gtk::test]
    fn a_bigger_drawn_page_tightens_the_zoom_ceiling() {
        let state = State::new();
        state.imp().device_scale.set(1);
        state.imp().page_size_pt.set(Some((200.0, 200.0)));
        let small_page_ceiling = state.max_zoom();

        // a page the load-time sample missed
        state.observe_page_size((2000.0, 3000.0));
        let big_page_ceiling = state.max_zoom();
        assert!(big_page_ceiling < small_page_ceiling);
        assert!(page_bytes_at((2000.0, 3000.0), 1, big_page_ceiling) <= MAX_PAGE_BYTES);

        // smaller pages don't loosen it again
        state.observe_page_size((100.0, 100.0));
        assert_eq!(state.max_zoom(), big_page_ceiling);
        // nor do degenerate sizes
        state.observe_page_size((0.0, 0.0));
        state.observe_page_size((f64::NAN, 10.0));
        assert_eq!(state.max_zoom(), big_page_ceiling);
    }

    #[test]
    fn zoom_ceiling_clamps_extreme_page_sizes() {
        // a stamp-sized page would allow absurd zoom; a poster-sized one less than 100%
        assert_eq!(zoom_ceiling((10.0, 10.0), 1), MAX_ZOOM);
        assert!(zoom_ceiling((8000.0, 8000.0), 2) < 1.0);
        // degenerate sizes fall back to the absolute bound rather than dividing by zero
        assert_eq!(zoom_ceiling((0.0, 0.0), 1), MAX_ZOOM);
        assert_eq!(zoom_ceiling((f64::NAN, 100.0), 1), MAX_ZOOM);
    }
}
