mod imp;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::subclass::prelude::*;
use gtk::{glib, glib::clone};

use crate::poppler::*;
use crate::render::Renderer;

glib::wrapper! {
    pub struct PageOverlay(ObjectSubclass<imp::PageOverlay>)
        @extends gtk::Widget;
}

impl PageOverlay {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }
}

impl Default for PageOverlay {
    fn default() -> Self {
        Self::new()
    }
}
