mod imp;
mod page_number_imp;

use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};

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

        page.set_size_request(600, 800);

        page
    }

    pub fn bind(&self, pn: &PageNumber, poppler_page: &poppler::Page) {
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
