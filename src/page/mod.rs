mod imp;
mod page_number_imp;

use gtk::gio::prelude::*;
use gtk::glib;
use gtk::subclass::prelude::ObjectSubclassIsExt;

#[derive(Default, Debug, Copy, Clone)]
pub struct Rectangle {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl Rectangle {
    pub fn new(x1: f64, y1: f64, x2: f64, y2: f64) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn from_poppler(rect: &poppler::Rectangle, height: f64) -> Self {
        Self {
            x1: rect.x1(),
            y1: height - rect.y2(),
            x2: rect.x2(),
            y2: height - rect.y1(),
        }
    }

    fn size(&self) -> (f64, f64) {
        (self.x2 - self.x1, self.y2 - self.y1)
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
        @extends gtk::DrawingArea, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Page {
    pub fn new(state: &crate::state::State) -> Self {
        glib::Object::builder().property("state", state).build()
    }

    pub(crate) fn bind(&self, pn: &PageNumber) {
        self.set_index(pn.page_number());

        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            prev_binding.unbind();
        }

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
}
