use std::cell::Cell;
use std::rc::Rc;

use gtk::subclass::prelude::*;
use poppler::Document;
use crate::state;
use gtk::prelude::*;
use gtk::{glib, glib::Object, glib::clone, DrawingArea};

// This is the private implementation
mod imp {
    use super::*;
    use glib::subclass::Signal;
    use once_cell::sync::Lazy;

    #[derive(Default)]
    pub struct Page {
        pub index: usize,
        pub width: i32,
        pub height: i32,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Page {
        const NAME: &'static str = "Page";
        type Type = super::Page;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for Page {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![
                    glib::ParamSpecUInt::builder("index").minimum(0).maximum(u32::MAX).flags(glib::ParamFlags::READWRITE).build(),
                    glib::ParamSpecInt::builder("width").minimum(0).maximum(i32::MAX).flags(glib::ParamFlags::READWRITE).build(),
                    glib::ParamSpecInt::builder("height").minimum(0).maximum(i32::MAX).flags(glib::ParamFlags::READWRITE).build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "index" => {
                    let index = value.get().expect("The value needs to be of type `usize`.");
                    self.index = index;
                }
                "width" => {
                    let width = value.get().expect("The value needs to be of type `i32`.");
                    self.width = width;
                }
                "height" => {
                    let height = value.get().expect("The value needs to be of type `i32`.");
                    self.height = height;
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "index" => self.index.to_value(),
                "width" => self.width.to_value(),
                "height" => self.height.to_value(),
                _ => unimplemented!(),
            }
        }
    }
}

// This is the public interface
glib::wrapper! {
    pub struct Page(ObjectSubclass<imp::Page>);
}

impl Page {
    pub fn new(index: usize, width: i32, height: i32) -> Self {
        Object::new(&[("index", &index), ("width", &width), ("height", &height)])
    }

    pub fn index(&self) -> usize {
        self.property("index")
    }

    pub fn width(&self) -> i32 {
        self.property("width")
    }

    pub fn height(&self) -> i32 {
        self.property("height")
    }
}

pub(crate) struct PageManager {
    doc: Document,
    list_view: gtk::ListView,
    model: gtk::gio::ListStore,
    zoom: Rc<Cell<f64>>,
    crop_left: Rc<Cell<i32>>,
    crop_right: Rc<Cell<i32>>,
}

impl PageManager {
    pub(crate) fn new(doc: Document) -> Self {
        let model = gtk::gio::ListStore::new(Page::static_type());
        let list_view = gtk::ListView::new(None::<&gtk::NoSelection>, None::<&gtk::SignalListItemFactory>);
        
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, list_item| {
            let drawing_area = DrawingArea::new();
            list_item.set_child(Some(&drawing_area));
        });

        factory.connect_bind(clone!(@strong doc, @weak zoom, @weak crop_left => move |_, list_item| {
            let drawing_area = list_item.child().unwrap().downcast::<DrawingArea>().unwrap();
            let page = list_item.item().unwrap().downcast::<Page>().unwrap();
            
            drawing_area.set_draw_func(clone!(@strong doc, @strong zoom, @strong crop_left, @strong page => move |_, cr, width, height| {
                let zoom = zoom.get();
                let page_index = page.index();
                let poppler_page = doc.page(page_index as i32).unwrap();
                
                cr.translate(crop_left.get() as f64 * (-zoom), 0.0);
                cr.scale(zoom, zoom);
                cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                cr.rectangle(0.0, 0.0, width as f64, height as f64);
                cr.fill().expect("Failed to fill");
                poppler_page.render(cr);
            }));
            
            PageManager::resize_page(&drawing_area, &page, zoom.get(), crop_left.get(), crop_right.get());
        }));

        list_view.set_factory(Some(&factory));
        list_view.set_model(Some(&gtk::SingleSelection::new(Some(model.clone()))));

        PageManager {
            doc,
            list_view,
            model,
            zoom: Rc::new(Cell::new(1.0)),
            crop_left: Rc::new(Cell::new(0)),
            crop_right: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn load(&mut self, state: state::DocumentState) {
        self.model.remove_all();
        self.zoom.replace(state.zoom);
        self.crop_left.replace(state.crop_left);
        self.crop_right.replace(state.crop_right);

        for i in 0..self.doc.n_pages() {
            let page = self.doc.page(i).unwrap();
            let (width, height) = page.size();
            let page_model = Page::new(i as usize, width as i32, height as i32);
            self.model.append(&page_model);
        }
    }

    pub(crate) fn apply_zoom(&mut self, zoom_factor: f64) {
        self.zoom.replace(self.zoom.get() * zoom_factor);
        self.list_view.queue_draw();
    }

    pub(crate) fn adjust_crop(&mut self, left: i32, right: i32) {
        self.crop_left.replace((self.crop_left.get() + left).max(0));
        self.crop_right.replace((self.crop_right.get() + right).max(0));
        self.list_view.queue_draw();
    }

    fn resize_page(page: &gtk::DrawingArea, page_model: &Page, zoom: f64, crop_left: i32, crop_right: i32) {
        let new_width = ((page_model.width - crop_left - crop_right) as f64 * zoom) as i32;
        let new_height = (page_model.height as f64 * zoom) as i32;
        page.set_size_request(new_width, new_height);
    }
}
