mod imp;

use glib::Object;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::document::Document;
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
