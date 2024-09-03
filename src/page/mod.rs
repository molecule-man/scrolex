mod imp;
mod page_number_imp;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gdk::BUTTON_PRIMARY;
use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};

#[derive(Default)]
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
        let page: Page = glib::Object::builder().build();

        page.connect_crop_notify(|p| {
            p.queue_draw();
        });

        page.connect_zoom_notify(|p| {
            p.queue_draw();
        });

        let mouse_coords = Rc::new(RefCell::new(None));
        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();
        gc.connect_pressed(clone!(
            #[strong]
            mouse_coords,
            move |_gc, _n_press, x, y| {
                mouse_coords.replace(Some((x, y)));
            }
        ));

        gc.connect_update(clone!(
            #[strong]
            mouse_coords,
            #[strong]
            page,
            move |gc, seq| {
                let Some((start_x, start_y)) = *mouse_coords.borrow() else {
                    return;
                };

                let Some((end_x, end_y)) = gc.point(seq) else {
                    return;
                };

                if let Some(poppler_page) = page.imp().popplerpage.borrow().as_ref() {
                    let mut rect = poppler::Rectangle::default();

                    let mut crop_x1 = 0.0;
                    let mut crop_y1 = 0.0;

                    if page.crop() {
                        let crop_bbox = page.imp().crop_bbox.borrow();
                        crop_x1 = crop_bbox.x1();
                        crop_y1 = crop_bbox.y1();
                    }

                    rect.set_x1(crop_x1 + start_x / page.zoom());
                    rect.set_y1(crop_y1 + start_y / page.zoom());
                    rect.set_x2(crop_x1 + end_x / page.zoom());
                    rect.set_y2(crop_y1 + end_y / page.zoom());

                    let selected =
                        &poppler_page.selected_text(poppler::SelectionStyle::Glyph, &mut rect);

                    page.imp().highlighted.replace(Highlighted {
                        x1: rect.x1(),
                        y1: rect.y1(),
                        x2: rect.x2(),
                        y2: rect.y2(),
                    });

                    if let Some(selected) = selected {
                        page.clipboard().set_text(selected);
                    }

                    page.queue_draw();
                };
            }
        ));

        page.add_controller(gc);

        page.set_size_request(600, 800);

        page
    }

    pub fn bind(&self, pn: &PageNumber, poppler_page: &poppler::Page) {
        self.imp().popplerpage.replace(Some(poppler_page.clone()));

        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            prev_binding.unbind();
        }

        let new_binding = self
            .bind_property("width-request", pn, "width")
            .sync_create()
            .build();

        self.imp().binding.replace(Some(new_binding));

        self.bind_draw(poppler_page);
    }

    fn bind_draw(&self, poppler_page: &poppler::Page) {
        let (width, height) = poppler_page.size();

        let mut bbox = poppler::Rectangle::default();
        poppler_page.get_bounding_box(&mut bbox);

        let mut crop_bbox = poppler::Rectangle::new();
        crop_bbox.set_x1((bbox.x1() - 5.0).max(0.0));
        crop_bbox.set_y1((bbox.y1() - 5.0).max(0.0));
        crop_bbox.set_x2((bbox.x2() + 5.0).min(width));
        crop_bbox.set_y2((bbox.y2() + 5.0).min(height));

        if crop_bbox.x2() - crop_bbox.x1() < width / 2.0 {
            crop_bbox.set_x2(crop_bbox.x1() + width / 2.0);
        }
        if crop_bbox.y2() - crop_bbox.y1() < height / 2.0 {
            crop_bbox.set_y2(crop_bbox.y1() + height / 2.0);
        }

        self.imp().crop_bbox.replace(crop_bbox);

        self.set_draw_func(clone!(
            #[strong(rename_to = page)]
            self,
            #[strong]
            poppler_page,
            #[strong]
            page,
            move |_, cr, _width, _height| {
                let zoom = page.zoom();

                if page.crop() {
                    cr.translate(-crop_bbox.x1() * zoom, -crop_bbox.y1() * zoom);
                }

                page.resize(width, height, crop_bbox);

                cr.rectangle(0.0, 0.0, width * zoom, height * zoom);
                cr.scale(zoom, zoom);
                cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                cr.fill().expect("Failed to fill");
                poppler_page.render(cr);

                let highlighted = &page.imp().highlighted.borrow();

                if highlighted.x2 - highlighted.x1 > 0.0 && highlighted.y2 - highlighted.y1 > 0.0 {
                    cr.set_source_rgba(1.0, 1.0, 0.0, 0.5);
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

        self.resize(width, height, crop_bbox);
    }

    fn resize(&self, orig_width: f64, orig_height: f64, bbox: poppler::Rectangle) {
        let mut width = orig_width;
        let mut height = orig_height;
        if self.crop() {
            width = bbox.x2() - bbox.x1();
            height = bbox.y2() - bbox.y1();
        }

        self.set_size_request((width * self.zoom()) as i32, (height * self.zoom()) as i32);
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}
