use crate::state;
use gtk::prelude::*;
use gtk::{glib::clone, Box, DrawingArea};
use poppler::Document;
use std::cell::Cell;
use std::rc::Rc;
use std::usize;

pub(crate) struct PageManager {
    doc: Document,
    pages_box: Box,
    current_page: usize,
    buffer_size: usize,
    loaded_from: usize,
    loaded_to: usize,

    zoom: Rc<Cell<f64>>,
    width: i32,
    height: i32,
}

impl PageManager {
    pub(crate) fn new(doc: Document, pages_box: Box) -> Self {
        PageManager {
            doc,
            pages_box,
            current_page: 0,
            buffer_size: 10,
            loaded_from: 0,
            loaded_to: 0,

            zoom: Rc::new(Cell::new(1.0)),
            width: 800,
            height: 800,
        }
    }

    pub(crate) fn current_state(&self) -> state::DocumentState {
        state::DocumentState {
            zoom: self.zoom.get(),
            scroll_position: 0.0,
            start: self.loaded_from,
        }
    }

    pub(crate) fn reload(&mut self, doc: Document, state: state::DocumentState) {
        self.doc = doc;
        self.load(state);
    }

    pub(crate) fn load(&mut self, state: state::DocumentState) {
        while let Some(child) = self.pages_box.first_child() {
            self.pages_box.remove(&child);
        }

        let start = state.start;
        let end = (start + self.buffer_size).min(self.doc.n_pages() as usize);
        //let end = self.doc.n_pages() as usize;

        let (width, height) = self.doc.page(start as i32).unwrap().size();

        self.zoom.replace(state.zoom);
        self.width = width as i32;
        self.height = height as i32;

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

    fn new_page_widget(&mut self, i: usize) -> gtk::Fixed {
        let zoom = self.zoom.get();

        let page = self.doc.page(i as i32).unwrap();
        let (width, height) = page.size();

        let drawing_area = DrawingArea::new();
        drawing_area.set_size_request((width * zoom) as i32, (height * zoom) as i32);
        drawing_area.set_draw_func(
            clone!(@strong self.zoom as zoom  => move |_, cr, _width, _height| {
                let zoom = zoom.get();
                cr.scale(zoom, zoom);
                page.render(cr);
            }),
        );

        let fixed = gtk::Fixed::new();
        fixed.put(&drawing_area, 0.0, 0.0);

        fixed
    }

    pub(crate) fn apply_zoom(&mut self, zoom_factor: f64) {
        self.zoom.replace(self.zoom.get() * zoom_factor);
        let zoom = self.zoom.get();

        let mut child = self.pages_box.first_child();
        while let Some(c) = child {
            if let Some(container) = c.downcast_ref::<gtk::Fixed>() {
                container.set_size_request(
                    (self.width as f64 * zoom) as i32,
                    (self.height as f64 * zoom) as i32,
                );

                let drawing_area = container.first_child().unwrap();

                if let Some(page) = drawing_area.downcast_ref::<DrawingArea>() {
                    page.set_size_request(
                        (self.width as f64 * zoom) as i32,
                        (self.height as f64 * zoom) as i32,
                    );
                    page.queue_draw();
                }
            }

            child = c.next_sibling();
        }
    }
}
