use super::Highlighted;
use gtk::gdk::BUTTON_PRIMARY;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::glib::clone;
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::DrawingArea;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default, glib::Properties)]
#[properties(wrapper_type = super::Page)]
pub struct Page {
    #[property(get, set)]
    zoom: Cell<f64>,

    #[property(get, set)]
    crop: Cell<bool>,

    #[property(get, set)]
    uri: RefCell<String>,

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
        let obj = self.obj();

        obj.connect_crop_notify(|p| {
            p.queue_draw();
        });

        obj.connect_zoom_notify(|p| {
            p.queue_draw();
        });

        let mouse_coords = Rc::new(RefCell::new(None));
        let cursor = Rc::new(RefCell::new(None));
        let gc = gtk::GestureClick::builder().button(BUTTON_PRIMARY).build();

        gc.connect_pressed(clone!(
            #[strong]
            mouse_coords,
            #[strong(rename_to = page)]
            obj,
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
            #[strong(rename_to = page)]
            obj,
            move |_, _| {
                page.set_cursor(cursor.borrow().as_ref());
            }
        ));

        obj.add_controller(gc);

        obj.set_size_request(600, 800);
    }
}

impl WidgetImpl for Page {}
impl DrawingAreaImpl for Page {}
