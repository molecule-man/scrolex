#![expect(unused_lifetimes)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

use futures::channel::oneshot;
use gtk::cairo::{Context, ImageSurface};
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
                draw_page(&obj, cr, &imp, &buf);
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

        gc.connect_pressed(clone!(
            #[strong]
            mouse_coords,
            #[strong(rename_to = page)]
            obj,
            move |_gc, _n_press, x, y| {
                mouse_coords.replace(Some((x, y)));
                page.set_cursor_from_name(Some("crosshair"));
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

                let highlighted = Rectangle::new(start_x, start_y, end_x, end_y);

                let mut poppler_rect = poppler::Rectangle::default();
                let Point { x: x1, y: y1 } = undo_zoom_and_crop(&obj, start_x, start_y);
                let Point { x: x2, y: y2 } = undo_zoom_and_crop(&obj, end_x, end_y);
                poppler_rect.set_x1(x1);
                poppler_rect.set_y1(y1);
                poppler_rect.set_x2(x2);
                poppler_rect.set_y2(y2);

                let selected =
                    &poppler_page.selected_text(poppler::SelectionStyle::Glyph, &mut poppler_rect);

                imp.highlighted.replace(highlighted);

                if let Some(selected) = selected {
                    obj.clipboard().set_text(selected);
                }

                obj.queue_draw();
            }
        ));

        let obj = self.obj().clone();
        gc.connect_end(move |_, _| {
            obj.set_cursor(None);
        });

        self.obj().add_controller(gc);
    }

    fn setup_link_handling(&self) {
        let obj = self.obj();
        let motion_controller = gtk::EventControllerMotion::new();

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
                    obj.set_cursor_from_name(Some("pointer"));
                    return;
                }

                obj.set_cursor(None);
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
                            println!("unhandled link: {msg:?}");
                        }
                        LinkType::Invalid => {
                            println!("invalid link: {link_type:?}");
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
}

