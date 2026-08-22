// Document data shared by the panes that show it, and render lifecycle bookkeeping.
#![expect(unused_lifetimes)]

use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// Source of per-window render-client ids, assigned to each Document on construction.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default, glib::Properties)]
#[properties(wrapper_type = super::Document)]
pub struct Document {
    // Tallest paper height in points.
    pub(crate) tallest_page_height: Cell<f64>,

    pub(crate) page_sizes: RefCell<Vec<Option<crate::mupdf_render::PageSize>>>,

    #[property(get, set)]
    n_pages: Cell<i32>,

    #[property(get, set)]
    uri: RefCell<String>,

    #[property(get, set)]
    multithread_rendering: Cell<bool>,

    // Slow status of the last three main-thread renders.
    pub(crate) slow_main_thread_renders: Cell<[bool; 3]>,

    pub(crate) bbox_cache: Rc<RefCell<HashMap<i32, crate::page::Rectangle>>>,
    pub(crate) links: Rc<RefCell<crate::links::Links>>,
    pub(crate) search: Rc<RefCell<crate::search::Search>>,

    // Whole-page and viewport-region textures, kept so scrolling back reuses rendered pixels
    // instead of re-rendering (and flashing white).
    pub(crate) render_cache: Rc<RefCell<crate::render_cache::RenderCache>>,
    // pages with a render in flight, mapped to the render_epoch it was scheduled at. One render per
    // page at a time: a zoom leaves the entry in place, and the stale render's completion releases it
    // (see Page::schedule_render), so zooming can't stack up buffers for the same page.
    pub(crate) render_inflight: Rc<RefCell<HashMap<i32, u64>>>,
    // widget currently waiting to display each page, so a finished render repaints the right widget
    // even if list recycling moved the requester
    pub(crate) render_waiters: Rc<RefCell<HashMap<i32, glib::WeakRef<crate::page::Page>>>>,

    // low-resolution page previews rendered ahead and shown instantly (upscaled) while the full
    // render is pending, so aggressive scrolling shows blurry pages, not blank. Small budget
    // (previews are tiny), kept across zoom (they're rescaled).
    pub(crate) preview_cache: Rc<RefCell<crate::render_cache::RenderCache>>,
    pub(crate) preview_inflight: Rc<RefCell<HashSet<i32>>>,
    // disabled once we see previews aren't cheap for this document (e.g. an image whose decode
    // dominates regardless of scale) - then they'd only waste cycles. Cell wrapped so it defaults
    // to false; set true on load.
    pub(crate) preview_enabled: Cell<bool>,
    // consecutive previews slow even at min scale; disable previews once it proves the doc is decode-bound.
    pub(crate) preview_slow_streak: Cell<u32>,
    // render scale for previews, adapted per document toward the time and memory budgets. Defaults
    // to 0.0 (Cell); set to the initial scale in constructed and on load.
    pub(crate) preview_scale: Cell<f64>,
    // this window's id in the render pool's wanted-range filter, so windows don't filter each other
    pub(crate) render_client_id: Cell<u64>,
    // Bumped when the document or its rendering mode changes; a render captures it at schedule and
    // drops out on completion if it changed. Per-Document so one window never invalidates another's
    // in-flight renders.
    pub(crate) doc_epoch: Cell<u64>,

    // global render-thread count (user setting). With the pane's visible page count it sets
    // prefetch depth. Set in constructed / by the window.
    pub(crate) render_threads: Cell<usize>,

    // bumped on each load; the async open's completion drops out if it changed, so a load started
    // while an earlier one is still opening supersedes it.
    pub(crate) load_seq: Cell<u64>,
}

#[glib::object_subclass]
impl ObjectSubclass for Document {
    const NAME: &'static str = "Document";
    type Type = super::Document;
}

#[glib::derived_properties]
impl ObjectImpl for Document {
    fn constructed(&self) {
        self.parent_constructed();
        self.render_client_id
            .set(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));

        // Previews are tiny; give their cache its own small budget rather than the default
        // (full-render) one. Sized for the default resident-preview count; the window resizes it
        // from config.
        *self.preview_cache.borrow_mut() = crate::render_cache::RenderCache::new(
            crate::state::preview_cache_budget(crate::config::DEFAULT_PREVIEW_CACHE_PAGES),
        );
        self.preview_scale.set(crate::page::PREVIEW_INITIAL_SCALE);
        self.render_threads
            .set(crate::config::DEFAULT_RENDER_THREADS);
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("load-started").build(),
                Signal::builder("load-failed")
                    .param_types([String::static_type()])
                    .build(),
                Signal::builder("before-load").build(),
                Signal::builder("loaded").build(),
            ]
        })
    }
}
