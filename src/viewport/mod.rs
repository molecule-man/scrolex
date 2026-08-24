// One pane's viewport: zoom, position, jump history, and selection over a shared document.
mod imp;
mod position;

use crate::document::Document;
use gtk::glib;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::*;

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use position::Position;

#[cfg(test)]
pub(crate) use position::use_scratch_state_dir;

// Zoom bounds. The same for every document: huge pages are the ones that need deep zoom most.
// Render buffers are bounded by scale instead (see page::render_scale).
const MAX_ZOOM: f64 = 10.0;
const MIN_ZOOM: f64 = 0.05;

static NEXT_VIEWPORT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ViewportId(u64);

impl ViewportId {
    fn next() -> Self {
        Self(NEXT_VIEWPORT_ID.fetch_add(1, Ordering::Relaxed))
    }

    #[cfg(test)]
    pub(crate) fn from_raw(id: u64) -> Self {
        Self(id)
    }

    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

// The zoom a typed percent asks for. None below MIN_ZOOM: too small is a typo, so keep the current
// zoom instead of clamping up to it.
pub(crate) fn zoom_from_percent(percent: f64) -> Option<f64> {
    let zoom = percent / 100.0;

    (zoom >= MIN_ZOOM).then(|| zoom.min(MAX_ZOOM))
}

pub(crate) fn zoom_is_supported(zoom: f64) -> bool {
    (MIN_ZOOM..=MAX_ZOOM).contains(&zoom)
}

// Zoom as a percent for the entry, at most two decimals so that it fully fits into entry input
pub(crate) fn zoom_percent_text(zoom: f64) -> String {
    format!("{}", (zoom * 10_000.0).round() / 100.0)
}

glib::wrapper! {
    pub struct Viewport(ObjectSubclass<imp::Viewport>);
}

impl Viewport {
    pub(crate) fn new(document: &Document) -> Self {
        glib::Object::builder()
            .property("document", document)
            .property("zoom", 1.0)
            .property("crop", false)
            .property("animate_scroll", true)
            .property("page", 0_u32)
            .build()
    }

