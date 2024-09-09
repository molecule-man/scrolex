mod imp;
mod page_number_imp;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::{sync, thread};

use futures::channel::oneshot;
use gtk::cairo::{Context, ImageSurface};
use gtk::gdk::BUTTON_PRIMARY;
use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};
use poppler::Document;

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
            p.rebind_draw();
            p.queue_draw();
        });

        page.connect_zoom_notify(|p| {
            p.rebind_draw();
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

    pub(crate) fn bind(
        &self,
        pn: &PageNumber,
        poppler_page: &poppler::Page,
        render_req_sender: Sender<RenderRequest>,
    ) {
        self.imp().popplerpage.replace(Some(poppler_page.clone()));
        self.imp()
            .render_req_sender
            .replace(Some(render_req_sender.clone()));

        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            prev_binding.unbind();
        }

        let new_binding = self
            .bind_property("width-request", pn, "width")
            .sync_create()
            .build();

        self.imp().binding.replace(Some(new_binding));

        self.bind_draw(poppler_page, &render_req_sender);
    }

    fn rebind_draw(&self) {
        let render_req_sender = self.imp().render_req_sender.borrow();
        let Some(render_req_sender) = render_req_sender.as_ref() else {
            return;
        };

        if let Some(poppler_page) = self.imp().popplerpage.borrow().as_ref() {
            self.bind_draw(poppler_page, render_req_sender);
        }
    }

    fn bind_draw(&self, poppler_page: &poppler::Page, render_req_sender: &Sender<RenderRequest>) {
        let (width, height) = poppler_page.size();
        let page_num = poppler_page.index();

        let (render_resp_sender, render_resp_receiver) = oneshot::channel();

        render_req_sender
            .send(RenderRequest {
                uri: self.uri(),
                page_num,
                resp_sender: render_resp_sender,
                zoom: self.zoom(),
                crop: self.crop(),
            })
            .expect("Failed to send render request");

        glib::spawn_future_local(clone!(
            #[strong(rename_to = page)]
            self,
            async move {
                let (canvas_width, canvas_height) = (width * page.zoom(), height * page.zoom());

                println!("Waiting for render response... page number {}", page_num);
                let render_response = render_resp_receiver
                    .await
                    .expect("Failed to receive rendered data");
                println!("Received render response! Page number {}", page_num);

                let rendered_data = render_response.data;

                let stride = 4 * canvas_width as i32;

                // Create an ImageSurface from the received pixel buffer
                let surface = ImageSurface::create_for_data(
                    rendered_data,
                    gtk::cairo::Format::ARgb32,
                    canvas_width as i32,
                    canvas_height as i32,
                    stride,
                )
                .expect("Failed to create image surface");

                page.set_draw_func(clone!(
                    #[strong]
                    page,
                    move |_, cr, _width, _height| {
                        cr.set_source_surface(&surface, 0.0, 0.0).unwrap();
                        cr.paint().unwrap();
                        page.resize(width, height, Some(render_response.crop_bbox));
                    }
                ));
            }
        ));

        self.resize(width, height, None);
    }

    fn resize(&self, orig_width: f64, orig_height: f64, bbox: Option<poppler::Rectangle>) {
        let mut width = orig_width;
        let mut height = orig_height;
        if self.crop() {
            if let Some(bbox) = bbox {
                width = bbox.x2() - bbox.x1();
                height = bbox.y2() - bbox.y1();
            }
        }

        self.set_size_request((width * self.zoom()) as i32, (height * self.zoom()) as i32);
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct RenderRequest {
    pub uri: String,
    pub page_num: i32,
    pub crop: bool,
    pub zoom: f64,
    pub resp_sender: oneshot::Sender<RenderResponse>,
}

#[derive(Debug)]
pub(crate) struct RenderResponse {
    pub data: Vec<u8>,
    pub crop_bbox: poppler::Rectangle,
}

pub(crate) fn spawn_pdf_renderer(render_req_reciever: Receiver<RenderRequest>) {
    thread::spawn(move || {
        let mut doc = None;
        let mut doc_uri = String::new();

        for req in render_req_reciever {
            if doc.is_none() || doc_uri != req.uri {
                doc = Some(Document::from_file(&req.uri, None).expect("Couldn't open the file!"));
                doc_uri.clone_from(&req.uri);
            }
            let doc = doc.as_ref().unwrap();

            if let Some(page) = doc.page(req.page_num) {
                let (width, height) = page.size();
                let (canvas_width, canvas_height) = (width * req.zoom, height * req.zoom);
                let zoom = req.zoom;

                // Create a pixel buffer for rendering
                let stride = 4 * canvas_width as i32; // ARGB32 has 4 bytes per pixel
                                                      // Create a temporary Cairo ImageSurface to render the page
                let surface = ImageSurface::create(
                    gtk::cairo::Format::ARgb32,
                    canvas_width as i32,
                    canvas_height as i32,
                )
                .expect("Couldn't create a surface!");
                let cairo_context = Context::new(&surface).expect("Couldn't create a context!");

                let mut crop_bbox = poppler::Rectangle::default();
                crop_bbox.set_x1(0.0);
                crop_bbox.set_y1(0.0);
                crop_bbox.set_x2(width);
                crop_bbox.set_y2(height);

                if req.crop {
                    let mut bbox = poppler::Rectangle::default();
                    page.get_bounding_box(&mut bbox);

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

                    cairo_context.translate(-crop_bbox.x1() * req.zoom, -crop_bbox.y1() * req.zoom);
                }

                cairo_context.rectangle(0.0, 0.0, canvas_width, canvas_height);
                cairo_context.scale(zoom, zoom);
                cairo_context.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                cairo_context.fill().expect("Failed to fill");

                // Render the Poppler page into the Cairo surface
                page.render(&cairo_context);

                // Now extract the pixel data from the surface
                let mut buffer = vec![0u8; (stride * canvas_height as i32) as usize];
                surface
                    .with_data(|data| {
                        // Copy the rendered pixel data into the buffer
                        buffer.copy_from_slice(data);
                    })
                    .expect("Failed to extract surface data");

                // Send the rendered buffer back to the main thread
                req.resp_sender
                    .send(RenderResponse {
                        data: buffer,
                        crop_bbox,
                    })
                    .expect("Failed to send rendered data");
            }
            // TODO else
        }
    });
}
