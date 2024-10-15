pub mod bg_job;
pub mod jump_stack;
pub mod links;
pub mod page;
pub mod poppler;
pub mod state;
pub mod window;

extern crate poppler as popp;
use gtk::cairo::ffi::cairo_t;
use gtk::glib::translate::*;
use popp::ffi;

//pub use crate::links::Links;
//
//pub use window::Window;

extern "C" {
    fn render_page(page: *mut ffi::PopplerPage, cairo: *mut cairo_t);
    fn render_doc_page(uri: *const i8, page_num: i32, cairo: *mut cairo_t);
    //fn render_page(
    //    page: *mut poppler::Page,
    //    surface: *mut gtk::cairo::ImageSurface,
    //    width: f64,
    //    height: f64,
    //);
}

pub fn cpp_render_page(page: &popp::Page, cr: &gtk::cairo::Context) {
    unsafe {
        render_page(page.to_glib_none().0, mut_override(cr.to_glib_none().0));
    }
}

pub fn cpp_render_doc_page(uri: &str, page_num: i32, cr: &gtk::cairo::Context) {
    unsafe {
        render_doc_page(
            uri.to_glib_none().0,
            page_num,
            mut_override(cr.to_glib_none().0),
        );
    }
}