    pub(crate) fn id(&self) -> ViewportId {
        ViewportId(self.imp().id.get())
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

    fn set_bounded_zoom(&self, zoom: f64) {
        if zoom != self.zoom() {
            self.set_zoom(zoom);
        }
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
        self.sync_jump_pages();
        page
    }

    pub(crate) fn jump_list_forward(&self, current_page: u32) -> Option<u32> {
        let page = self.imp().forward_jump_stack.borrow_mut().pop();
        if page.is_some() {
            self.imp().jump_stack.borrow_mut().push(current_page);
        }
        self.sync_jump_pages();
        page
    }

    fn sync_jump_pages(&self) {
        self.set_prev_page(self.imp().jump_stack.borrow().peek().unwrap_or_default());
        self.set_next_page(
            self.imp()
                .forward_jump_stack
                .borrow()
                .peek()
                .unwrap_or_default(),
        );
    }

    // Drop what belongs to the document being closed.
    pub(crate) fn reset(&self) {
        self.imp().jump_stack.borrow_mut().reset();
        self.imp().forward_jump_stack.borrow_mut().reset();
        self.set_prev_page(0);
        self.set_next_page(0);
        self.imp().selection.replace(None);
        self.imp().current_search_result.set(None);
    }

    // Where the reader left this document, or the defaults.
    pub(crate) fn restore_position(&self) {
        let position = Position::read(&self.document().uri());
        self.zoom_to(position.zoom);
        self.set_page(position.page);
        self.set_crop(position.crop);
        log::info!(
            "Start page {}, zoom {}, crop {}",
            self.page(),
            self.zoom(),
            self.crop()
        );
    }

    pub(crate) fn save_position(&self) -> io::Result<()> {
        Position {
            zoom: self.manual_zoom(),
            page: self.page(),
            crop: self.crop(),
        }
        .write(&self.document().uri())
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

    pub(crate) fn render_epoch(&self) -> u64 {
        self.imp().render_epoch.get()
    }

    pub(crate) fn current_search_result(&self) -> Option<(i32, usize)> {
        self.imp().current_search_result.get()
    }

    pub(crate) fn set_current_search_result(&self, result: Option<(i32, usize)>) {
        self.imp().current_search_result.set(result);
    }

    pub(crate) fn scroll_forward(&self) -> bool {
        self.imp().scroll_forward.get()
    }

    pub(crate) fn set_scroll_forward(&self, forward: bool) {
        self.imp().scroll_forward.set(forward);
    }

    pub(crate) fn visible_page_count(&self) -> i32 {
        self.imp().visible_page_count.get()
    }

    pub(crate) fn set_visible_page_count(&self, count: i32) {
        self.imp().visible_page_count.set(count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page;
    use crate::viewport::use_scratch_state_dir;
    use gtk::prelude::Cast;

    fn viewport() -> Viewport {
        Viewport::new(&Document::new())
    }

    #[gtk::test]
    fn jump_history_moves_in_both_directions() {
        let viewport = viewport();
        viewport.jump_list_add(1);
        viewport.jump_list_add(2);

        assert_eq!(viewport.jump_list_back(3), Some(2));
        assert_eq!(viewport.prev_page(), 1);
        assert_eq!(viewport.next_page(), 3);

        assert_eq!(viewport.jump_list_back(2), Some(1));
        assert_eq!(viewport.prev_page(), 0);
        assert_eq!(viewport.next_page(), 2);

        assert_eq!(viewport.jump_list_forward(1), Some(2));
        assert_eq!(viewport.prev_page(), 1);
        assert_eq!(viewport.next_page(), 3);
    }

    #[test]
    fn viewports_select_search_results_independently() {
        let document = Document::new();
        let left = Viewport::new(&document);
        let right = Viewport::new(&document);

        left.set_current_search_result(Some((2, 1)));

        assert_eq!(left.current_search_result(), Some((2, 1)));
        assert_eq!(right.current_search_result(), None);
    }

    #[test]
    fn a_shared_render_keeps_the_other_viewport_interest() {
        let document = Document::new();
        let left = Viewport::new(&document);
        let right = Viewport::new(&document);
        let key = crate::document::RenderJobKey::Page(
            crate::render_cache::PageRenderKey::from_factors(2, 1.0, 1.0),
        );

        let demand = document
            .request_render(key.clone(), &left, None)
            .expect("the first request starts the job");
        assert!(document.request_render(key.clone(), &right, None).is_none());

        document.remove_render_interests(left.id());
        assert!(!demand.is_empty());
        assert!(document.has_render_job(&key));

        document.remove_render_interests(right.id());
        assert!(demand.is_empty());
        assert!(!document.has_render_job(&key));
    }

    #[gtk::test]
    fn a_page_jump_clears_forward_history() {
        let viewport = viewport();
        viewport.jump_list_add(1);
        assert_eq!(viewport.jump_list_back(2), Some(1));

        viewport.jump_list_add(1);

        assert_eq!(viewport.next_page(), 0);
        assert_eq!(viewport.jump_list_forward(3), None);
    }

    #[gtk::test]
    fn zoom_bounds_hold_whatever_the_document() {
        let viewport = viewport();
        // no page size may narrow this range
        viewport.zoom_to(50.0);
        assert_eq!(viewport.zoom(), MAX_ZOOM);
        viewport.zoom_to(0.0);
        assert_eq!(viewport.zoom(), MIN_ZOOM);
    }

    #[gtk::test]
    fn fit_zoom_does_not_replace_the_saved_manual_zoom() {
        use_scratch_state_dir();
        let viewport = viewport();
        viewport.document().set_uri("fit-zoom-test.pdf");
        viewport.zoom_to(2.0);
        viewport.fit_zoom_to(0.5);

        viewport.save_position().unwrap();

        assert_eq!(Position::read("fit-zoom-test.pdf").zoom, 2.0);
    }

    #[gtk::test]
    fn a_saved_position_comes_back_to_the_viewport() {
        use_scratch_state_dir();
        let document = Document::new();
        document.set_uri("restore-test.pdf");
        let first = Viewport::new(&document);
        first.zoom_to(2.0);
        first.set_page(4);
        first.set_crop(true);
        first.save_position().unwrap();

        let second = Viewport::new(&document);
        second.restore_position();

        assert_eq!(second.zoom(), 2.0);
        assert_eq!(second.page(), 4);
        assert!(second.crop());
    }

    // What split view needs: one document, two viewports that move on their own.
    #[gtk::test]
    fn two_viewports_share_the_document_and_keep_their_own_zoom() {
        let document = Document::new();
        document.set_n_pages(9);
        let left = Viewport::new(&document);
        let right = Viewport::new(&document);

        left.zoom_to(2.0);
        left.set_page(3);
        right.zoom_to(0.5);
        right.set_page(7);

        assert_eq!(left.zoom(), 2.0);
        assert_eq!(left.page(), 3);
        assert_eq!(right.zoom(), 0.5);
        assert_eq!(right.page(), 7);

        assert_eq!(left.document().n_pages(), 9);
        assert_eq!(right.document().n_pages(), 9);
        assert!(Rc::ptr_eq(
            &left.document().render_cache(),
            &right.document().render_cache()
        ));
    }

    #[gtk::test]
    fn a_zoom_leaves_the_other_viewport_alone() {
        let document = Document::new();
        let left = Viewport::new(&document);
        let right = Viewport::new(&document);
        let (before_left, before_right) = (left.render_epoch(), right.render_epoch());

        left.zoom_to(2.0);

        assert_eq!(left.render_epoch(), before_left + 1);
        assert_eq!(right.render_epoch(), before_right);
    }

    #[gtk::test]
    fn a_zoom_keeps_the_full_render_as_a_transition_texture() {
        let viewport = viewport();
        let bytes = glib::Bytes::from_owned(vec![255u8; 16]);
        let texture =
            gtk::gdk::MemoryTexture::new(2, 2, gtk::gdk::MemoryFormat::B8g8r8x8, &bytes, 8);
        let cache = viewport.document().render_cache();
        let render = crate::render_cache::PageRenderKey::from_factors(3, 1.0, 1.0);
        cache.borrow_mut().insert(render, texture.upcast());

        viewport.zoom_to(1.1);

        assert!(cache.borrow().contains(render));
    }

    fn selection_on(page: i32, text: &str) -> crate::selection::PageSelection {
        crate::selection::PageSelection {
            page,
            rects: vec![page::Rectangle::new(0.0, 0.0, 10.0, 10.0)],
            text: text.to_string(),
        }
    }

    // Pages announced for repaint, in order.
    fn watch_repaints(viewport: &Viewport) -> Rc<RefCell<Vec<i32>>> {
        let repainted = Rc::new(RefCell::new(Vec::new()));
        viewport.connect_closure(
            "selection-changed",
            false,
            glib::closure_local!(
                #[strong]
                repainted,
                move |_: &Viewport, page: i32| repainted.borrow_mut().push(page)
            ),
        );
        repainted
    }

    #[gtk::test]
    fn one_selection_at_a_time_and_both_pages_repaint() {
        let viewport = viewport();
        let repainted = watch_repaints(&viewport);

        viewport.set_selection(Some(selection_on(3, "first")));
        assert!(viewport.has_selection());
        assert_eq!(viewport.selected_text().as_deref(), Some("first"));
        assert_eq!(*repainted.borrow(), vec![3]);

        // page 3 loses the highlight, page 7 gains it
        viewport.set_selection(Some(selection_on(7, "second")));
        assert_eq!(
            viewport.selection().borrow().as_ref().map(|s| s.page),
            Some(7)
        );
        assert_eq!(viewport.selected_text().as_deref(), Some("second"));
        assert_eq!(*repainted.borrow(), vec![3, 3, 7]);

        // same page: one repaint
        repainted.borrow_mut().clear();
        viewport.set_selection(Some(selection_on(7, "second, longer")));
        assert_eq!(*repainted.borrow(), vec![7]);
    }

    #[gtk::test]
    fn clearing_repaints_the_page_that_held_the_selection() {
        let viewport = viewport();
        viewport.set_selection(Some(selection_on(4, "text")));
        let repainted = watch_repaints(&viewport);

        viewport.clear_selection();
        assert!(!viewport.has_selection());
        assert_eq!(viewport.selected_text(), None);
        assert_eq!(*repainted.borrow(), vec![4]);

        // clearing with nothing selected is silent
        repainted.borrow_mut().clear();
        viewport.clear_selection();
        assert!(repainted.borrow().is_empty());
    }

    #[gtk::test]
    fn an_empty_selection_has_no_text_to_copy() {
        let viewport = viewport();
        viewport.set_selection(Some(selection_on(1, "")));
        assert_eq!(viewport.selected_text(), None);
    }
}
