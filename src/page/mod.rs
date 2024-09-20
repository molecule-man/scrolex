mod imp;
mod page_number_imp;

use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};
use std::cell::RefCell;
use std::rc::Rc;

use crate::render::Renderer;

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
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub(crate) fn bind(
        &self,
        pn: &PageNumber,
        poppler_page: &poppler::Page,
        renderer: &Rc<RefCell<Renderer>>,
    ) {
        self.set_popplerpage(poppler_page.clone());

        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            prev_binding.unbind();
        }

        let new_binding = self
            .bind_property("width-request", pn, "width")
            .sync_create()
            .build();

        self.imp().binding.replace(Some(new_binding));

        self.set_draw_func(clone!(
            #[strong(rename_to = page)]
            self,
            #[strong]
            renderer,
            #[strong]
            poppler_page,
            move |_, cr, _width, _height| {
                cr.save().expect("Failed to save");
                renderer.borrow().render(cr, &page, &poppler_page);
                cr.restore().expect("Failed to restore");

                renderer.borrow().resize(&page, &poppler_page);

                let highlighted = &page.imp().highlighted.borrow();

                if highlighted.x2 - highlighted.x1 > 0.0 && highlighted.y2 - highlighted.y1 > 0.0 {
                    cr.set_source_rgba(0.5, 0.8, 0.9, 0.3);
                    cr.rectangle(
                        highlighted.x1,
                        highlighted.y1,
                        highlighted.x2 - highlighted.x1,
                        highlighted.y2 - highlighted.y1,
                    );
                    cr.fill().expect("Failed to fill");
                }
            }
        ));

        renderer.borrow().resize(self, poppler_page);
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
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}
