use gtk::prelude::*;
use gtk::DrawingArea;

pub(crate) struct ZoomHandler {
    zoom: f64,
    pages_box: gtk::Box,
}

impl ZoomHandler {
    pub(crate) fn new(pages_box: gtk::Box) -> Self {
        ZoomHandler {
            zoom: 1.0,
            pages_box,
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
                let width = page.width();
                let height = page.height();
                page.set_size_request(
                    (width as f64 * zoom_factor) as i32,
                    (height as f64 * zoom_factor) as i32,
                );
                page.queue_draw();
            }
            child = c.next_sibling();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.zoom = 1.0;
    }
}
