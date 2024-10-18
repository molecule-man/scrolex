mod imp;
mod page_number_imp;

pub use imp::draw_surface;
pub use imp::render_surface;

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

    pub(crate) fn as_poppler(&self) -> poppler::Rectangle {
        let mut rect = poppler::Rectangle::new();
        rect.set_x1(self.x1);
        rect.set_y1(self.y1);
        rect.set_x2(self.x2);
        rect.set_y2(self.y2);
        rect
    }

    pub(crate) fn from_poppler(rect: &poppler::Rectangle, height: f64) -> Self {
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
