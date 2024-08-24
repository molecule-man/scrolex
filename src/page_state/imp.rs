use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use std::cell::{Cell, RefCell};

#[derive(Debug, Default, glib::Properties)]
#[properties(wrapper_type = super::PageState)]
pub struct PageState {
    #[property(get, set)]
    zoom: Cell<f64>,

    #[property(get, set)]
    crop: Cell<bool>,

    #[property(get, set)]
    doc: RefCell<Option<poppler::Document>>,
}

#[glib::object_subclass]
impl ObjectSubclass for PageState {
    const NAME: &'static str = "PageState";
    type Type = super::PageState;
}

#[glib::derived_properties]
impl ObjectImpl for PageState {}
