mod imp;
mod page_number_imp;

use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;

#[derive(Default, Debug)]
pub struct Highlighted {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
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
        self.imp().trigger_resize();
    }

    pub(crate) fn resize(
        &self,
        orig_width: f64,
        orig_height: f64,
        bbox: Option<poppler::Rectangle>,
    ) {
        let mut width = orig_width;
        let mut height = orig_height;

        if let (Some(bbox), true) = (bbox, self.crop()) {
            width = bbox.x2() - bbox.x1();
            height = bbox.y2() - bbox.y1();
            self.set_bbox(bbox);
        } else {
            let mut bbox = poppler::Rectangle::default();
            bbox.set_x1(0.0);
            bbox.set_y1(0.0);
            bbox.set_x2(width);
            bbox.set_y2(height);
            self.set_bbox(bbox);
        }

        self.set_size_request((width * self.zoom()) as i32, (height * self.zoom()) as i32);
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
