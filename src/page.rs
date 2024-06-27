use crate::zoom::ZoomHandler;
use gtk::prelude::*;
use gtk::{glib::clone, Box, DrawingArea};
use poppler::Document;
use std::cell::RefCell;
use std::rc::Rc;
use std::usize;

pub(crate) struct PageManager {
    doc: Document,
    zoom_handler: Rc<RefCell<ZoomHandler>>,
    pages_box: Box,
    current_page: usize,
    buffer_size: usize,
    loaded_from: usize,
    loaded_to: usize,
}

impl PageManager {
    pub(crate) fn new(
        doc: Document,
        pages_box: Box,
        zoom_handler: Rc<RefCell<ZoomHandler>>,
    ) -> Self {
        PageManager {
            doc,
            zoom_handler,
            pages_box,
            current_page: 0,
            buffer_size: 10,
            loaded_from: 0,
            loaded_to: 0,
        }
    }

    pub(crate) fn reload(&mut self, doc: Document) {
        self.doc = doc;
        self.load();
    }

    pub(crate) fn load(&mut self) {
        while let Some(child) = self.pages_box.first_child() {
            self.pages_box.remove(&child);
        }

        let start = 0;
        let end = (start + self.buffer_size).min(self.doc.n_pages() as usize);
        //let end = self.doc.n_pages() as usize;

        let (width, height) = self.doc.page(start as i32).unwrap().size();
        self.zoom_handler
            .borrow_mut()
            .reset(width as i32, height as i32);

        for i in start..end {
            let page = self.new_page_widget(i);
            self.pages_box.append(&page);
        }

        self.loaded_from = start;
        self.loaded_to = end;

        // Update the adjustment values for the ScrolledWindow
        //let total_height = (self.total_pages as f64 * 100.0 * self.zoom) as i32; // Assume each page is 100px high for example
        //let adjustment = self.pages_box.vadjustment().unwrap();
        //adjustment.set_upper(total_height as f64);
        //adjustment.set_page_size((self.buffer_size as f64 * 100.0 * self.zoom) as f64); // Adjust to buffer size

        self.current_page = start;
    }

    pub(crate) fn shift_loading_buffer_right(&mut self) -> bool {
        if self.loaded_to >= self.doc.n_pages() as usize {
            return false;
        }

        self.pages_box
            .remove(&self.pages_box.first_child().unwrap());

        let new_page = self.new_page_widget(self.loaded_to);
        self.pages_box.append(&new_page);

        self.loaded_from += 1;
        self.loaded_to += 1;

        true
    }

    pub(crate) fn shift_loading_buffer_left(&mut self) -> bool {
        if self.loaded_from == 0 {
            return false;
        }

        self.pages_box.remove(&self.pages_box.last_child().unwrap());

        let new_page = self.new_page_widget(self.loaded_from - 1);
        self.pages_box.prepend(&new_page);

        self.loaded_from -= 1;
        self.loaded_to -= 1;

        true
    }

    fn new_page_widget(&mut self, i: usize) -> DrawingArea {
        let zoom_handler = self.zoom_handler.clone();

        let page = self.doc.page(i as i32).unwrap();
        let (width, height) = page.size();

        let drawing_area = DrawingArea::new();
        drawing_area.set_size_request(width as i32, height as i32);
        drawing_area.set_draw_func(
            clone!(@strong zoom_handler => move |_, cr, _width, _height| {
                let zoom = zoom_handler.borrow().zoom();
                cr.scale(zoom, zoom);
                page.render(cr);
            }),
        );

        drawing_area
    }
}
