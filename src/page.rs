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
    crop_left: usize,
    crop_right: usize,
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
            crop_left: 0,
            crop_right: 0,
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
        fixed.set_overflow(gtk::Overflow::Hidden);
        fixed.put(&drawing_area, 0.0, 0.0);

        self.set_page_wrapper_sizes(&fixed);

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

    pub(crate) fn adjust_crop(&mut self, left: i32, right: i32) {
        if self.crop_left as i32 + left >= 0 {
            self.crop_left = (self.crop_left as i32 + left) as usize;
        }

        if self.crop_right as i32 + right >= 0 {
            self.crop_right = (self.crop_right as i32 + right) as usize;
        }

        self.redraw();
    }

    fn redraw(&self) {
        let mut child = self.pages_box.first_child();
        while let Some(c) = child {
            if let Some(container) = c.downcast_ref::<gtk::Fixed>() {
                self.set_page_wrapper_sizes(container);

                let drawing_area = container.first_child().unwrap();

                if let Some(page) = drawing_area.downcast_ref::<DrawingArea>() {
                    self.set_page_sizes(page);
                    page.queue_draw();
                }

                container.queue_draw();
            }

            child = c.next_sibling();
        }
    }

    fn set_page_wrapper_sizes(&self, wrapper: &gtk::Fixed) {
        let zoom = self.zoom.get();

        let new_wrapper_width = self.width - self.crop_left as i32 - self.crop_right as i32;
        dbg!(self.width);
        dbg!(self.crop_right);
        dbg!(wrapper.width());
        dbg!(new_wrapper_width);
        dbg!(new_wrapper_width as f64 * zoom);
        dbg!(((self.width - self.crop_left as i32 - self.crop_right as i32) as f64 * zoom) as i32);

        if self.crop_left > 0 {
            wrapper.set_size_request(300, (self.height as f64 * zoom) as i32);
        } else {
            wrapper.set_size_request(
                ((self.width - self.crop_left as i32 - self.crop_right as i32) as f64 * zoom)
                    as i32,
                (self.height as f64 * zoom) as i32,
            );
        }
        wrapper.move_(
            &wrapper.first_child().unwrap(),
            (self.crop_left as f64) * (-zoom),
            0.0,
        );
    }

    fn set_page_sizes(&self, page: &gtk::DrawingArea) {
        let zoom = self.zoom.get();

        page.set_size_request(
            (self.width as f64 * zoom) as i32,
            (self.height as f64 * zoom) as i32,
        );
    }
}
