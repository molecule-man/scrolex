use crate::state;
use gtk::gio::prelude::*;
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::{glib, glib::clone, DrawingArea};
use poppler::Document;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::usize;

pub(crate) struct PageManager {
    doc: Document,
    uri: String,
    model: gtk::gio::ListStore,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    page_drawer: Rc<RefCell<PageDrawer>>,
}

impl PageManager {
    pub(crate) fn new(
        list_view: gtk::ListView,
        f: &gtk::gio::File,
    ) -> Result<Self, DocumentOpenError> {
        //let (doc_send, doc_recv) = std::sync::mpsc::channel();
        //let (bbox_send, bbox_recv) = std::sync::mpsc::channel();
        //
        //let doc_path = "some path";
        //
        //std::thread::spawn(move || {
        //    for doc_path in doc_recv {
        //        let doc = Document::from_file(doc_path, None).unwrap();
        //        for i in 0..doc.n_pages() {
        //            let mut rect = poppler::Rectangle::default();
        //            doc.page(i).unwrap().get_bounding_box(&mut rect);
        //            bbox_send.send(rect).unwrap();
        //        }
        //    }
        //});
        //
        //doc_send.send(doc_path).unwrap();

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

        let doc = Document::from_gfile(f, None, gtk::gio::Cancellable::NONE).map_err(|err| {
            DocumentOpenError {
                message: err.to_string(),
            }
        })?;

        let page_drawer = Rc::new(RefCell::new(PageDrawer::new(doc.clone())));

        let pm = PageManager {
            doc,
            uri: f.uri().to_string(),
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

        Ok(pm)
    }

    pub(crate) fn store_state(&self) {
        let drawer = self.page_drawer.borrow();

        if let Err(err) = state::save(
            &self.uri,
            &state::DocumentState {
                zoom: drawer.zoom.get(),
                page: self.selection.selected(),
                crop: drawer.crop.get(),
            },
        ) {
            eprintln!("Error saving state: {}", err);
        }
    }

    pub(crate) fn reset(&mut self, f: &gtk::gio::File) -> Result<(), DocumentOpenError> {
        self.store_state();

        let doc = Document::from_gfile(f, None, gtk::gio::Cancellable::NONE).map_err(|err| {
            DocumentOpenError {
                message: err.to_string(),
            }
        })?;
        self.uri = f.uri().to_string();
        self.doc = doc;

        Ok(())
    }

    pub(crate) fn load(&mut self) {
        let state = state::load(&self.uri);

        self.model.remove_all();
        {
            let mut drawer = self.page_drawer.borrow_mut();
            drawer.doc = self.doc.clone();
            drawer.zoom.replace(state.zoom);
            drawer.crop.replace(state.crop);
        }

        for i in 0..self.doc.n_pages() {
            let num = PageNumber::new(i);
            self.model.append(&num);
        }

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
        self.redraw_all();
    }

    pub(crate) fn toggle_crop(&mut self, enabled: bool) {
        self.page_drawer.borrow().crop.replace(enabled);
        self.redraw_all();
    }

    fn redraw_all(&self) {
        let mut child = self.list_view.first_child();
        while let Some(page) = child {
            if let Some(da) = page.first_child() {
                da.queue_draw();
            }
            child = page.next_sibling();
        }
    }
}

struct PageDrawer {
    doc: Document,
    zoom: Rc<Cell<f64>>,
    crop: Rc<Cell<bool>>,
}

impl PageDrawer {
    pub(crate) fn new(doc: Document) -> Self {
        PageDrawer {
            doc,
            zoom: Rc::new(Cell::new(1.0)),
            crop: Rc::new(Cell::new(false)),
        }
    }

    pub(crate) fn new_drawing_area(&self, i: i32) -> gtk::DrawingArea {
        let page = self.doc.page(i).unwrap();
        let (width, height) = page.size();
        let mut rect = poppler::Rectangle::default();
        page.get_bounding_box(&mut rect);
        let (x1, x2) = (rect.x1(), rect.x2());

        let drawing_area = DrawingArea::new();
        drawing_area.set_draw_func(clone!(
            #[strong(rename_to = zoom)]
            self.zoom,
            #[strong(rename_to = crop)]
            self.crop,
            #[strong]
            page,
            move |da, cr, _width, _height| {
                let zoom = zoom.get();

                if crop.get() {
                    let mut rect = poppler::Rectangle::default();
                    page.get_bounding_box(&mut rect);
                    cr.translate((-x1 + 5.0) * zoom, 0.0);
                }

                resize_page(da, zoom, crop.get(), width, height, x1, x2);

                cr.rectangle(0.0, 0.0, width * zoom, height * zoom);
                cr.scale(zoom, zoom);
                cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                cr.fill().expect("Failed to fill");
                page.render(cr);
            }
        ));

        resize_page(
            &drawing_area,
            self.zoom.get(),
            self.crop.get(),
            width,
            height,
            x1,
            x2,
        );

        drawing_area
    }

    pub(crate) fn apply_zoom(&self, zoom_factor: f64) {
        self.zoom.replace(self.zoom.get() * zoom_factor);
    }
}

fn resize_page(
    page_widget: &impl IsA<gtk::Widget>,
    zoom: f64,
    crop_margins: bool,
    width: f64,
    height: f64,
    x1: f64,
    x2: f64,
) {
    let mut width = width;
    if crop_margins {
        width = x2 - x1 + 10.0;
    }

    page_widget.set_size_request((width * zoom) as i32, (height * zoom) as i32);
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

#[derive(Debug, Clone)]
pub(crate) struct DocumentOpenError {
    message: String,
}

impl std::fmt::Display for DocumentOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Error opening document: {}", self.message)
    }
}

impl std::error::Error for DocumentOpenError {}
