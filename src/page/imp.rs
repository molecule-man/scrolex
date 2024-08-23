use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;
use std::cell::{Cell, RefCell};

#[derive(Default, glib::Properties)]
#[properties(wrapper_type = super::Page)]
pub struct Page {
    #[property(get, set)]
    poppler_page: RefCell<Option<poppler::Page>>,
    //poppler_page: Cell<i32>,
    #[property(get, set)]
    zoom: Cell<f64>,

    #[property(get, set)]
    crop: Cell<bool>,

    #[property(get, set)]
    pub binding: RefCell<Option<glib::Binding>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "HallyviewedPage";
    type Type = super::Page;
    type ParentType = DrawingArea;
}

#[glib::derived_properties]
impl ObjectImpl for Page {}
impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}
