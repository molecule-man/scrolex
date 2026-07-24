#![expect(unused_lifetimes)]

use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::jump_stack;

// Source of per-window render-client ids, assigned to each State on construction.
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Default, glib::Properties)]
#[properties(wrapper_type = super::State)]
pub struct State {
    #[property(get, set)]
    zoom: Cell<f64>,

    #[property(get, set)]
    crop: Cell<bool>,

    #[property(get, set)]
    animate_scroll: Cell<bool>,

    #[property(get, set)]
    n_pages: Cell<i32>,

    #[property(get, set)]
    uri: RefCell<String>,

    #[property(get, set)]
    page: Cell<u32>,

    #[property(get, set)]
    prev_page: RefCell<u32>,

    #[property(get, set)]
    multithread_rendering: Cell<bool>,

    pub(super) jump_stack: Rc<RefCell<jump_stack::JumpStack>>,
    pub(crate) bbox_cache: Rc<RefCell<HashMap<i32, crate::page::Rectangle>>>,
    pub(crate) links: Rc<RefCell<crate::links::Links>>,
    pub(crate) search: Rc<RefCell<crate::search::Search>>,

    // rendered pages keyed by page index, kept so scrolling back to an already seen page reuses the
    // surface instead of re-rendering (and flashing white)
    pub(crate) render_cache: Rc<RefCell<crate::render_cache::RenderCache>>,
    // page indices with a render currently queued, to avoid scheduling duplicates
    pub(crate) render_inflight: Rc<RefCell<HashSet<i32>>>,
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
    // direction of travel, used to prefetch the pages being read toward: true = forward (higher page
    // numbers), the default; flipped when the user scrolls back.
    pub(crate) scroll_forward: Cell<bool>,
    // bumped on zoom (alongside the inflight/waiter clears); a render captures it at schedule and
    // drops out on completion if it changed, so an old-scale render can't cache/rebook. Per-State so
    // one window's zoom never invalidates another's in-flight renders.
    pub(crate) render_epoch: Cell<u64>,
    // this window's id in the render pool's wanted-range filter, so windows don't filter each other
    pub(crate) render_client_id: Cell<u64>,
    // bumped when this window loads/reloads a document; a render captures it at schedule and drops
    // out on completion if it changed (catches same-path reload, where the uri is unchanged).
    // Per-State so one window's load never invalidates another's in-flight renders.
    pub(crate) doc_epoch: Cell<u64>,

    // global render-thread count (user setting) and how many pages fully fit across the viewport;
    // together they set prefetch depth. Set in constructed / by the window.
    pub(crate) render_threads: Cell<usize>,
    pub(crate) visible_page_count: Cell<i32>,
}

#[glib::object_subclass]
impl ObjectSubclass for State {
    const NAME: &'static str = "DocState";
    type Type = super::State;
}

#[glib::derived_properties]
impl ObjectImpl for State {
    fn constructed(&self) {
        self.parent_constructed();
        self.render_client_id
            .set(NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed));
        // animated scrolling is on by default; the builder-created instance doesn't run State::new,
        // so set it here
        self.obj().set_animate_scroll(true);

        // Previews are tiny; give their cache its own small budget rather than the default
        // (full-render) one. Sized for the default resident-preview count; the window resizes it
        // from config. Must live here, not in State::new: the builder-created instance the window
        // uses doesn't run State::new.
        *self.preview_cache.borrow_mut() = crate::render_cache::RenderCache::new(
            super::preview_cache_budget(crate::config::DEFAULT_PREVIEW_CACHE_PAGES),
        );
        self.preview_scale.set(crate::page::PREVIEW_INITIAL_SCALE);
        self.scroll_forward.set(true);
        self.render_threads
            .set(crate::config::DEFAULT_RENDER_THREADS);

        // Zoom changes every page's render scale: drop the now-wrong-scale cache entries and queued
        // renders. Must live here, not State::new: the builder-created instance skips it.
        self.obj().connect_notify_local(Some("zoom"), |state, _| {
            let imp = state.imp();
            imp.render_cache.borrow_mut().clear();
            imp.render_inflight.borrow_mut().clear();
            imp.render_waiters.borrow_mut().clear();
            crate::page::clear_full_renders(imp.render_client_id.get());
            // in-flight renders started at the old scale are now stale; bump so their completion
            // drops out instead of caching an obsolete-scale surface
            imp.render_epoch.set(imp.render_epoch.get().wrapping_add(1));
        });
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("before-load").build(),
                Signal::builder("loaded").build(),
            ]
        })
    }
}
