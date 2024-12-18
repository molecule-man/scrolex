#![expect(unused_lifetimes)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

use futures::channel::oneshot;
use gtk::cairo::{Context, ImageSurface};
use gtk::gdk::prelude::*;
use gtk::gdk::BUTTON_PRIMARY;
use gtk::glib::clone;
use gtk::glib::subclass::{prelude::*, Signal};
use gtk::glib::{self, Priority};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;
use once_cell::sync::Lazy;

use super::Rectangle;
use crate::bg_job::DebouncingJobQueue;
use crate::poppler::{Dest, DestExt, LinkType};

thread_local!(
    static RENDER_QUEUE: Lazy<DebouncingJobQueue> = Lazy::new(|| {
        DebouncingJobQueue::new(
            1,
            //Duration::from_millis(10),
        )
    });
);

#[derive(Default, glib::Properties)]
#[properties(wrapper_type = super::Page)]
pub struct Page {
    #[property(get, set)]
    state: RefCell<crate::state::State>,

    #[property(get, set)]
    pub(crate) binding: RefCell<Option<glib::Binding>>,

    #[property(get, set)]
    index: Cell<i32>,

    highlighted: RefCell<Rectangle>,
    bbox: RefCell<Rectangle>,
    cursor_guard: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for Page {
    const NAME: &'static str = "Page";
    type Type = super::Page;
    type ParentType = DrawingArea;
}

#[glib::derived_properties]
impl ObjectImpl for Page {
    fn constructed(&self) {
        self.parent_constructed();

        self.setup_draw_function();
        self.setup_state_listeners();
        self.setup_text_selection();
        self.setup_link_handling();

        self.obj().set_size_request(600, 800);
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![Signal::builder("named-link-clicked")
                .param_types([i32::static_type()])
                .build()]
        })
    }
}

impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}

