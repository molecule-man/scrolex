use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::{gio::prelude::*, glib::subclass::Signal};
use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

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
}

#[glib::object_subclass]
impl ObjectSubclass for State {
    const NAME: &'static str = "PageState";
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
