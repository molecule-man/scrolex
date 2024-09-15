use super::Highlighted;
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
    zoom: Cell<f64>,

    #[property(get, set)]
    crop: Cell<bool>,

    #[property(get, set)]
    uri: RefCell<String>,

    #[property(get, set)]
    pub(crate) binding: RefCell<Option<glib::Binding>>,

    #[property(get, set)]
    popplerpage: RefCell<Option<poppler::Page>>,

    #[property(name = "x1", get, set, type = f64, member = x1)]
    #[property(name = "y1", get, set, type = f64, member = y1)]
    #[property(name = "x2", get, set, type = f64, member = x2)]
    #[property(name = "y2", get, set, type = f64, member = y2)]
    pub highlighted: RefCell<Highlighted>,

    #[property(get, set)]
    bbox: RefCell<poppler::Rectangle>,
}

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "Page";
    type Type = super::Page;
    type ParentType = DrawingArea;
}

#[glib::derived_properties]
impl ObjectImpl for Page {
    fn constructed(&self) {
        self.parent_constructed();
        self.obj().setup();
    }
}

impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}