impl Page {
    fn setup_draw_function(&self) {
        let obj = self.obj();
        let buf = Rc::new(RefCell::new(None::<Box<[u8]>>));

        let buf_clone = Rc::clone(&buf);
        obj.connect_unmap(move |_| {
            buf_clone.replace(None);
        });

        obj.set_draw_func(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |_, cr, _width, _height| {
                let Some(poppler_page) = obj.state().doc().and_then(|doc| doc.page(obj.index()))
                else {
                    return;
                };

                cr.save().expect("Failed to save");

                if obj.state().multithread_rendering() {
                    imp.multithread_render_to_cairo(cr, &poppler_page, &buf);
                } else {
                    imp.render_to_cairo(cr, &poppler_page);
                }

                cr.restore().expect("Failed to restore");

                let highlighted = &imp.highlighted.borrow();
                let (w, h) = highlighted.size();

                if w * w > 0.0 && h * h > 0.0 {
                    imp.render_selection_overlay(cr, &poppler_page, highlighted);
                }
            }
        ));
    }

    fn setup_state_listeners(&self) {
        let obj = self.obj().clone();
        obj.property_expression("state")
            .chain_property::<crate::state::State>("crop")
            .watch(gtk::Widget::NONE, move || obj.imp().resize());

        let obj = self.obj().clone();
        obj.property_expression("state")
            .chain_property::<crate::state::State>("zoom")
            .watch(gtk::Widget::NONE, move || obj.imp().resize());
    }

    pub(super) fn resize(&self) {
        let Some(poppler_page) = self.poppler_page() else {
            return;
        };
        let page = self.obj().clone();
        let (w, h) = poppler_page.size();

        self.get_bbox_async(
            &poppler_page,
            page.crop(),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move |bbox| {
                    let bbox = if page.crop() {
                        *bbox
                    } else {
                        Rectangle::new(0.0, 0.0, w, h)
                    };

                    imp.bbox.replace(bbox);
                    let (w, h) = bbox.size();
                    page.set_size_request((w * page.zoom()) as i32, (h * page.zoom()) as i32);
                }
            ),
        );
    }

    fn poppler_page(&self) -> Option<poppler::Page> {
        let obj = self.obj();
        self.obj()
            .state()
            .doc()
            .and_then(|doc| doc.page(obj.index()))
    }

    fn setup_text_selection(&self) {
        let obj = self.obj();
        let mouse_coords = Rc::new(RefCell::new(None));
        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        // indicates that we have "borrowed" global page cursor
        let cursor = Rc::new(Cell::new(false));

        gc.connect_pressed(clone!(
            #[strong]
            mouse_coords,
            #[strong(rename_to = page)]
            obj,
            #[weak(rename_to = imp)]
            self,
            #[strong]
            cursor,
            move |_gc, _n_press, x, y| {
                mouse_coords.replace(Some((x, y)));
                if !imp.cursor_guard.get() {
                    page.set_cursor_from_name(Some("text"));
                    imp.cursor_guard.set(true);
                    cursor.set(true);
                }
            }
        ));

        let obj = self.obj().clone();
        gc.connect_update(clone!(
            #[strong]
            mouse_coords,
            #[weak(rename_to = imp)]
            self,
            move |gc, seq| {
                let Some((start_x, start_y)) = *mouse_coords.borrow() else {
                    return;
                };
                let Some((end_x, end_y)) = gc.point(seq) else {
                    return;
                };
                let Some(poppler_page) = imp.poppler_page() else {
                    return;
                };

                let Point { x: x1, y: y1 } = undo_zoom_and_crop(&obj, start_x, start_y);
                let Point { x: x2, y: y2 } = undo_zoom_and_crop(&obj, end_x, end_y);
                let highlighted = Rectangle::new(x1, y1, x2, y2);
                imp.highlighted.replace(highlighted);

                let selected = &poppler_page.selected_text(
                    poppler::SelectionStyle::Glyph,
                    &mut highlighted.as_poppler(),
                );

                if let Some(selected) = selected {
                    obj.clipboard().set_text(selected);
                }

                obj.queue_draw();
            }
        ));

        let obj = self.obj().clone();
        gc.connect_end(move |_, _| {
            if Cell::get(&cursor) {
                cursor.set(false);
                obj.set_cursor(None);
                obj.imp().cursor_guard.set(false);
            }
        });

        self.obj().add_controller(gc);
    }

    fn setup_link_handling(&self) {
        let obj = self.obj();
        let motion_controller = gtk::EventControllerMotion::new();

        // indicates that we have "borrowed" global page cursor
        let cursor = Cell::new(false);

        motion_controller.connect_motion(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |_, x, y| {
                let Some(poppler_page) = imp.poppler_page() else {
                    return;
                };

                let Point { x, y } = undo_zoom_and_crop(&obj, x, y);
                if imp
                    .state
                    .borrow()
                    .imp()
                    .links
                    .borrow_mut()
                    .get_link(&poppler_page, x, y)
                    .is_some()
                {
                    if !imp.cursor_guard.get() {
                        obj.set_cursor_from_name(Some("pointer"));
                        imp.cursor_guard.set(true);
                        cursor.set(true);
                    }
                    return;
                }

                if Cell::get(&cursor) {
                    obj.set_cursor(None);
                    imp.cursor_guard.set(false);
                    cursor.set(false);
                }
            }
        ));
        obj.add_controller(motion_controller);

        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        gc.connect_pressed(clone!(
            #[strong]
            obj,
            #[weak(rename_to = imp)]
            self,
            move |gc, _n_press, x, y| {
                let Some(poppler_page) = imp.poppler_page() else {
                    return;
                };

                let Point { x, y } = undo_zoom_and_crop(&obj, x, y);

                if let Some(link_type) =
                    imp.state
                        .borrow()
                        .imp()
                        .links
                        .borrow_mut()
                        .get_link(&poppler_page, x, y)
                {
                    match link_type {
                        LinkType::GotoNamedDest(name) => {
                            if let Some(doc) = obj.state().doc() {
                                let Some(dest) = doc.find_dest(name) else {
                                    return;
                                };

                                let Dest::Xyz(page_num) = dest.to_dest() else {
                                    return;
                                };

                                gc.set_state(gtk::EventSequenceState::Claimed); // stop the event from propagating
                                obj.emit_by_name::<()>("named-link-clicked", &[&page_num]);
                            }
                        }
                        LinkType::Uri(uri) => {
                            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                                uri,
                                gtk::gio::AppLaunchContext::NONE,
                            );
                        }
                        LinkType::Unknown(msg) => {
                            log::warn!("unhandled link: {msg:?}");
                        }
                        LinkType::Invalid => {
                            log::warn!("invalid link: {link_type:?}");
                        }
                    }
                };
            }
        ));
        obj.add_controller(gc);
    }

    fn get_bbox(&self, page: &poppler::Page, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        let bbox = get_bbox(page, true);
        self.state
            .borrow()
            .bbox_cache()
            .borrow_mut()
            .insert(page.index(), bbox);
        bbox
    }

    fn get_cached_bbox(&self, page: &poppler::Page, crop: bool) -> Rectangle {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            return bbox;
        }

        let (w, h) = page.size();
        Rectangle::new(0.0, 0.0, w, h)
    }

    fn get_bbox_async<F>(&self, page: &poppler::Page, crop: bool, cb: F)
    where
        F: FnOnce(&Rectangle) + 'static,
    {
        if let Some(bbox) = self.lookup_bbox(page, crop) {
            cb(&bbox);
            return;
        }
        let bbox_cache = self.state.borrow().bbox_cache().clone();

        let uri = self.obj().uri();
        let page_num = page.index();
        let (resp_sender, resp_receiver) = oneshot::channel();
        crate::bg_job::execute(
            &uri,
            Box::new(move |doc| {
                if let Some(page) = doc.page(page_num) {
                    let bbox = get_bbox(&page, true);
                    resp_sender.send(bbox).expect("Failed to send bbox");
                }
            }),
        );

        glib::spawn_future_local(async move {
            let bbox = resp_receiver.await.expect("Failed to receive bbox");
            bbox_cache.borrow_mut().insert(page_num, bbox);
            cb(&bbox);
        });
    }

    fn lookup_bbox(&self, page: &poppler::Page, crop: bool) -> Option<Rectangle> {
        if !crop {
            let (w, h) = page.size();
            return Some(Rectangle::new(0.0, 0.0, w, h));
        }
        self.state
            .borrow()
            .bbox_cache()
            .borrow()
            .get(&page.index())
            .copied()
    }

    fn render_to_cairo(&self, cr: &Context, poppler_page: &poppler::Page) {
        let start = std::time::Instant::now();
        let obj = self.obj();
        let (width, height) = poppler_page.size();
        let scale_factor = obj.scale_factor() as f64;

        // surface has to be created anew because existing surface created with different scale
        // factor has different size
        let surface = ImageSurface::create(
            gtk::cairo::Format::Rgb24,
            (width * scale_factor) as i32,
            (height * scale_factor) as i32,
        )
        .expect("Failed to create image surface");
        cr.set_source_surface(surface, 0., 0.).unwrap();

        let bbox = self.get_bbox(poppler_page, obj.crop());
        let scale = obj.zoom();

        if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
            cr.translate(-bbox.x1 * scale, -bbox.y1 * scale);
        }

        cr.rectangle(0.0, 0.0, width * scale, height * scale);
        //cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.fill().expect("Failed to fill");

        cr.scale(scale, scale);

        poppler_page.render(cr);

        let elapsed = start.elapsed();
        log::trace!(
            "Rendered page {} with multithreading disabled in {elapsed:?}",
            poppler_page.index()
        );

        if elapsed > std::time::Duration::from_millis(100) {
            log::warn!("Rendering took too long: {elapsed:?}. Switching to multithreading mode.");
            obj.state().set_multithread_rendering(true);
        }
    }

    fn render_selection_overlay(
        &self,
        cr: &Context,
        poppler_page: &poppler::Page,
        rect: &Rectangle,
    ) {
        let start = std::time::Instant::now();

        let bbox = self.get_bbox(poppler_page, self.obj().crop());
        let scale = self.obj().zoom();

        let (w, h) = poppler_page.size();
        let mask = ImageSurface::create(gtk::cairo::Format::ARgb32, w as i32, h as i32)
            .expect("Failed to create mask surface");
        let mask_cr = Context::new(&mask).expect("Failed to create mask context");
        poppler_page.render_selection(
            &mask_cr,
            &mut rect.as_poppler(),
            &mut poppler::Rectangle::new(),
            poppler::SelectionStyle::Glyph,
            &mut poppler::Color::new(),
            &mut poppler::Color::new(),
        );

        if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
            cr.translate(-bbox.x1 * scale, -bbox.y1 * scale);
        }
        cr.scale(scale, scale);
        cr.set_source_rgba(0.5, 0.8, 0.9, 0.7);
        cr.mask_surface(&mask, 0.0, 0.0)
            .expect("Failed to mask surface");

        let elapsed = start.elapsed();
        log::trace!("Rendered selection {} in {elapsed:?}", poppler_page.index());
    }

    fn multithread_render_to_cairo(
        &self,
        cr: &Context,
        poppler_page: &poppler::Page,
        buf: &Rc<RefCell<Option<Box<[u8]>>>>,
    ) {
        let obj = self.obj();
        let page_num = poppler_page.index();
        log::trace!("Rendering page {page_num} with multithreading enabled");

        let (width, height) = poppler_page.size();
        let scale = obj.zoom();
        let scale_factor = obj.scale_factor() as f64;
        let (canvas_width, canvas_height) =
            (width * scale * scale_factor, height * scale * scale_factor);
        let stride = 4 * canvas_width as i32; // RGB24 has 3 bytes per pixel. But why 4? TODO

        if let Some(buffer) = buf.take() {
            if buffer.len() != (stride * canvas_height as i32) as usize {
                log::info!("Buffer size mismatch. Requesting new rendering.");
                buf.replace(None);
            } else {
                buf.replace(Some(buffer));
            }
        }

        if let Some(buffer) = buf.take() {
            let return_location = Rc::new(RefCell::new(None));
            {
                let holder = DataHolder {
                    data: Some(buffer),
                    return_location: Rc::clone(&return_location),
                };
                // Create an ImageSurface from the received pixel buffer
                let surface = ImageSurface::create_for_data(
                    holder,
                    gtk::cairo::Format::Rgb24,
                    canvas_width as i32,
                    canvas_height as i32,
                    stride,
                )
                .expect("Failed to create image surface");
                surface.set_device_scale(scale_factor, scale_factor);
                let bbox = self.get_bbox(poppler_page, obj.crop());
                draw_surface(cr, &surface, &bbox, scale);
            }

            assert!(return_location.borrow().is_some());
            buf.replace(return_location.take());
        } else {
            let (resp_sender, resp_receiver) = oneshot::channel();
            let buf = Rc::clone(buf);
            let obj_clone = obj.clone();
            glib::spawn_future_local(async move {
                let buffer = resp_receiver.await.expect("Failed to receive buffer");
                if !obj_clone.is_drawable() {
                    log::debug!("Page {page_num} not drawable anymore. Aborting.");
                    //buf.replace(None);
                    return;
                }

                let Ok(buffer) = buffer else {
                    if obj_clone.is_drawable() {
                        log::debug!("Page {page_num} is still drawable. Rescheduling.");
                        glib::idle_add_local_full(Priority::HIGH, move || {
                            obj_clone.queue_draw();
                            glib::ControlFlow::Break
                        });
                    }
                    return;
                };

                let prev = buf.replace(Some(buffer));
                if prev.is_none() {
                    glib::idle_add_local_full(Priority::HIGH, move || {
                        obj_clone.queue_draw();
                        glib::ControlFlow::Break
                    });
                }
            });
            let uri = obj.uri();
            RENDER_QUEUE.with(move |queue| {
                queue.execute(
                    &uri,
                    Box::new(move |doc| {
                        if let Ok(doc) = doc {
                            request_render(doc, scale, scale_factor, page_num, resp_sender);
                        } else {
                            resp_sender.send(Err(())).expect("Failed to send buffer");
                        }
                    }),
                );
            });

            let bbox = self.get_cached_bbox(poppler_page, obj.crop());
            let (w, h) = bbox.size();
            cr.rectangle(0.0, 0.0, w * scale, h * scale);
            //cr.scale(scale, scale);
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            cr.fill().expect("Failed to fill");
        }
    }

    pub fn render_surface(&self, cr: &Context, surface: &ImageSurface, bbox: &Rectangle) {
        draw_surface(cr, surface, bbox, self.obj().zoom());
    }
}

