use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;
use std::cell::Cell;

#[derive(Debug, Default, glib::Properties)]
#[properties(wrapper_type = super::PageNumber)]
pub struct PageNumber {
    #[property(get, set)]
    page_number: Cell<i32>,
}

#[glib::object_subclass]
impl ObjectSubclass for PageNumber {
    const NAME: &'static str = "PageNumber";
    type Type = super::PageNumber;
}

#[glib::derived_properties]
impl ObjectImpl for PageNumber {}

#[derive(Default)]
pub struct Page;

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "HallyviewedPage";
    type Type = super::Page;
    type ParentType = DrawingArea;
}

impl ObjectImpl for Page {}
impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}
