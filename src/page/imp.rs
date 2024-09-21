#![expect(unused_lifetimes)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use gtk::gdk::BUTTON_PRIMARY;
use gtk::glib;
use gtk::glib::clone;
use gtk::glib::subclass::{prelude::*, Signal};
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;

use super::Highlighted;
use crate::poppler::{Dest, DestExt, LinkMappingExt, LinkType};

#[derive(Default, glib::Properties)]
#[properties(wrapper_type = super::Page)]
pub struct Page {
    #[property(get, set)]
    state: RefCell<crate::state::State>,

    #[property(get, set)]
    pub(crate) binding: RefCell<Option<glib::Binding>>,

    #[property(get, set)]
    popplerpage: RefCell<Option<poppler::Page>>,

    #[property(name = "x1", get, set, type = f64, member = x1)]
    #[property(name = "y1", get, set, type = f64, member = y1)]
    #[property(name = "x2", get, set, type = f64, member = x2)]
    #[property(name = "y2", get, set, type = f64, member = y2)]
    pub highlighted: RefCell<Highlighted>,

    #[property(get, set)]
    bbox: RefCell<poppler::Rectangle>,
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

        gc.connect_update(clone!(
            #[strong]
            mouse_coords,
            #[strong(rename_to = page)]
            obj,
            move |gc, seq| {
                let Some((start_x, start_y)) = *mouse_coords.borrow() else {
                    return;
                };

                let Some((end_x, end_y)) = gc.point(seq) else {
                    return;
                };

                if let Some(poppler_page) = page.popplerpage().as_ref() {
                    let mut rect = poppler::Rectangle::default();

                    let Point { x: x1, y: y1 } = to_poppler_coords(&page, start_x, start_y, None);
                    let Point { x: x2, y: y2 } = to_poppler_coords(&page, end_x, end_y, None);
                    rect.set_x1(x1);
                    rect.set_y1(y1);
                    rect.set_x2(x2);
                    rect.set_y2(y2);

                    let selected =
                        &poppler_page.selected_text(poppler::SelectionStyle::Glyph, &mut rect);

                    page.set_x1(start_x);
                    page.set_y1(start_y);
                    page.set_x2(end_x);
                    page.set_y2(end_y);

                    if let Some(selected) = selected {
                        page.clipboard().set_text(selected);
                    }

                    page.queue_draw();
                };
            }
        ));

        gc.connect_end(clone!(
            #[strong(rename_to = page)]
            obj,
            move |_, _| {
                page.set_cursor(None);
            }
        ));

        obj.add_controller(gc);
    }

    fn setup_link_handling(&self) {
        let obj = self.obj();
        let motion_controller = gtk::EventControllerMotion::new();
        motion_controller.connect_motion(clone!(
            #[strong]
            obj,
            move |_, x, y| {
                if let Some(poppler_page) = obj.popplerpage().as_ref() {
                    let (_, height) = poppler_page.size();
                    let raw_links = poppler_page.link_mapping();

                    if raw_links.is_empty() {
                        return;
                    }

                    let Point { x, y } = to_poppler_coords(&obj, x, y, Some(height));

                    for raw_link in raw_links {
                        let crate::poppler::Link(_, area) = raw_link.to_link();

                        if area.x1() <= x && x <= area.x2() && area.y1() <= y && y <= area.y2() {
                            obj.set_cursor_from_name(Some("pointer"));
                            return;
                        }
                    }

                    obj.set_cursor(None);
                }
            }
        ));
        obj.add_controller(motion_controller);

        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        gc.connect_pressed(clone!(
            #[strong]
            obj,
            move |gc, _n_press, x, y| {
                if let Some(poppler_page) = obj.popplerpage().as_ref() {
                    let (_, height) = poppler_page.size();
                    let raw_links = poppler_page.link_mapping();

                    if raw_links.is_empty() {
                        return;
                    }

                    let Point { x, y } = to_poppler_coords(&obj, x, y, Some(height));

                    for raw_link in raw_links {
                        let crate::poppler::Link(link_type, area) = raw_link.to_link();

                        if area.x1() <= x && x <= area.x2() && area.y1() <= y && y <= area.y2() {
                            match link_type {
                                LinkType::GotoNamedDest(name) => {
                                    if let Some(doc) = obj.state().doc() {
                                        let Some(dest) = doc.find_dest(&name) else {
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
                                        &uri,
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
                            return;
                        }
                    }

                    obj.set_cursor(None);
                }
            }
        ));
        obj.add_controller(gc);
    }
}

struct Point {
    x: f64,
    y: f64,
}

fn to_poppler_coords(page: &super::Page, x: f64, y: f64, page_height: Option<f64>) -> Point {
    let mut x = x / page.zoom();
    let mut y = y / page.zoom();

    if page.crop() {
        x += page.bbox().x1();
        y += page.bbox().y1();
    }

    // Adjust y for Poppler's coordinate system if page height is provided
    if let Some(height) = page_height {
        y = height - y;
    }

    Point { x, y }
}
