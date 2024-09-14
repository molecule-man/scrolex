mod imp;
mod page_number_imp;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gdk::BUTTON_PRIMARY;
use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};

use crate::poppler::*;
use crate::render::Renderer;

#[derive(Default, Debug)]
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
        let cursor = Rc::new(RefCell::new(None));
        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        gc.connect_pressed(clone!(
            #[strong]
            mouse_coords,
            #[strong]
            page,
            #[strong]
            cursor,
            move |_gc, _n_press, x, y| {
                mouse_coords.replace(Some((x, y)));
                cursor.replace(page.cursor());
                page.set_cursor_from_name(Some("crosshair"));
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

                if let Some(poppler_page) = page.popplerpage().as_ref() {
                    let mut rect = poppler::Rectangle::default();

                    let mut crop_x1 = 0.0;
                    let mut crop_y1 = 0.0;

                    if page.crop() {
                        let crop_bbox = page.bbox();
                        crop_x1 = crop_bbox.x1();
                        crop_y1 = crop_bbox.y1();
                    }

                    rect.set_x1(crop_x1 + start_x / page.zoom());
                    rect.set_y1(crop_y1 + start_y / page.zoom());
                    rect.set_x2(crop_x1 + end_x / page.zoom());
                    rect.set_y2(crop_y1 + end_y / page.zoom());

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
            #[strong]
            page,
            move |_, _| {
                page.set_cursor(cursor.borrow().as_ref());
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
        renderer: Rc<RefCell<Renderer>>,
        overlay: &gtk::Overlay,
        state: &crate::state::State,
        listview: &gtk::ListView,
    ) {
        self.set_popplerpage(poppler_page.clone());

        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            prev_binding.unbind();
        }

        let new_binding = self
            .bind_property("width-request", pn, "width")
            .sync_create()
            .build();

        self.imp().binding.replace(Some(new_binding));

        self.bind_draw(poppler_page, renderer);

        let mut child = overlay.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if c.type_() == gtk::Button::static_type() {
                overlay.remove_overlay(&c);
            }
            //overlay.remove(&c);
            //child = overlay.first_child();
        }

        for link in poppler_page.link_mapping() {
            let link = link.from_raw();
            match link {
                crate::poppler::Link::GotoNamedDest(name, area) => {
                    let btn = gtk::Button::builder()
                        .valign(gtk::Align::Start)
                        .halign(gtk::Align::Start)
                        .opacity(0.0)
                        .css_classes(vec!["link-overlay"])
                        .cursor(&gtk::gdk::Cursor::from_name("pointer", None).unwrap())
                        .build();

                    self.connect_zoom_notify(clone!(
                        #[strong]
                        btn,
                        move |page| {
                            update_link_location(page, &btn, &area);
                        }
                    ));

                    self.connect_bbox_notify(clone!(
                        #[strong]
                        btn,
                        move |page| {
                            update_link_location(page, &btn, &area);
                        }
                    ));

                    update_link_location(self, &btn, &area);

                    btn.connect_clicked(clone!(
                        #[strong]
                        listview,
                        #[strong]
                        state,
                        move |_| {
                            let Some(doc) = state.doc() else {
                                return;
                            };

                            let Some(dest) = doc.find_dest(&name) else {
                                return;
                            };

                            let Dest::Xyz(page_num) = dest.from_raw() else {
                                return;
                            };

                            dbg!(page_num);
                            listview.scroll_to(
                                (page_num as u32).saturating_sub(1),
                                gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                                None,
                            );
                        }
                    ));

                    overlay.add_overlay(&btn);
                }
                _ => {
                    println!("unhandled link: {:?}", link);
                }
            }
        }
    }

    fn bind_draw(&self, poppler_page: &poppler::Page, renderer: Rc<RefCell<Renderer>>) {
        self.set_draw_func(clone!(
            #[strong(rename_to = page)]
            self,
            #[strong]
            renderer,
            #[strong]
            poppler_page,
            move |_, cr, _width, _height| {
                cr.save().expect("Failed to save");
                renderer.borrow().render(cr, &page, &poppler_page);
                cr.restore().expect("Failed to restore");

                renderer.borrow().resize(&page, &poppler_page);

                let highlighted = &page.imp().highlighted.borrow();

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

        renderer.borrow().resize(self, poppler_page);
    }

    pub(crate) fn resize(
        &self,
        orig_width: f64,
        orig_height: f64,
        bbox: Option<poppler::Rectangle>,
    ) {
        let mut width = orig_width;
        let mut height = orig_height;

        match (bbox, self.crop()) {
            (Some(bbox), true) => {
                width = bbox.x2() - bbox.x1();
                height = bbox.y2() - bbox.y1();
                self.set_bbox(bbox);
            }
            _ => {
                let mut bbox = poppler::Rectangle::default();
                bbox.set_x1(0.0);
                bbox.set_y1(0.0);
                bbox.set_x2(width);
                bbox.set_y2(height);
                self.set_bbox(bbox);
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

fn update_link_location(page: &Page, btn: &gtk::Button, area: &poppler::Rectangle) {
    let (_, height) = page.popplerpage().as_ref().unwrap().size();
    btn.set_margin_start((page.zoom() * (area.x1() - page.bbox().x1())) as i32);
    btn.set_margin_top((page.zoom() * (height - area.y2() - page.bbox().y1())) as i32);
    btn.set_width_request((page.zoom() * (area.x2() - area.x1())) as i32);
    btn.set_height_request((page.zoom() * (area.y2() - area.y1())) as i32);
}
