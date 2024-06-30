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
    crop_left: Rc<Cell<i32>>,
    crop_right: Rc<Cell<i32>>,
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
            crop_left: Rc::new(Cell::new(0)),
            crop_right: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn current_state(&self) -> state::DocumentState {
        state::DocumentState {
            zoom: self.zoom.get(),
            scroll_position: 0.0,
            start: self.loaded_from,
            crop_left: self.crop_left.get(),
            crop_right: self.crop_right.get(),
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
        self.crop_left.replace(state.crop_left);
        self.crop_right.replace(state.crop_right);

        for i in start..end {
            let page = self.new_page_widget(i);
            self.pages_box.append(&page);
        }

        self.resize_all(false);

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

    fn new_page_widget(&mut self, i: usize) -> gtk::DrawingArea {
        let page = self.doc.page(i as i32).unwrap();

        let drawing_area = DrawingArea::new();
        drawing_area.set_draw_func(
            clone!(@strong self.zoom as zoom, @strong self.crop_left as crop_left => move |_, cr, _width, _height| {
                let zoom = zoom.get();
                cr.translate(crop_left.get() as f64 * (-zoom), 0.0);
                cr.scale(zoom, zoom);
                page.render(cr);
            }),
        );

        self.resize_page(&drawing_area);

        drawing_area
    }

    pub(crate) fn apply_zoom(&mut self, zoom_factor: f64) {
        self.zoom.replace(self.zoom.get() * zoom_factor);
        self.resize_all(true);
    }

    pub(crate) fn adjust_crop(&mut self, left: i32, right: i32) {
        self.crop_right
            .replace(std::cmp::max(self.crop_right.get() + right, 0));
        self.crop_left
            .replace(std::cmp::max(self.crop_left.get() + left, 0));

        self.resize_all(true);
    }

    fn resize_all(&self, redraw: bool) {
        let mut child = self.pages_box.first_child();
        while let Some(page) = child {
            let da = page.clone().downcast::<gtk::DrawingArea>().unwrap();
            self.resize_page(&da);
            if redraw {
                da.queue_draw();
            }
            child = page.next_sibling();
        }
    }

    fn resize_page(&self, page: &gtk::DrawingArea) {
        let zoom = self.zoom.get();

        let new_width =
            ((self.width - self.crop_left.get() - self.crop_right.get()) as f64 * zoom) as i32;
        let new_height = (self.height as f64 * zoom) as i32;

        page.set_size_request(new_width, new_height);
    }
}
