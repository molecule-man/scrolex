use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::thread;

use futures::channel::oneshot;
use gtk::cairo::{Context, ImageSurface};
use gtk::glib;
use poppler::Document;

#[derive(Clone)]
pub(crate) struct PageRenderInfo {
    pub(crate) uri: String,
    pub(crate) crop: bool,
    pub(crate) zoom: f64,
    pub(crate) scale_factor: i32,
}

struct Request {
    page_num: i32,
    page_info: PageRenderInfo,
    req_type: RequestType,
}

enum RequestType {
    Render(RenderRequest),
    Bbox(BboxRequest),
}

struct RenderRequest {
    resp_sender: oneshot::Sender<RenderResponse>,
}
struct BboxRequest {
    resp_sender: oneshot::Sender<poppler::Rectangle>,
}

#[derive(Debug)]
struct RenderResponse {
    data: Vec<u8>,
    crop_bbox: poppler::Rectangle,
    canvas_width: f64,
    canvas_height: f64,
    scale_factor: i32,
}

pub(crate) struct Renderer {
    send: std::sync::mpsc::Sender<Request>,
    prerendered: Rc<RefCell<HashMap<i32, RenderResponse>>>,
}

impl Renderer {
    pub(crate) fn new() -> Self {
        let (send, recv) = std::sync::mpsc::channel();
        let renderer = Renderer {
            send,
            prerendered: Rc::new(RefCell::new(HashMap::new())),
        };
        renderer.spawn_bg_render_thread(recv);
        renderer
    }

    pub(crate) async fn get_bbox(&self, page_num: i32, uri: &str) -> poppler::Rectangle {
        let (resp_sender, resp_receiver) = oneshot::channel();
        self.send
            .send(Request {
                page_num,
                req_type: RequestType::Bbox(BboxRequest { resp_sender }),
                page_info: PageRenderInfo {
                    uri: uri.to_string(),
                    crop: true,
                    zoom: 1.0,
                    scale_factor: 1,
                },
            })
            .expect("Failed to send bbox request");

        resp_receiver.await.expect("Failed to receive bbox")
    }

    pub(crate) fn render(
        &self,
        cr: &gtk::cairo::Context,
        page: &poppler::Page,
        page_info: &PageRenderInfo,
    ) -> poppler::Rectangle {
        let now = std::time::Instant::now();
        let mut bbox = poppler::Rectangle::default();

        if let Some(resp) = self.prerendered.borrow().get(&page.index()) {
            println!("Rendering from buffer {}", page.index());
            self.render_from_buffer(cr, resp);
            bbox = resp.crop_bbox;
        } else {
            bbox = get_bbox(page, page_info.crop);
            println!("Rendering from main loop {}", page.index());
            render(cr, page, page_info.zoom, &bbox);
        }
        println!("Elapsed: {:.2?}", now.elapsed());

        let prerender_num = 3;
        let max_prerendered = 5;

        for i in -prerender_num..=prerender_num {
            let page_num = page.index() + i;
            if self.prerendered.borrow().contains_key(&page_num) {
                continue;
            }

            let (resp_sender, resp_receiver) = oneshot::channel();
            self.send
                .send(Request {
                    page_num,
                    req_type: RequestType::Render(RenderRequest { resp_sender }),
                    page_info: page_info.clone(),
                })
                .expect("Failed to send render request");

            glib::spawn_future_local(glib::clone!(
                #[strong(rename_to = prerendered)]
                self.prerendered,
                async move {
                    let resp = resp_receiver
                        .await
                        .expect("Failed to receive rendered data");
                    prerendered.borrow_mut().insert(page_num, resp);
                }
            ));
        }

        for i in [
            page.index() - max_prerendered,
            page.index() + max_prerendered,
        ] {
            self.prerendered.borrow_mut().remove(&i);
        }

        bbox
    }

    fn render_from_buffer(&self, cr: &gtk::cairo::Context, resp: &RenderResponse) {
        let rendered_data = resp.data.clone();
        let (canvas_width, canvas_height) = (resp.canvas_width, resp.canvas_height);
        let scale_factor = resp.scale_factor;

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

        cr.scale(1.0 / scale_factor as f64, 1.0 / scale_factor as f64);
        cr.set_source_surface(&surface, 0.0, 0.0).unwrap();
        cr.paint().unwrap();
    }

    fn spawn_bg_render_thread(&self, render_req_reciever: Receiver<Request>) {
        thread::spawn(move || {
            let mut doc = None;
            let mut doc_uri = String::new();

            for req in render_req_reciever {
                if doc.is_none() || doc_uri != req.page_info.uri {
                    doc = Some(
                        Document::from_file(&req.page_info.uri, None)
                            .expect("Couldn't open the file!"),
                    );
                    doc_uri.clone_from(&req.page_info.uri);
                }
                let doc = doc.as_ref().unwrap();

                if let Some(page) = doc.page(req.page_num) {
                    match req.req_type {
                        RequestType::Render(t) => {
                            let (width, height) = page.size();
                            let scale = req.page_info.zoom * req.page_info.scale_factor as f64;
                            let (canvas_width, canvas_height) = (width * scale, height * scale);

                            // Create a pixel buffer for rendering
                            let stride = 4 * canvas_width as i32; // ARGB32 has 4 bytes per pixel

                            // Create a temporary Cairo ImageSurface to render the page
                            let surface = ImageSurface::create(
                                gtk::cairo::Format::ARgb32,
                                canvas_width as i32,
                                canvas_height as i32,
                            )
                            .expect("Couldn't create a surface!");
                            let cairo_context =
                                Context::new(&surface).expect("Couldn't create a context!");

                            let crop_bbox = get_bbox(&page, req.page_info.crop);

                            render(&cairo_context, &page, scale, &crop_bbox);

                            // Now extract the pixel data from the surface
                            let mut buffer = vec![0u8; (stride * canvas_height as i32) as usize];
                            surface
                                .with_data(|data| {
                                    // Copy the rendered pixel data into the buffer
                                    buffer.copy_from_slice(data);
                                })
                                .expect("Failed to extract surface data");

                            // Send the rendered buffer back to the main thread
                            t.resp_sender
                                .send(RenderResponse {
                                    data: buffer,
                                    crop_bbox,
                                    canvas_width,
                                    canvas_height,
                                    scale_factor: req.page_info.scale_factor,
                                })
                                .expect("Failed to send rendered data");
                        }
                        RequestType::Bbox(t) => {
                            t.resp_sender
                                .send(get_bbox(&page, true))
                                .expect("Failed to send bbox");
                        }
                    }
                    // TODO else
                }
            }
        });
    }
}

fn render(cr: &Context, page: &poppler::Page, scale: f64, bbox: &poppler::Rectangle) {
    let (width, height) = page.size();

    if bbox.x1() != 0.0 || bbox.y1() != 0.0 {
        cr.translate(-bbox.x1() * scale, -bbox.y1() * scale);
    }

    cr.rectangle(0.0, 0.0, width * scale, height * scale);
    cr.scale(scale, scale);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.fill().expect("Failed to fill");
    page.render(cr);
}

pub(crate) fn get_bbox(page: &poppler::Page, crop: bool) -> poppler::Rectangle {
    let (width, height) = page.size();
    let mut crop_bbox = poppler::Rectangle::default();
    crop_bbox.set_x1(0.0);
    crop_bbox.set_y1(0.0);
    crop_bbox.set_x2(width);
    crop_bbox.set_y2(height);

    if crop {
        let mut bbox = poppler::Rectangle::default();
        page.get_bounding_box(&mut bbox);

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
    }

    crop_bbox
}
