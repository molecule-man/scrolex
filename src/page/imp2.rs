use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::subclass::prelude::*;
use std::cell::Cell;

#[derive(Debug, Default, glib::Properties)]
#[properties(wrapper_type = super::PageNumber)]
pub struct PageNumber {
    #[property(get, set)]
    page_number: Cell<i32>,

    #[property(get, set)]
    width: Cell<i32>,
}

#[glib::object_subclass]
impl ObjectSubclass for PageNumber {
    const NAME: &'static str = "PageNumber";
    type Type = super::PageNumber;
}

#[glib::derived_properties]
impl ObjectImpl for PageNumber {}
