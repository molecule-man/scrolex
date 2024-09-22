#![expect(unused_lifetimes)]

use std::cell::{Cell, RefCell};
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
    index: Cell<i32>,

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
        obj.set_draw_func(clone!(
            #[strong]
            obj,
            move |_, cr, _width, _height| {
                let Some(poppler_page) = obj.state().doc().and_then(|doc| doc.page(obj.index()))
                else {
                    return;
                };

                cr.save().expect("Failed to save");
                crate::render::RENDERER.with(|r| {
                    r.render(cr, &obj, &poppler_page);
                    r.resize(&obj, &poppler_page);
                });
                cr.restore().expect("Failed to restore");

                let highlighted = &obj.imp().highlighted.borrow();

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
    }

    fn setup_state_listeners(&self) {
        let obj = self.obj().clone();
        obj.property_expression("state")
            .chain_property::<crate::state::State>("crop")
            .watch(gtk::Widget::NONE, move || obj.imp().trigger_resize());

        let obj = self.obj().clone();
        obj.property_expression("state")
            .chain_property::<crate::state::State>("zoom")
            .watch(gtk::Widget::NONE, move || obj.imp().trigger_resize());
    }

    pub(super) fn trigger_resize(&self) {
        let obj = self.obj();
        if let Some(page) = self.poppler_page() {
            crate::render::RENDERER.with(|r| {
                r.resize(&obj, &page);
            });
        }
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
            move |gc, seq| {
                let Some((start_x, start_y)) = *mouse_coords.borrow() else {
                    return;
                };

                let Some((end_x, end_y)) = gc.point(seq) else {
                    return;
                };

                if let Some(poppler_page) = obj.imp().poppler_page() {
                    let mut rect = poppler::Rectangle::default();

                    let Point { x: x1, y: y1 } = to_poppler_coords(&obj, start_x, start_y, None);
                    let Point { x: x2, y: y2 } = to_poppler_coords(&obj, end_x, end_y, None);
                    rect.set_x1(x1);
                    rect.set_y1(y1);
                    rect.set_x2(x2);
                    rect.set_y2(y2);

                    let selected =
                        &poppler_page.selected_text(poppler::SelectionStyle::Glyph, &mut rect);

                    obj.set_x1(start_x);
                    obj.set_y1(start_y);
                    obj.set_x2(end_x);
                    obj.set_y2(end_y);

                    if let Some(selected) = selected {
                        obj.clipboard().set_text(selected);
                    }

                    obj.queue_draw();
                };
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
            move |_, x, y| {
                if let Some(poppler_page) = obj.imp().poppler_page() {
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
                if let Some(poppler_page) = obj.imp().poppler_page() {
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
