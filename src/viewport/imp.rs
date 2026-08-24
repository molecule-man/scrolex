// One pane's view of a document: where it looks, how far it zooms, what it selected.
#![expect(unused_lifetimes)]

use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

use crate::jump_stack;

#[derive(Debug, Default, glib::Properties)]
#[properties(wrapper_type = super::Viewport)]
pub struct Viewport {
    pub(crate) id: Cell<u64>,

    // The document this pane reads. Set once, at construction.
    #[property(get = Self::document, set, construct_only, type = crate::document::Document)]
    document: RefCell<Option<crate::document::Document>>,

    #[property(get, set)]
    zoom: Cell<f64>,

    #[property(get, set)]
    crop: Cell<bool>,

    // Fit the paper to the viewport height, and keep it fitted as the viewport changes. The
    // intent lives here; the view owns the viewport and works out the zoom it means.
    #[property(get, set)]
    fit_height: Cell<bool>,

    // Zoom that the reader selected.
    pub(crate) manual_zoom: Cell<f64>,

    #[property(get, set)]
    animate_scroll: Cell<bool>,

    #[property(get, set)]
    page: Cell<u32>,

    #[property(get, set)]
    prev_page: RefCell<u32>,

    #[property(get, set)]
    next_page: RefCell<u32>,

    pub(super) jump_stack: Rc<RefCell<jump_stack::JumpStack>>,
    pub(super) forward_jump_stack: Rc<RefCell<jump_stack::JumpStack>>,
    pub(crate) selection: Rc<RefCell<Option<crate::selection::PageSelection>>>,
    pub(crate) current_search_result: Cell<Option<(i32, usize)>>,

    // direction of travel, used to prefetch the pages being read toward: true = forward (higher page
    // numbers), the default; flipped when the user scrolls back.
    pub(crate) scroll_forward: Cell<bool>,

    // bumped on zoom; a render captures it at schedule and drops out on completion if it changed, so
    // an old-scale render can't cache/rebook. Per pane, so one pane's zoom never invalidates
    // another's in-flight renders.
    pub(crate) render_epoch: Cell<u64>,

    // how many pages fully fit across this pane; with the render-thread count it sets prefetch depth.
    pub(crate) visible_page_count: Cell<i32>,
}

impl Viewport {
    fn document(&self) -> crate::document::Document {
        self.document
            .borrow()
            .clone()
            .expect("a viewport has a document")
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Viewport {
    const NAME: &'static str = "Viewport";
    type Type = super::Viewport;
}

#[glib::derived_properties]
impl ObjectImpl for Viewport {
    fn constructed(&self) {
        self.parent_constructed();
        self.id.set(super::ViewportId::next().0);
        self.scroll_forward.set(true);

        // Zoom changes every page's render scale. Keep completed textures as bounded transition
        // images, but drop queued renders whose scale is now wrong. In-flight markers stay: their
        // renders are still running and holding buffers, so the page waits for that completion
        // rather than starting a second render alongside it.
        self.obj()
            .connect_notify_local(Some("zoom"), |viewport, _| {
                let document = viewport.document();
                document.remove_render_interests(viewport.id());
                document
                    .imp()
                    .render_cache
                    .borrow_mut()
                    .clear_pins(viewport.id());
                crate::page::clear_full_renders(viewport.id());
                // in-flight renders started at the old scale are now stale; bump so their completion
                // drops out instead of caching an obsolete-scale texture
                let epoch = viewport.imp().render_epoch.get();
                viewport.imp().render_epoch.set(epoch.wrapping_add(1));
            });
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            // a page that gained or lost the selection highlight and needs repainting
            vec![Signal::builder("selection-changed")
                .param_types([i32::static_type()])
                .build()]
        })
    }
}
