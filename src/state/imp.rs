use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use std::cell::{Cell, RefCell};

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
impl ObjectImpl for State {}
