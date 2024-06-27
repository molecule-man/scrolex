use gtk::prelude::*;
use gtk::DrawingArea;

pub(crate) struct ZoomHandler {
    zoom: f64,
    pages_box: gtk::Box,
    width: i32,
    height: i32,
}

impl ZoomHandler {
    pub(crate) fn new(pages_box: gtk::Box) -> Self {
        ZoomHandler {
            zoom: 1.0,
            pages_box,
            width: 800,
            height: 800,
        }
    }

    pub(crate) fn zoom(&self) -> f64 {
        self.zoom
    }

    pub(crate) fn apply_zoom(&mut self, zoom_factor: f64) {
        self.zoom *= zoom_factor;

        let mut child = self.pages_box.first_child();
        while let Some(c) = child {
            if let Some(page) = c.downcast_ref::<DrawingArea>() {
                page.set_size_request(
                    (self.width as f64 * self.zoom) as i32,
                    (self.height as f64 * self.zoom) as i32,
                );
                page.queue_draw();
            }
            child = c.next_sibling();
        }
    }

    pub(crate) fn reset(&mut self, width: i32, height: i32) {
        self.zoom = 1.0;
        self.width = width;
        self.height = height;
    }
}