pub fn draw_surface(cr: &Context, surface: &ImageSurface, bbox: &Rectangle, scale: f64) {
    cr.scale(1.0, 1.0);
    cr.set_source_surface(surface, -bbox.x1 * scale, -bbox.y1 * scale)
        .unwrap();
    let (w, h) = bbox.size();
    cr.rectangle(0.0, 0.0, w * scale, h * scale);
    cr.clip();
    cr.paint().unwrap();

    // Release the surface data
    cr.set_source_rgb(0.0, 0.0, 0.0);
}

fn request_render(
    doc: &poppler::Document,
    scale: f64,
    device_scale_factor: f64,
    page_num: i32,
    resp_sender: oneshot::Sender<Result<Box<[u8]>, ()>>,
) {
    let Some(page) = doc.page(page_num) else {
        todo!("Page not found");
    };

    let surface = render_surface(&page, scale, device_scale_factor);

    let mut buffer = vec![0u8; (surface.stride() * surface.height()) as usize];
    surface
        .with_data(|data| {
            buffer.copy_from_slice(data);
        })
        .expect("Failed to extract surface data");
    surface.finish();
    resp_sender
        .send(Ok(buffer.into_boxed_slice()))
        .expect("Failed to send buffer");
}

pub fn render_surface(page: &poppler::Page, scale: f64, device_scale_factor: f64) -> ImageSurface {
    let (width, height) = page.size();
    let scale_factor = device_scale_factor * scale;
    let (canvas_width, canvas_height) = (width * scale_factor, height * scale_factor);

    let surface = ImageSurface::create(
        gtk::cairo::Format::Rgb24,
        //gtk::cairo::Format::ARgb32,
        canvas_width as i32,
        canvas_height as i32,
    )
    .expect("Couldn't create a surface!");
    surface.set_device_scale(device_scale_factor, device_scale_factor);
    let cr = Context::new(&surface).expect("Couldn't create a context!");
    cr.rectangle(0.0, 0.0, canvas_width, canvas_height);
    cr.scale(scale, scale);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.fill().expect("Failed to fill");
    page.render(&cr);

    //let mut old_rect = poppler::Rectangle::new();
    //let mut rect = poppler::Rectangle::new();
    //rect.set_x1(0.0);
    //rect.set_y1(0.0);
    //rect.set_x2(width);
    //rect.set_y2(height / 2.0);
    //page.render_selection(
    //    &cr,
    //    &mut rect,
    //    &mut old_rect,
    //    poppler::SelectionStyle::Glyph,
    //    &mut poppler::Color::new(),
    //    &mut poppler::Color::new(),
    //);

    surface
}

