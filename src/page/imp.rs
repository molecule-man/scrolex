use super::Highlighted;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;
use std::{
    cell::{Cell, RefCell},
    sync::mpsc::Sender,
};

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

    #[property(get, set)]
    rebind_needed: Cell<bool>,

    #[property(name = "x1", get, set, type = f64, member = x1)]
    #[property(name = "y1", get, set, type = f64, member = y1)]
    #[property(name = "x2", get, set, type = f64, member = x2)]
    #[property(name = "y2", get, set, type = f64, member = y2)]
    pub highlighted: RefCell<Highlighted>,

    #[property(get, set)]
    crop_bbox: RefCell<poppler::Rectangle>,

    pub(crate) render_req_sender: RefCell<Option<Sender<super::RenderRequest>>>,

    #[property(get, set)]
    last_drawn_page: Cell<i32>,
}

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "HallyviewPage";
    type Type = super::Page;
    type ParentType = DrawingArea;
}

#[glib::derived_properties]
impl ObjectImpl for Page {}
impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}
