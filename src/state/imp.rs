#![expect(unused_lifetimes)]

use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::OnceLock;

use crate::jump_stack;

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
    doc: RefCell<Option<poppler::Document>>,

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

    // rendered pages keyed by page index, kept so scrolling back to an already seen page reuses the
    // surface instead of re-rendering (and flashing white)
    pub(crate) render_cache: Rc<RefCell<crate::render_cache::RenderCache>>,
    // page indices with a render currently queued, to avoid scheduling duplicates
    pub(crate) render_inflight: Rc<RefCell<HashSet<i32>>>,
    // widget currently waiting to display each page, so a finished render repaints the right widget
    // even if list recycling moved the requester
    pub(crate) render_waiters: Rc<RefCell<HashMap<i32, glib::WeakRef<crate::page::Page>>>>,
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
        // animated scrolling is on by default; the builder-created instance doesn't run State::new,
        // so set it here
        self.obj().set_animate_scroll(true);

        // Zooming could have made the cache entries inaccurate. Drop them. This must live here
        // rather than in State::new: the builder-created instance the window uses doesn't run
        // State::new.
        self.obj().connect_notify_local(Some("zoom"), |state, _| {
            let imp = state.imp();
            imp.render_cache.borrow_mut().clear();
            imp.render_inflight.borrow_mut().clear();
            imp.render_waiters.borrow_mut().clear();
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