struct DataHolder {
    data: Option<Box<[u8]>>,
    return_location: Rc<RefCell<Option<Box<[u8]>>>>,
}

// This stores the pixels back into the return_location as now nothing
// references the pixels anymore
impl Drop for DataHolder {
    fn drop(&mut self) {
        *self.return_location.borrow_mut() = Some(self.data.take().expect("Holding no image"));
    }
}

// Needed for DataSurface::create_for_data() to be able to access the pixels
impl AsRef<[u8]> for DataHolder {
    fn as_ref(&self) -> &[u8] {
        self.data.as_ref().expect("Holding no image").as_ref()
    }
}

impl AsMut<[u8]> for DataHolder {
    fn as_mut(&mut self) -> &mut [u8] {
        self.data.as_mut().expect("Holding no image").as_mut()
    }
}

struct Point {
    x: f64,
    y: f64,
}

fn undo_zoom_and_crop(page: &super::Page, x: f64, y: f64) -> Point {
    let mut x = x / page.zoom();
    let mut y = y / page.zoom();

    if page.crop() {
        x += page.imp().bbox.borrow().x1;
        y += page.imp().bbox.borrow().y1;
    }

    Point { x, y }
}

fn get_bbox(page: &poppler::Page, crop: bool) -> Rectangle {
    let (width, height) = page.size();
    let mut bbox = poppler::Rectangle::default();
    bbox.set_x1(0.0);
    bbox.set_y1(0.0);
    bbox.set_x2(width);
    bbox.set_y2(height);

    if crop {
        let mut poppler_bbox = poppler::Rectangle::default();
        page.get_bounding_box(&mut poppler_bbox);

        bbox.set_x1(poppler_bbox.x1() - 5.0);
        bbox.set_x2(poppler_bbox.x2() + 5.0);

        bbox.set_y1(poppler_bbox.y1() - 5.0);
        bbox.set_y2(poppler_bbox.y2() + 5.0);

        if bbox.x2() - bbox.x1() < width / 2.0 {
            bbox.set_x2(bbox.x1() + width / 2.0);
        }
        if bbox.y2() - bbox.y1() < height / 2.0 {
            bbox.set_y2(bbox.y1() + height / 2.0);
        }

        bbox.set_x1(bbox.x1().max(0.0));
        bbox.set_y1(bbox.y1().max(0.0));
        bbox.set_x2(bbox.x2().min(width));
        bbox.set_y2(bbox.y2().min(height));
    }

    //Rectangle::from_poppler(&bbox, height)
    Rectangle::new(bbox.x1(), bbox.y1(), bbox.x2(), bbox.y2())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::env;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;

    const EPSILON: f64 = 0.0001;
    const SMALL_PDF: &[u8] = b"%PDF-1.2 \n\
9 0 obj\n<<\n>>\nstream\nBT/ 32 Tf(  YOUR TEXT HERE   )' ET\nendstream\nendobj\n\
10 0 obj\n<<\n/Subtype /Link\n/Rect [ {BBOX} ]\n/Contents (Your Annotation Text)\n\
/C [ 1 1 0 ]\n>>\nendobj\n\
4 0 obj\n<<\n/Type /Page\n/Parent 5 0 R\n/Contents 9 0 R\n/Annots [10 0 R ]\n>>\nendobj\n\
5 0 obj\n<<\n/Kids [4 0 R ]\n/Count 1\n/Type /Pages\n/MediaBox [ 0 0 250 50 ]\n>>\nendobj\n\
3 0 obj\n<<\n/Pages 5 0 R\n/Type /Catalog\n>>\nendobj\n\
trailer\n<<\n/Root 3 0 R\n>>\n\
%%EOF";

    const SMALL_RENDERABLE_PDF: &[u8] = b"%PDF-1.1
%\xc2\xa5\xc2\xb1\xc3\xab

1 0 obj
  << /Type /Catalog
     /Pages 2 0 R
  >>
endobj

2 0 obj
  << /Type /Pages
     /Kids [3 0 R]
     /Count 1
     /MediaBox [0 0 80 12]
  >>
endobj

3 0 obj
  <<  /Type /Page
      /Parent 2 0 R
      /Resources
       << /Font
           << /F1
               << /Type /Font
                  /Subtype /Type1
                  /BaseFont /Times-Roman
               >>
           >>
       >>
      /Contents 4 0 R
  >>
endobj

4 0 obj
  << /Length 55 >>
stream
  BT
    /F1 18 Tf
    0 0 Td
    (Hello World) Tj
  ET
endstream
endobj

xref
0 5
0000000000 65535 f
0000000018 00000 n
0000000077 00000 n
0000000178 00000 n
0000000457 00000 n
trailer
  <<  /Root 1 0 R
      /Size 5
  >>
startxref
565
%%EOF";

    #[test]
    fn test_get_bbox_no_crop() {
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "0 0 240 40");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, false);
        assert!((bbox.x1 - 0.0).abs() < EPSILON);
        assert!((bbox.y1 - 0.0).abs() < EPSILON);
        assert!((bbox.x2 - 250.0).abs() < EPSILON);
        assert!((bbox.y2 - 50.0).abs() < EPSILON);
    }

    #[test]
    fn test_get_bbox_with_crop() {
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "10 6.5 238 41.5");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, true);

        // [ 10 6.5 238 41.5 ]
        // corresponds to this bbox in poppler:
        // { x1: 9.5, y1: 8.0, x2: 238.5, y2: 44.0}
        // notice strange y2 and y1. Poppler uses left-bottom as origin.
        // 0.5 pixels for the border I guess.

        assert!((bbox.x1 - 4.5).abs() < EPSILON); // 10.0 - 0.5 - 5
        assert!((bbox.y1 - 3.0).abs() < EPSILON); // 50 - (41.5 + 0.5 + 5)
        assert!((bbox.x2 - 243.5).abs() < EPSILON); // 238.0 + 0.5 + 5
        assert!((bbox.y2 - 49.0).abs() < EPSILON); // 50 - (6.5 - 0.5 - 5)
    }

    #[test]
    fn test_get_bbox_with_big_margins() {
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "10 34 20 43.5");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, true);

        assert!((bbox.x1 - 4.5).abs() < EPSILON); // 10.0 - 0.5 - 5
        assert!((bbox.y1 - 1.0).abs() < EPSILON);
        assert!((bbox.x2 - 129.5).abs() < EPSILON); // 4.5 + 250 / 2
        assert!((bbox.y2 - 26.0).abs() < EPSILON);
    }

    #[gtk::test]
    fn test_render() {
        let doc = poppler::Document::from_data(SMALL_RENDERABLE_PDF, None).unwrap();

        let state = crate::state::State::new();
        let page = crate::page::Page::new(&state);
        page.state().set_doc(&doc);
        page.bind(&crate::page::PageNumber::new(0));

        let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::Rgb24, 80, 12).unwrap();
        let cr = gtk::cairo::Context::new(&surface).unwrap();

        page.imp().render_to_cairo(&cr, &doc.page(0).unwrap());
        let mut buffer = vec![0u8; (surface.stride() * surface.height()) as usize];
        surface
            .with_data(|data| {
                buffer.copy_from_slice(data);
            })
            .expect("Failed to extract surface data");
        surface.finish();

        assert_snapshot("test_render", &buffer);
    }

    #[test]
    fn test_render_surface() {
        let doc = poppler::Document::from_data(SMALL_RENDERABLE_PDF, None).unwrap();
        let surface = render_surface(&doc.page(0).unwrap(), 1.0, 1.0);

        let mut buffer = vec![0u8; (surface.stride() * surface.height()) as usize];
        surface
            .with_data(|data| {
                buffer.copy_from_slice(data);
            })
            .expect("Failed to extract surface data");
        surface.finish();

        assert_snapshot("test_render_surface", &buffer);
    }

    fn assert_snapshot(snapshot_name: &str, data: &[u8]) {
        let snapshot_dir = Path::new(".snapshots");
        let snapshot_file_path = snapshot_dir.join(format!("{snapshot_name}.snap"));

        if env::var("UPDATE_SNAP").is_ok() {
            let compressed_data = compress_data(data);

            fs::create_dir_all(snapshot_dir).expect("Failed to create snapshot directory");
            fs::write(&snapshot_file_path, compressed_data).expect("Failed to write snapshot file");

            println!("Snapshot updated.");
        } else {
            let compressed_snapshot =
                fs::read(&snapshot_file_path).expect("Failed to read snapshot file");

            let decompressed_snapshot =
                decompress_data(&compressed_snapshot).expect("Failed to decompress snapshot");

            let diff = decompressed_snapshot
                .iter()
                .zip(data.iter())
                .fold(0, |acc, (a, b)| acc + (*a as i32 - *b as i32).abs());

            assert_eq!(diff, 0)
        }
    }

    fn compress_data(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::new(9));
        encoder.write_all(data).expect("Failed to compress data");
        encoder.finish().expect("Failed to finish compression")
    }

    fn decompress_data(compressed_data: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        let mut decoder = GzDecoder::new(compressed_data);
        let mut decompressed_data = Vec::new();
        decoder.read_to_end(&mut decompressed_data)?;
        Ok(decompressed_data)
    }
}
