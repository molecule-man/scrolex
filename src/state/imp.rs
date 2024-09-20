use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
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

        //self.obj().connect_prev_page_notify(glib::clone!(
        //    #[strong(rename_to = jump_stack)]
        //    self.jump_stack,
        //    move |state| {
        //        let prev_page = state.prev_page();
        //        let page = state.page();
        //        if prev_page != page {
        //            jump_stack.borrow_mut().push(prev_page);
        //        }
        //    }
        //));
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
