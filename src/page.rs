mod imp;
mod imp2;

use crate::{page_state, state};
use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};
use poppler::Document;
//use std::sync::mpsc;

pub(crate) struct PageManager {
    doc: Document,
    //doc_send: mpsc::Sender<String>,
    uri: String,
    model: gtk::gio::ListStore,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    page_state: page_state::PageState,
}

impl PageManager {
    pub(crate) fn new(
        list_view: gtk::ListView,
        f: &gtk::gio::File,
        page_state: page_state::PageState,
    ) -> Result<Self, DocumentOpenError> {
        //let (doc_send, doc_recv) = mpsc::channel::<String>();
        //let (bbox_send, bbox_recv) = mpsc::channel();
        //
        //std::thread::spawn(move || {
        //    for doc_path in doc_recv {
        //        let f = gtk::gio::File::for_uri(&doc_path);
        //        let doc = Document::from_gfile(&f, None, gtk::gio::Cancellable::NONE).unwrap();
        //        let mut bboxs = Vec::new();
        //        //let start = std::time::Instant::now();
        //        for i in 0..doc.n_pages() {
        //            let mut rect = poppler::Rectangle::default();
        //            doc.page(i).unwrap().get_bounding_box(&mut rect);
        //            bboxs.push(rect);
        //        }
        //        bbox_send.send(bboxs).unwrap();
        //        //println!(
        //        //    "Finished sending bounding boxes for {}. Time took: {}",
        //        //    doc_path,
        //        //    start.elapsed().as_millis(),
        //        //);
        //    }
        //});

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

        let doc = Document::from_gfile(f, None, gtk::gio::Cancellable::NONE).map_err(|err| {
            DocumentOpenError {
                message: err.to_string(),
            }
        })?;

        page_state.set_doc(doc.clone());

        //let page_drawer = Rc::new(RefCell::new(PageDrawer::new(doc.clone(), bbox_recv)));

        let pm = PageManager {
            doc,
            //doc_send,
            uri: f.uri().to_string(),
            model,
            list_view,
            selection,
            page_state: page_state.clone(),
        };

        Ok(pm)
    }

    pub(crate) fn store_state(&self) {
        if let Err(err) = state::save(
            &self.uri,
            &state::DocumentState {
                zoom: self.page_state.zoom(),
                page: self.selection.selected(),
                crop: self.page_state.crop(),
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
        self.page_state.set_doc(doc.clone());
        self.uri = f.uri().to_string();
        self.doc = doc;

        Ok(())
    }

    pub(crate) fn load(&mut self) {
        //self.doc_send.send(self.uri.clone()).unwrap();

        let state = state::load(&self.uri);
        self.page_state.set_crop(state.crop);
        self.page_state.set_zoom(state.zoom);

        self.model.remove_all();

        let vector: Vec<PageNumber> = (0..self.doc.n_pages()).map(PageNumber::new).collect();
        self.model.extend_from_slice(&vector);

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
}

//fn get_bbox(
//    bbox_store: &Rc<RefCell<Option<Vec<poppler::Rectangle>>>>,
//    page: &poppler::Page,
//    bbox_recv: &mpsc::Receiver<Vec<poppler::Rectangle>>,
//) -> poppler::Rectangle {
//    if let Some(bboxs) = bbox_store.borrow().as_ref() {
//        return bboxs[page.index() as usize];
//    }
//
//    if let Ok(bboxs) = bbox_recv.try_recv() {
//        let bbox = bboxs[page.index() as usize];
//        bbox_store.replace(Some(bboxs));
//        return bbox;
//    }
//
//    let mut rect = poppler::Rectangle::default();
//    page.get_bounding_box(&mut rect);
//    rect
//}

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
    pub struct PageNumber(ObjectSubclass<imp2::PageNumber>);
}

impl PageNumber {
    pub fn new(number: i32) -> Self {
        glib::Object::builder()
            .property("page_number", number)
            .property("width", 100)
            .build()
    }
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

glib::wrapper! {
    pub struct Page(ObjectSubclass<imp::Page>)
        @extends gtk::DrawingArea, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Page {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn bind(&self, pn: &PageNumber, poppler_page: &poppler::Page) {
        if let Some(prev_binding) = self.imp().binding.borrow_mut().take() {
            prev_binding.unbind();
        }

        let new_binding = self
            .bind_property("width-request", pn, "width")
            .sync_create()
            .build();

        self.imp().binding.replace(Some(new_binding));

        self.bind_draw(poppler_page);
    }

    fn bind_draw(&self, poppler_page: &poppler::Page) {
        let (width, height) = poppler_page.size();

        let mut rect = poppler::Rectangle::default();
        poppler_page.get_bounding_box(&mut rect);
        let (x1, x2) = (rect.x1(), rect.x2());

        self.set_draw_func(clone!(
            #[strong(rename_to = page)]
            self,
            #[strong]
            poppler_page,
            #[strong]
            page,
            move |_, cr, _width, _height| {
                let zoom = page.zoom();

                if page.crop() {
                    cr.translate((-x1 + 5.0) * zoom, 0.0);
                }

                resize_page(&page, zoom, page.crop(), width, height, x1, x2);

                cr.rectangle(0.0, 0.0, width * zoom, height * zoom);
                cr.scale(zoom, zoom);
                cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
                cr.fill().expect("Failed to fill");
                poppler_page.render(cr);
            }
        ));

        resize_page(self, self.zoom(), self.crop(), width, height, x1, x2);
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}
