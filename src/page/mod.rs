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
                    // TODO: handle crop
                    rect.set_x1(start_x / page.zoom());
                    rect.set_y1(start_y / page.zoom());
                    rect.set_x2(end_x / page.zoom());
                    rect.set_y2(end_y / page.zoom());
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
                    cr.translate((-bbox.x1() + 5.0) * zoom, (-bbox.y1() + 5.0) * zoom);
                }

                resize_page(&page, zoom, page.crop(), width, height, bbox);

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

        resize_page(self, self.zoom(), self.crop(), width, height, bbox);
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

fn resize_page(
    page_widget: &impl IsA<gtk::Widget>,
    zoom: f64,
    crop_margins: bool,
    orig_width: f64,
    orig_height: f64,
    bbox: poppler::Rectangle,
) {
    let mut width = orig_width;
    let mut height = orig_height;
    if crop_margins {
        width = bbox.x2() - bbox.x1() + 10.0;
        height = bbox.y2() - bbox.y1() + 10.0;
    }

    if width < orig_width / 2.0 {
        width = orig_width / 2.0;
    }

    if height < orig_height / 2.0 {
        height = orig_height / 2.0;
    }

    page_widget.set_size_request((width * zoom) as i32, (height * zoom) as i32);
}