fn draw_page(obj: &super::Page, cr: &Context, imp: &Page, buf: &Rc<RefCell<Option<Box<[u8]>>>>) {
    let Some(poppler_page) = obj.state().doc().and_then(|doc| doc.page(obj.index())) else {
        return;
    };

    let uri = obj.uri();
    let page_num = poppler_page.index();

    cr.save().expect("Failed to save");

    //let bbox = imp.get_bbox(&poppler_page, obj.crop());
    //let (width, height) = poppler_page.size();
    //let scale = obj.zoom();
    //
    //if bbox.x1 != 0.0 || bbox.y1 != 0.0 {
    //    cr.translate(-bbox.x1 * scale, -bbox.y1 * scale);
    //}
    //
    //cr.rectangle(0.0, 0.0, width * scale, height * scale);
    //cr.scale(scale, scale);
    //cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    //cr.fill().expect("Failed to fill");
    //poppler_page.render(cr);
    //imp.resize();

    let (width, height) = poppler_page.size();
    let scale = obj.zoom();

    let scale_factor = obj.scale_factor() as f64;
    let (canvas_width, canvas_height) =
        (width * scale * scale_factor, height * scale * scale_factor);
    let stride = 4 * canvas_width as i32; // ARGB32 has 4 bytes per pixel

    if let Some(buffer) = buf.take() {
        if buffer.len() != (stride * canvas_height as i32) as usize {
            println!("Buffer size mismatch. Requesting new rendering.");
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
                gtk::cairo::Format::ARgb32,
                canvas_width as i32,
                canvas_height as i32,
                stride,
            )
            .expect("Failed to create image surface");
            let bbox = imp.get_bbox(&poppler_page, obj.crop());
            draw_surface(cr, &surface, &bbox, scale, scale_factor);
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
                println!("Page {page_num} not drawable anymore. Aborting.");
                //buf.replace(None);
                return;
            }

            let Ok(buffer) = buffer else {
                if obj_clone.is_drawable() {
                    println!("Page {page_num} is still drawable. Rescheduling.");
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
        RENDER_QUEUE.with(move |queue| {
            queue.execute(
                &uri.clone(),
                Box::new(move |doc| {
                    if let Ok(doc) = doc {
                        request_render(
                            &uri,
                            doc,
                            scale,
                            scale_factor,
                            page_num,
                            canvas_width,
                            canvas_height,
                            stride,
                            resp_sender,
                        );
                    } else {
                        resp_sender.send(Err(())).expect("Failed to send buffer");
                        //handle.abort();
                        //println!("Aborted rendering of page {page_num}");
                    }
                }),
            );
        });

        let bbox = imp.get_cached_bbox(&poppler_page, obj.crop());
        let (w, h) = bbox.size();
        cr.rectangle(0.0, 0.0, w * scale, h * scale);
        //cr.scale(scale, scale);
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.fill().expect("Failed to fill");

        //let downscale = 1.0 / 32.0;
        //let surface = render(&poppler_page, scale * downscale);
        //draw_surface(cr, &surface, &bbox, scale, downscale);
    }

    //imp.resize();

    cr.restore().expect("Failed to restore");

    let highlighted = &imp.highlighted.borrow();

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

pub fn draw_surface(
    cr: &Context,
    surface: &ImageSurface,
    bbox: &Rectangle,
    scale: f64,
    scale_factor: f64,
) {
    cr.scale(1.0 / scale_factor, 1.0 / scale_factor);
    //let bbox = imp.get_bbox(poppler_page, obj.crop());
    cr.set_source_surface(
        surface,
        -bbox.x1 * scale * scale_factor,
        -bbox.y1 * scale * scale_factor,
    )
    .unwrap();
    let (w, h) = bbox.size();
    cr.rectangle(0.0, 0.0, w * scale * scale_factor, h * scale * scale_factor);
    cr.clip();
    cr.paint().unwrap();

    // Release the surface data
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
}

fn request_render(
    uri: &str,
    doc: &poppler::Document,
    scale: f64,
    scale_factor: f64,
    page_num: i32,
    canvas_width: f64,
    canvas_height: f64,
    stride: i32,
    resp_sender: oneshot::Sender<Result<Box<[u8]>, ()>>,
) {
    let Some(page) = doc.page(page_num) else {
        todo!("Page not found");
    };

    let scale = scale * scale_factor;
    let surface = render(uri, &page, scale);

    let mut buffer = vec![0u8; (stride * canvas_height as i32) as usize];
    surface
        .with_data(|data| {
            buffer.copy_from_slice(data);
        })
        .expect("Failed to extract surface data");
    surface.finish();
    drop(surface);
    resp_sender
        .send(Ok(buffer.into_boxed_slice()))
        .expect("Failed to send buffer");
}

pub fn render(uri: &str, page: &poppler::Page, scale: f64) -> ImageSurface {
    let (width, height) = page.size();
    let (canvas_width, canvas_height) = (width * scale, height * scale);

    let surface = ImageSurface::create(
        gtk::cairo::Format::ARgb32,
        canvas_width as i32,
        canvas_height as i32,
    )
    .expect("Couldn't create a surface!");
    let cr = Context::new(&surface).expect("Couldn't create a context!");
    cr.rectangle(0.0, 0.0, canvas_width, canvas_height);
    cr.scale(scale, scale);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.fill().expect("Failed to fill");

    if page.index() == 15 {
        //crate::cpp_render_page(page, &cr);
        let uri_without_scheme = uri.split_once(':').map(|(_, rest)| rest).unwrap_or(uri);
        crate::cpp_render_doc_page(uri_without_scheme, page.index(), &cr);
    } else {
        page.render(&cr);
    };

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
        assert!((bbox.y1 - 1.0).abs() < EPSILON); // 6.5 - 0.5 - 5
        assert!((bbox.x2 - 243.5).abs() < EPSILON); // 238.0 + 0.5 + 5
        assert!((bbox.y2 - 47.0).abs() < EPSILON); // 41.5 + 0.5 + 5
    }

    #[test]
    fn test_get_bbox_with_big_margins() {
        let content = String::from_utf8_lossy(SMALL_PDF).replace("{BBOX}", "10 34 20 43.5");
        let doc = poppler::Document::from_data(content.as_bytes(), None).unwrap();
        let page = doc.page(0).unwrap();
        let bbox = get_bbox(&page, true);

        assert!((bbox.x1 - 4.5).abs() < EPSILON); // 10.0 - 0.5 - 5
        assert!((bbox.y1 - 24.0).abs() < EPSILON);
        assert!((bbox.x2 - 129.5).abs() < EPSILON); // 4.5 + 250 / 2
        assert!((bbox.y2 - 49.0).abs() < EPSILON);
    }
}
