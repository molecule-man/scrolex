mod imp;

use glib::Object;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::document::Document;
use crate::links::DocumentLocation;
use crate::viewport::Viewport;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReaderKeyContext {
    Document,
    NumericEntry,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ZoomChoice {
    pub label: String,
    action: ZoomChoiceAction,
}

#[derive(Clone, Debug, PartialEq)]
enum ZoomChoiceAction {
    Scale(f64),
    FitHeight(f64),
    FitPages { first: i32, count: usize, zoom: f64 },
    FitVisible,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct HorizontalChrome {
    pub pane: f64,
    pub row: f64,
}

impl HorizontalChrome {
    pub(crate) fn total(self) -> f64 {
        self.pane + self.row
    }
}

glib::wrapper! {
    pub struct DocumentPane(ObjectSubclass<imp::DocumentPane>)
        @extends gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget;
}

#[gtk::template_callbacks]
impl DocumentPane {
    pub fn new(document: &Document) -> Self {
        Object::builder().property("document", document).build()
    }

    pub fn document(&self) -> &Document {
        self.imp().document()
    }

    pub fn viewport(&self) -> &Viewport {
        self.imp().viewport()
    }

    pub(crate) fn selection(&self) -> gtk::SingleSelection {
        self.imp().selection.clone()
    }

    pub(crate) fn prepare_load(&self) {
        self.imp().cancel_scroll_motion();
    }

    pub(crate) fn clear_document(&self) {
        self.imp().clear_model();
        self.viewport().reset();
    }

    pub(crate) fn finish_document_load(&self) {
        self.imp().handle_document_load();
    }

    pub(crate) fn focus_reader(&self) {
        self.imp().scrolledwindow.grab_focus();
    }

    pub(crate) fn page_area(&self) -> gtk::ScrolledWindow {
        self.imp().scrolledwindow.clone()
    }

    pub(crate) fn close_button(&self) -> gtk::Button {
        self.imp().close_button.clone()
    }

    pub(crate) fn set_close_visible(&self, visible: bool) {
        self.imp().close_button.set_visible(visible);
    }

    pub(crate) fn release_renders(&self) {
        let viewport = self.viewport().id();
        crate::page::set_wanted_pages(self.document().id(), viewport, None);
        crate::page::clear_full_renders(viewport);
        self.clear_render_pins();
    }

    pub(crate) fn clear_render_pins(&self) {
        self.document()
            .render_cache()
            .borrow_mut()
            .clear_pins(self.viewport().id());
    }

    pub(crate) fn paper_width(&self, page: i32) -> Option<f64> {
        self.document()
            .cached_bbox(page, self.viewport().crop())
            .map(|bbox| bbox.size().0)
    }

    pub(crate) fn apply_split_zoom(&self, zoom: f64) {
        self.viewport().set_fit_height(false);
        self.viewport().fit_zoom_to(zoom);
    }

    pub(crate) fn vertical_position(&self) -> f64 {
        self.imp().vscrolledwindow.vadjustment().value()
    }

    pub(crate) fn restore_vertical_position(&self, value: f64) {
        self.imp().vscrolledwindow.vadjustment().set_value(value);
    }

    pub(crate) fn reveal_page_horizontally(&self, page: i32) {
        self.imp().reveal_page_horizontally(page);
    }

    pub(crate) fn horizontal_chrome(&self, page: i32) -> Option<HorizontalChrome> {
        let page = self.imp().mapped_page(page)?;
        let row = page.parent()?;
        let row_width = row.measure(gtk::Orientation::Horizontal, -1).1;
        let page_width = page.measure(gtk::Orientation::Horizontal, -1).1;
        Some(HorizontalChrome {
            pane: (f64::from(self.width()) - self.viewport_width()).max(0.0),
            row: (f64::from(row_width) - f64::from(page_width)).max(0.0),
        })
    }

    pub(crate) fn viewport_width(&self) -> f64 {
        self.imp().scrolledwindow.hadjustment().page_size()
    }

    pub(crate) fn redraw_page(&self, index: i32) {
        self.imp().redraw_page(index);
    }

    pub(crate) fn reveal_current(&self) {
        self.imp().reveal_current();
    }

    // Actions the header bar and the menu drive.

    pub fn zoom_in(&self) {
        self.imp().zoom_in();
    }

    pub fn zoom_out(&self) {
        self.imp().zoom_out();
    }

    pub fn fit_width(&self) {
        self.imp().fit_width();
    }

    pub fn reset_zoom(&self) {
        self.imp().reset_zoom();
    }

    pub(crate) fn zoom_choices(&self) -> Vec<ZoomChoice> {
        self.imp().zoom_choices()
    }

    pub(crate) fn apply_zoom_choice(&self, choice: &ZoomChoice) {
        self.imp().apply_zoom_choice(choice);
    }

    pub fn jump_back(&self) {
        self.imp().jump_back();
    }

    pub fn jump_forward(&self) {
        self.imp().jump_forward();
    }

    pub fn goto_page(&self, page_num: u32) {
        self.imp().goto_page(page_num);
    }

    pub fn navigate_to_location(&self, location: DocumentLocation) {
        self.imp().navigate_to_location(location);
    }

    pub(crate) fn follow_link(&self, source_page: i32, location: DocumentLocation) {
        self.imp().follow_link(source_page, location);
    }

    pub fn apply_zoom_percent(&self, percent: f64) {
        self.imp().apply_zoom_percent(percent);
    }

    pub(crate) fn handle_reader_key(
        &self,
        keyval: gtk::gdk::Key,
        modifier: gtk::gdk::ModifierType,
        context: ReaderKeyContext,
    ) -> glib::Propagation {
        self.imp().handle_reader_key(keyval, modifier, context)
    }

    // The page a jump to `page_num` would land on, 1-based. None while no document is open.
    pub fn target_page(&self, page_num: u32) -> Option<u32> {
        self.imp().target_page(page_num)
    }

    // Repaint every laid-out page, e.g. after the render colours changed.
    pub fn redraw_pages(&self) {
        self.imp().redraw_pages();
    }
}

pub(crate) use imp::fit_width_zoom;
