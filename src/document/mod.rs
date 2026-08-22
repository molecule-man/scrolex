// Document loading and the render bookkeeping the panes share.
mod imp;

use crate::state::preview_cache_budget;
use futures::channel::oneshot;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::clone;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::*;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use crate::page;

const MAX_MAIN_THREAD_RENDER_TIME: Duration = Duration::from_millis(100);

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
    pub struct Document(ObjectSubclass<imp::Document>);
}

impl Document {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    pub(crate) fn record_main_thread_render(&self, elapsed: Duration) -> bool {
        let mut recent = self.imp().slow_main_thread_renders.get();
        recent.rotate_left(1);
        recent[2] = elapsed > MAX_MAIN_THREAD_RENDER_TIME;

        let use_workers = recent.into_iter().filter(|slow| *slow).count() >= 2;
        self.imp()
            .slow_main_thread_renders
            .set(if use_workers { [false; 3] } else { recent });
        use_workers
    }

    pub(crate) fn tallest_page_height(&self) -> f64 {
        self.imp().tallest_page_height.get()
    }

    pub(crate) fn page_size(&self, index: i32) -> Option<crate::mupdf_render::PageSize> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.imp().page_sizes.borrow().get(index).copied().flatten())
    }

    pub fn load(&self, f: &gtk::gio::File) {
        let seq = self.imp().load_seq.get().wrapping_add(1);
        self.imp().load_seq.set(seq);

        let uri = f.uri();
        let size_bytes = document_size_bytes(f);
        self.emit_by_name::<()>("load-started", &[]);

        // Stage and open the document off the main thread so a heavy file doesn't freeze the UI.
        // A failed open leaves the current document (and its in-flight render markers) intact,
        // since nothing below the commit runs until the open succeeds. Staging fetches a remote
        // file exactly once; those bytes are the ones committed for rendering - no re-fetch.
        let (tx, rx) = oneshot::channel::<
            Option<(
                crate::mupdf_render::Candidate,
                crate::mupdf_render::DocumentInfo,
            )>,
        >();
        let uri_probe = uri.clone();
        std::thread::spawn(move || {
            let probed = crate::mupdf_render::stage_candidate(&uri_probe).and_then(|candidate| {
                let info = candidate.probe()?;
                (!info.page_sizes.is_empty()).then_some((candidate, info))
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
                let Some((candidate, info)) = probed else {
                    state.emit_by_name::<()>(
                        "load-failed",
                        &[&"could not open document".to_string()],
                    );
                    return;
                };
                state.commit_load(&uri, candidate, info, size_bytes);
            }
        ));
    }

    fn commit_load(
        &self,
        uri: &str,
        candidate: crate::mupdf_render::Candidate,
        info: crate::mupdf_render::DocumentInfo,
        size_bytes: i64,
    ) {
        let n_pages = info.page_sizes.len() as i32;
        let tallest_page_height = info.tallest_page_height();
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

        self.set_uri(uri);
        self.set_n_pages(n_pages);
        self.imp().page_sizes.replace(info.page_sizes);
        self.imp()
            .tallest_page_height
            .set(tallest_page_height.unwrap_or(0.0));
        self.imp().slow_main_thread_renders.set([false; 3]);
        self.set_multithread_rendering(false);

        log::info!(
            "Loaded document: {n_pages} pages, {size_bytes} bytes, tallest page {tallest_page_height:?} pt"
        );

        self.emit_by_name::<()>("loaded", &[]);
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

    pub(crate) fn render_client_id(&self) -> u64 {
        self.imp().render_client_id.get()
    }

    pub(crate) fn doc_epoch(&self) -> u64 {
        self.imp().doc_epoch.get()
    }

    pub(crate) fn invalidate_rendering(&self) {
        crate::page::clear_all_renders(self.render_client_id());
        self.imp()
            .doc_epoch
            .set(self.imp().doc_epoch.get().wrapping_add(1));
        self.imp().render_cache.borrow_mut().clear();
        self.imp().render_inflight.borrow_mut().clear();
        self.imp().preview_cache.borrow_mut().clear();
        self.imp().preview_inflight.borrow_mut().clear();
    }

    pub(crate) fn set_render_cache_bytes(&self, bytes: usize) {
        self.imp().render_cache.borrow_mut().set_budget(bytes);
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

    pub(crate) fn render_threads(&self) -> usize {
        self.imp().render_threads.get()
    }

    pub(crate) fn set_render_threads(&self, n: usize) {
        self.imp().render_threads.set(n);
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[gtk::test]
    fn one_slow_main_thread_render_does_not_require_workers() {
        let document = Document::new();

        assert!(!document.record_main_thread_render(Duration::from_millis(101)));
        assert!(!document.record_main_thread_render(Duration::from_millis(20)));
        assert!(!document.record_main_thread_render(Duration::from_millis(20)));
    }

    #[gtk::test]
    fn two_consecutive_slow_main_thread_renders_require_workers() {
        let document = Document::new();

        assert!(!document.record_main_thread_render(Duration::from_millis(101)));
        assert!(document.record_main_thread_render(Duration::from_millis(101)));
        assert!(!document.record_main_thread_render(Duration::from_millis(101)));
    }

    #[gtk::test]
    fn two_alternating_slow_main_thread_renders_require_workers() {
        let document = Document::new();

        assert!(!document.record_main_thread_render(Duration::from_millis(101)));
        assert!(!document.record_main_thread_render(Duration::from_millis(20)));
        assert!(document.record_main_thread_render(Duration::from_millis(101)));
    }
}
