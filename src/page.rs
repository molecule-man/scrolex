use crate::state;
use gtk::gio::prelude::*;
use gtk::glib::subclass::prelude::*;
use gtk::{glib, glib::clone, DrawingArea};
use gtk::{prelude::*, EventControllerScrollFlags};
use poppler::Document;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::usize;

pub(crate) struct PageManager {
    doc: Document,
    model: gtk::gio::ListStore,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    page_drawer: Rc<RefCell<PageDrawer>>,
}

impl PageManager {
    pub(crate) fn new(list_view: gtk::ListView, doc: Document) -> Self {
        //let model = gtk::gio::ListStore::new::<PageNumber>();
        //
        //let factory = gtk::SignalListItemFactory::new();
        //
        //let selection = gtk::SingleSelection::new(Some(model.clone()));
        //let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        //list_view.set_hexpand(true);
        //list_view.set_orientation(gtk::Orientation::Horizontal);

        let selection = list_view
            .model()
            .unwrap()
            .downcast::<gtk::SingleSelection>()
            .unwrap();
        let model = selection
            .model()
            .unwrap()
            .downcast::<gtk::gio::ListStore>()
            .unwrap();
        let factory = list_view
            .factory()
            .unwrap()
            .downcast::<gtk::SignalListItemFactory>()
            .unwrap();

        let page_drawer = Rc::new(RefCell::new(PageDrawer {
            doc: doc.clone(),
            zoom: Rc::new(Cell::new(1.0)),
            crop_left: Rc::new(Cell::new(0)),
            crop_right: Rc::new(Cell::new(0)),
            width: 800,
            height: 800,
        }));

        let scroll_controller = gtk::EventControllerScroll::new(
            EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
        );
        scroll_controller.connect_scroll(clone!(@weak list_view, @weak selection, @weak model => @default-return glib::Propagation::Stop, move |_, _dx, dy| {
            if dy < 0.0 {
                // scroll left
                list_view.scroll_to(
                    selection.selected().saturating_sub(1),
                    gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                    None,
                );
            } else {
                list_view.scroll_to(
                    (selection.selected() + 1).min(model.n_items() - 1),
                    gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                    None,
                );
            }

            glib::Propagation::Stop
        }));
        list_view.add_controller(scroll_controller);

        let pm = PageManager {
            doc: doc.clone(),
            model,
            list_view,
            page_drawer: page_drawer.clone(),
            selection,
        };

        factory.connect_bind(move |_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            let page_number = list_item.item().and_downcast::<PageNumber>().unwrap();

            let drawing_area = page_drawer
                .borrow()
                .new_drawing_area(page_number.page_number());
            list_item.set_child(Some(&drawing_area));
        });

        pm
    }

    pub(crate) fn list_view(&self) -> gtk::ListView {
        self.list_view.clone()
    }

    pub(crate) fn current_state(&self) -> state::DocumentState {
        let drawer = self.page_drawer.borrow();

        state::DocumentState {
            zoom: drawer.zoom.get(),
            crop_left: drawer.crop_left.get(),
            crop_right: drawer.crop_right.get(),
            page: self.selection.selected(),
        }
    }

    pub(crate) fn reload(&mut self, doc: Document, state: state::DocumentState) {
        self.doc = doc;
        self.load(state);
    }

    pub(crate) fn load(&mut self, state: state::DocumentState) {
        self.model.remove_all();

        let (width, height) = self.doc.page(0).unwrap().size();

        {
            let mut drawer = self.page_drawer.borrow_mut();
            drawer.doc = self.doc.clone();
            drawer.zoom.replace(state.zoom);
            drawer.width = width as i32;
            drawer.height = height as i32;
            drawer.crop_left.replace(state.crop_left);
            drawer.crop_right.replace(state.crop_right);
        }

        for i in 0..self.doc.n_pages() {
            let num = PageNumber::new(i);
            self.model.append(&num);
        }

        self.resize_all(false);

        println!(
            "loaded. scrolling to {}",
            state.page.min(self.model.n_items() - 1)
        );
        self.list_view.model().unwrap().select_item(5, true);

        let lv = self.list_view.clone();
        let scroll_to = state.page.min(self.model.n_items() - 1);

        glib::idle_add_local(move || {
            lv.scroll_to(
                scroll_to,
                gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                None,
            );
            glib::ControlFlow::Break
        });
    }

    pub(crate) fn apply_zoom(&mut self, zoom_factor: f64) {
        self.page_drawer.borrow().apply_zoom(zoom_factor);
        self.resize_all(true);
    }

    pub(crate) fn adjust_crop(&mut self, left: i32, right: i32) {
        self.page_drawer.borrow().adjust_crop(left, right);

        self.resize_all(true);
    }

    fn resize_all(&self, redraw: bool) {
        let mut child = self.list_view.first_child();
        while let Some(page) = child {
            if let Some(da) = page.first_child() {
                self.page_drawer.borrow().resize_page(&da);
                if redraw {
                    da.queue_draw();
                }
            }
            child = page.next_sibling();
        }
    }
}

struct PageDrawer {
    doc: Document,
    zoom: Rc<Cell<f64>>,
    crop_left: Rc<Cell<i32>>,
    crop_right: Rc<Cell<i32>>,
    width: i32,
    height: i32,
}

impl PageDrawer {
    fn new_drawing_area(&self, i: i32) -> gtk::DrawingArea {
        let page = self.doc.page(i).unwrap();

        let drawing_area = DrawingArea::new();
        drawing_area.set_draw_func(
            clone!(@strong self.zoom as zoom, @strong self.crop_left as crop_left => move |_, cr, width, height| {
                let zoom = zoom.get();
                cr.translate(crop_left.get() as f64 * (-zoom), 0.0);
                cr.scale(zoom, zoom);
                cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                cr.rectangle(0.0, 0.0, width as f64, height as f64);
                cr.fill().expect("Failed to fill");
                page.render(cr);
            }),
        );

        self.resize_page(&drawing_area);

        drawing_area
    }

    pub(crate) fn apply_zoom(&self, zoom_factor: f64) {
        self.zoom.replace(self.zoom.get() * zoom_factor);
    }

    pub(crate) fn adjust_crop(&self, left: i32, right: i32) {
        self.crop_left.replace((self.crop_left.get() + left).max(0));
        self.crop_right
            .replace((self.crop_right.get() + right).max(0));
    }

    fn resize_page(&self, page: &impl IsA<gtk::Widget>) {
        let zoom = self.zoom.get();

        let new_width =
            ((self.width - self.crop_left.get() - self.crop_right.get()) as f64 * zoom) as i32;
        let new_height = (self.height as f64 * zoom) as i32;

        page.set_size_request(new_width, new_height);
    }
}

glib::wrapper! {
    pub struct PageNumber(ObjectSubclass<imp::PageNumber>);
}

impl PageNumber {
    pub fn new(number: i32) -> Self {
        glib::Object::builder()
            .property("page_number", number)
            .build()
    }
}

mod imp {
    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::PageNumber)]
    pub struct PageNumber {
        #[property(get, set)]
        page_number: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PageNumber {
        const NAME: &'static str = "PageNumber";
        type Type = super::PageNumber;
    }

    #[glib::derived_properties]
    impl ObjectImpl for PageNumber {}
}
