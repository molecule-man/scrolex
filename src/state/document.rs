// Public state API for document loading, persistence, and rendering coordination.
use super::document_imp as imp;
use super::{preview_cache_budget, Position, MAX_ZOOM, MIN_ZOOM};
use futures::channel::oneshot;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::clone;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::*;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
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

    // Apply and save a manual zoom.
    pub(crate) fn zoom_to(&self, zoom: f64) {
        let bounded = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.imp().manual_zoom.set(bounded);
        self.set_bounded_zoom(bounded);
    }

    // Apply a calculated zoom. Preserve the saved manual zoom.
    pub(crate) fn fit_zoom_to(&self, zoom: f64) {
        self.set_bounded_zoom(zoom.clamp(MIN_ZOOM, MAX_ZOOM));
    }

    pub(crate) fn manual_zoom(&self) -> f64 {
        self.imp().manual_zoom.get()
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

    fn set_bounded_zoom(&self, zoom: f64) {
        if zoom != self.zoom() {
            self.set_zoom(zoom);
        }
    }

    pub(crate) fn tallest_page_height(&self) -> f64 {
        self.imp().tallest_page_height.get()
    }

    pub(crate) fn page_size(&self, index: i32) -> Option<crate::mupdf_render::PageSize> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.imp().page_sizes.borrow().get(index).copied().flatten())
    }

    pub(crate) fn jump_list_add(&self, page: u32) {
        self.set_prev_page(page);
        self.imp().jump_stack.borrow_mut().push(page);
        self.imp().forward_jump_stack.borrow_mut().reset();
        self.set_next_page(0);
    }

    pub(crate) fn jump_list_back(&self, current_page: u32) -> Option<u32> {
        let page = self.imp().jump_stack.borrow_mut().pop();
        if page.is_some() {
            self.imp()
                .forward_jump_stack
                .borrow_mut()
                .push(current_page);
        }
        self.set_prev_page(self.imp().jump_stack.borrow().peek().unwrap_or_default());
        self.set_next_page(
            self.imp()
                .forward_jump_stack
                .borrow()
                .peek()
                .unwrap_or_default(),
        );
        page
    }

    pub(crate) fn jump_list_forward(&self, current_page: u32) -> Option<u32> {
        let page = self.imp().forward_jump_stack.borrow_mut().pop();
        if page.is_some() {
            self.imp().jump_stack.borrow_mut().push(current_page);
        }
        self.set_prev_page(self.imp().jump_stack.borrow().peek().unwrap_or_default());
        self.set_next_page(
            self.imp()
                .forward_jump_stack
                .borrow()
                .peek()
                .unwrap_or_default(),
        );
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
        self.imp().selection.replace(None);
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

        self.imp().jump_stack.borrow_mut().reset();
        self.imp().forward_jump_stack.borrow_mut().reset();
        self.set_prev_page(0);
        self.set_next_page(0);
        self.set_uri(uri);
        self.set_n_pages(n_pages);
        self.imp().page_sizes.replace(info.page_sizes);
        self.imp()
            .tallest_page_height
            .set(tallest_page_height.unwrap_or(0.0));
        self.imp().slow_main_thread_renders.set([false; 3]);
        self.set_multithread_rendering(false);

        let position = Position::read(uri);
        self.zoom_to(position.zoom);
        self.set_page(position.page);
        self.set_crop(position.crop);

        log::info!(
            "Loaded document: {n_pages} pages, {size_bytes} bytes, tallest page {tallest_page_height:?} pt, \
             start page {}, zoom {}, crop {}",
            self.page(),
            self.zoom(),
            self.crop(),
        );

        self.emit_by_name::<()>("loaded", &[]);
    }

    pub fn save(&self) -> io::Result<()> {
        Position {
            zoom: self.imp().manual_zoom.get(),
            page: self.page(),
            crop: self.crop(),
        }
        .write(&self.uri())
    }

    pub(crate) fn bbox_cache(&self) -> Rc<RefCell<HashMap<i32, page::Rectangle>>> {
        self.imp().bbox_cache.clone()
    }

    pub(crate) fn search(&self) -> Rc<RefCell<crate::search::Search>> {
        self.imp().search.clone()
    }

    pub(crate) fn selection(&self) -> Rc<RefCell<Option<crate::selection::PageSelection>>> {
        self.imp().selection.clone()
    }

    // Emits selection-changed for the page losing the highlight and the one gaining it.
    pub(crate) fn set_selection(&self, selection: Option<crate::selection::PageSelection>) {
        let page = selection.as_ref().map(|s| s.page);
        let prev_page = self.imp().selection.replace(selection).map(|s| s.page);

        if let Some(prev_page) = prev_page.filter(|prev| Some(*prev) != page) {
            self.emit_by_name::<()>("selection-changed", &[&prev_page]);
        }
        if let Some(page) = page {
            self.emit_by_name::<()>("selection-changed", &[&page]);
        }
    }

    pub(crate) fn clear_selection(&self) {
        self.set_selection(None);
    }

    pub(crate) fn has_selection(&self) -> bool {
        self.imp().selection.borrow().is_some()
    }

    // Empty text (a drag over no glyphs) reads as nothing selected.
    pub(crate) fn selected_text(&self) -> Option<String> {
        self.imp()
            .selection
            .borrow()
            .as_ref()
            .map(|selection| selection.text.clone())
            .filter(|text| !text.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::use_scratch_state_dir;
    use gtk::prelude::Cast;
    use std::time::Duration;

    #[gtk::test]
    fn one_slow_main_thread_render_does_not_require_workers() {
        let state = State::new();

        assert!(!state.record_main_thread_render(Duration::from_millis(101)));
        assert!(!state.record_main_thread_render(Duration::from_millis(20)));
        assert!(!state.record_main_thread_render(Duration::from_millis(20)));
    }

    #[gtk::test]
    fn two_consecutive_slow_main_thread_renders_require_workers() {
        let state = State::new();

        assert!(!state.record_main_thread_render(Duration::from_millis(101)));
        assert!(state.record_main_thread_render(Duration::from_millis(101)));
        assert!(!state.record_main_thread_render(Duration::from_millis(101)));
    }

    #[gtk::test]
    fn two_alternating_slow_main_thread_renders_require_workers() {
        let state = State::new();

        assert!(!state.record_main_thread_render(Duration::from_millis(101)));
        assert!(!state.record_main_thread_render(Duration::from_millis(20)));
        assert!(state.record_main_thread_render(Duration::from_millis(101)));
    }

    #[gtk::test]
    fn jump_history_moves_in_both_directions() {
        let state = State::new();
        state.jump_list_add(1);
        state.jump_list_add(2);

        assert_eq!(state.jump_list_back(3), Some(2));
        assert_eq!(state.prev_page(), 1);
        assert_eq!(state.next_page(), 3);

        assert_eq!(state.jump_list_back(2), Some(1));
        assert_eq!(state.prev_page(), 0);
        assert_eq!(state.next_page(), 2);

        assert_eq!(state.jump_list_forward(1), Some(2));
        assert_eq!(state.prev_page(), 1);
        assert_eq!(state.next_page(), 3);
    }

    #[gtk::test]
    fn a_page_jump_clears_forward_history() {
        let state = State::new();
        state.jump_list_add(1);
        assert_eq!(state.jump_list_back(2), Some(1));

        state.jump_list_add(1);

        assert_eq!(state.next_page(), 0);
        assert_eq!(state.jump_list_forward(3), None);
    }

    #[gtk::test]
    fn zoom_bounds_hold_whatever_the_document() {
        let state = State::new();
        // no page size may narrow this range
        state.zoom_to(50.0);
        assert_eq!(state.zoom(), MAX_ZOOM);
        state.zoom_to(0.0);
        assert_eq!(state.zoom(), MIN_ZOOM);
    }

    #[gtk::test]
    fn fit_zoom_does_not_replace_the_saved_manual_zoom() {
        use_scratch_state_dir();
        let state = State::new();
        state.set_uri("fit-zoom-test.pdf");
        state.zoom_to(2.0);
        state.fit_zoom_to(0.5);

        state.save().unwrap();

        assert_eq!(Position::read(&state.uri()).zoom, 2.0);
    }

    #[gtk::test]
    fn zoom_retains_full_render_as_a_transition_texture() {
        let state = State::new();
        let bytes = glib::Bytes::from_owned(vec![255u8; 16]);
        let texture =
            gtk::gdk::MemoryTexture::new(2, 2, gtk::gdk::MemoryFormat::B8g8r8x8, &bytes, 8);
        state
            .render_cache()
            .borrow_mut()
            .insert(3, texture.upcast(), 1.0);

        state.zoom_to(1.1);

        assert!(state.render_cache().borrow().contains_at_scale(3, 1.0));
    }

    fn selection_on(page: i32, text: &str) -> crate::selection::PageSelection {
        crate::selection::PageSelection {
            page,
            rects: vec![page::Rectangle::new(0.0, 0.0, 10.0, 10.0)],
            text: text.to_string(),
        }
    }

    // Pages announced for repaint, in order.
    fn watch_repaints(state: &State) -> Rc<RefCell<Vec<i32>>> {
        let repainted = Rc::new(RefCell::new(Vec::new()));
        state.connect_closure(
            "selection-changed",
            false,
            glib::closure_local!(
                #[strong]
                repainted,
                move |_: &State, page: i32| repainted.borrow_mut().push(page)
            ),
        );
        repainted
    }

    #[gtk::test]
    fn one_selection_at_a_time_and_both_pages_repaint() {
        let state = State::new();
        let repainted = watch_repaints(&state);

        state.set_selection(Some(selection_on(3, "first")));
        assert!(state.has_selection());
        assert_eq!(state.selected_text().as_deref(), Some("first"));
        assert_eq!(*repainted.borrow(), vec![3]);

        // page 3 loses the highlight, page 7 gains it
        state.set_selection(Some(selection_on(7, "second")));
        assert_eq!(state.selection().borrow().as_ref().map(|s| s.page), Some(7));
        assert_eq!(state.selected_text().as_deref(), Some("second"));
        assert_eq!(*repainted.borrow(), vec![3, 3, 7]);

        // same page: one repaint
        repainted.borrow_mut().clear();
        state.set_selection(Some(selection_on(7, "second, longer")));
        assert_eq!(*repainted.borrow(), vec![7]);
    }

    #[gtk::test]
    fn clearing_repaints_the_page_that_held_the_selection() {
        let state = State::new();
        state.set_selection(Some(selection_on(4, "text")));
        let repainted = watch_repaints(&state);

        state.clear_selection();
        assert!(!state.has_selection());
        assert_eq!(state.selected_text(), None);
        assert_eq!(*repainted.borrow(), vec![4]);

        // clearing with nothing selected is silent
        repainted.borrow_mut().clear();
        state.clear_selection();
        assert!(repainted.borrow().is_empty());
    }

    #[gtk::test]
    fn an_empty_selection_has_no_text_to_copy() {
        let state = State::new();
        state.set_selection(Some(selection_on(1, "")));
        assert_eq!(state.selected_text(), None);
    }
}
