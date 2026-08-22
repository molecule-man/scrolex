// Page widgets and shared rendering controls.
mod imp;

#[cfg(test)]
pub(crate) use imp::device_scale;
mod page_number_imp;

pub(crate) use imp::clear_all_renders;
pub(crate) use imp::clear_full_renders;
pub(crate) use imp::set_render_threads;
pub(crate) use imp::set_wanted_pages;
pub(crate) use imp::PREVIEW_INITIAL_SCALE;

use gtk::gio::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::ObjectSubclassIsExt;

#[derive(Default, Debug, Copy, Clone)]
pub struct Rectangle {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

impl Rectangle {
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        Self { x1, y1, x2, y2 }
    }

    pub(crate) fn size(&self) -> (f64, f64) {
        (self.x2 - self.x1, self.y2 - self.y1)
    }

    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x1 && x <= self.x2 && y >= self.y1 && y <= self.y2
    }
}

impl From<(f64, f64, f64, f64)> for Rectangle {
    fn from((x1, y1, x2, y2): (f64, f64, f64, f64)) -> Self {
        Self { x1, y1, x2, y2 }
    }
}

glib::wrapper! {
    pub struct PageNumber(ObjectSubclass<page_number_imp::PageNumber>);
}

impl PageNumber {
    pub fn new(number: i32) -> Self {
        glib::Object::builder()
            .property("page_number", number)
            .property("width", 100)
            .build()
    }
}

glib::wrapper! {
    pub struct Page(ObjectSubclass<imp::Page>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Page {
    pub fn new(state: &crate::state::Document) -> Self {
        glib::Object::builder().property("state", state).build()
    }

    pub(crate) fn bind(&self, pn: &PageNumber) {
        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            self.imp().unpin_render();
            prev_binding.unbind();
        }
        self.set_index(pn.page_number());

        let new_binding = self
            .bind_property("width-request", pn, "width")
            .sync_create()
            .build();

        self.imp().binding.replace(Some(new_binding));
        self.imp().resize();
    }

    pub(crate) fn crop(&self) -> bool {
        self.state().crop()
    }

    pub(crate) fn zoom(&self) -> f64 {
        self.state().zoom()
    }

    pub(crate) fn uri(&self) -> String {
        self.state().uri()
    }

    pub(crate) fn uses_tiles(&self) -> bool {
        self.imp().uses_tiles()
    }
}
