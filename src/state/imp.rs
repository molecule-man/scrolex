#![expect(unused_lifetimes)]

use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    doc: RefCell<Option<poppler::Document>>,

    #[property(get, set)]
    uri: RefCell<String>,

    #[property(get, set)]
    page: Cell<u32>,

    #[property(get, set)]
    prev_page: RefCell<u32>,

    pub(super) jump_stack: Rc<RefCell<jump_stack::JumpStack>>,
    pub(crate) bbox_cache: Rc<RefCell<HashMap<i32, poppler::Rectangle>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for State {
    const NAME: &'static str = "DocState";
    type Type = super::State;
}

#[glib::derived_properties]
impl ObjectImpl for State {
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
