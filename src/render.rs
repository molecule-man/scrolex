use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::thread;

use futures::channel::oneshot;
use gtk::cairo::Context;
use gtk::glib;
use poppler::Document;

struct Request {
    page_num: i32,
    uri: String,
    resp_sender: oneshot::Sender<poppler::Rectangle>,
}

pub(crate) struct Renderer {
    send: std::sync::mpsc::Sender<Request>,
    bbox_cache: Rc<RefCell<HashMap<i32, poppler::Rectangle>>>,
}

impl Renderer {
    pub(crate) fn new() -> Self {
        let (send, recv) = std::sync::mpsc::channel();
        let renderer = Renderer {
            send,
            bbox_cache: Rc::new(RefCell::new(HashMap::new())),
        };
        renderer.spawn_bg_thread(recv);
        renderer
    }

    pub(crate) fn clear_cache(&self) {
        self.bbox_cache.borrow_mut().clear();
    }

    fn get_bbox(&self, page: &poppler::Page, crop: bool) -> poppler::Rectangle {
        if !crop {
            let mut bbox = poppler::Rectangle::default();
            bbox.set_x1(0.0);
            bbox.set_y1(0.0);
            let (w, h) = page.size();
            bbox.set_x2(w);
            bbox.set_y2(h);
            return bbox;
        }
        if let Some(bbox) = self.bbox_cache.borrow().get(&page.index()) {
            return *bbox;
        }

        let bbox = get_bbox(page, true);
        self.bbox_cache.borrow_mut().insert(page.index(), bbox);
        bbox
    }

    pub(crate) fn resize(&self, page: &crate::page::Page, poppler_page: &poppler::Page) {
        let (w, h) = poppler_page.size();
        let page_num = poppler_page.index();

        if !page.crop() {
            page.resize(w, h, None);
            return;
        }

        if let Some(bbox) = self.bbox_cache.borrow().get(&poppler_page.index()) {
            page.resize(w, h, Some(*bbox));
            return;
        }

        let (resp_sender, resp_receiver) = oneshot::channel();
        self.send
            .send(Request {
                page_num,
                uri: page.uri().to_string(),
                resp_sender,
            })
            .expect("Failed to send bbox request");

        glib::spawn_future_local(glib::clone!(
            #[strong(rename_to = bbox_cache)]
            self.bbox_cache,
            #[strong]
            page,
            async move {
                let bbox = resp_receiver.await.expect("Failed to receive bbox");
                bbox_cache.borrow_mut().insert(page_num, bbox);
                page.resize(w, h, Some(bbox));
            }
        ));
    }

    pub(crate) fn render(
        &self,
        cr: &gtk::cairo::Context,
        page: &crate::page::Page,
        poppler_page: &poppler::Page,
    ) {
        let now = std::time::Instant::now();

        let bbox = self.get_bbox(poppler_page, page.crop());
        render(cr, poppler_page, page.zoom(), &bbox);
        println!(
            "Rendering from main loop {}. elapsed: {:.2?}",
            poppler_page.index(),
            now.elapsed()
        );
    }

    fn spawn_bg_thread(&self, recv: Receiver<Request>) {
        thread::spawn(move || {
            let mut doc = None;
            let mut doc_uri = String::new();

            for req in recv {
                if doc.is_none() || doc_uri != req.uri {
                    doc =
                        Some(Document::from_file(&req.uri, None).expect("Couldn't open the file!"));
                    doc_uri.clone_from(&req.uri);
                }
                let doc = doc.as_ref().unwrap();

                if let Some(page) = doc.page(req.page_num) {
                    req.resp_sender
                        .send(get_bbox(&page, true))
                        .expect("Failed to send bbox");
                }
                // TODO else
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
