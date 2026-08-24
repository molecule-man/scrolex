mod imp;

use glib::Object;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::document::Document;
use crate::document_pane::DocumentPane;
use crate::viewport::Viewport;

pub(crate) use crate::document_pane::{ReaderKeyContext, ZoomChoice};

glib::wrapper! {
    pub struct DocumentView(ObjectSubclass<imp::DocumentView>)
        @extends gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget;
}

impl Default for DocumentView {
    fn default() -> Self {
        Self::new()
    }
}

#[gtk::template_callbacks]
impl DocumentView {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn document(&self) -> &Document {
        self.imp().document()
    }

    pub fn viewport(&self) -> &Viewport {
        self.pane().viewport()
    }

    pub fn selection(&self) -> gtk::SingleSelection {
        self.pane().selection()
    }

    pub fn load(&self, file: &gio::File) {
        self.imp().load(file);
    }

    pub fn save_position(&self) -> std::io::Result<()> {
        self.viewport().save_position()
    }

    pub fn is_loading(&self) -> bool {
        self.imp().loading_spinner.is_spinning()
    }

    pub fn zoom_in(&self) {
        self.pane().zoom_in();
    }

    pub fn zoom_out(&self) {
        self.pane().zoom_out();
    }

    pub fn fit_width(&self) {
        self.pane().fit_width();
    }

    pub fn reset_zoom(&self) {
        self.pane().reset_zoom();
    }

    pub(crate) fn zoom_choices(&self) -> Vec<ZoomChoice> {
        self.pane().zoom_choices()
    }

    pub(crate) fn apply_zoom_choice(&self, choice: &ZoomChoice) {
        self.pane().apply_zoom_choice(choice);
    }

    pub fn jump_back(&self) {
        self.pane().jump_back();
    }

    pub fn jump_forward(&self) {
        self.pane().jump_forward();
    }

    pub fn goto_page(&self, page_num: u32) {
        self.pane().goto_page(page_num);
    }

    pub fn apply_zoom_percent(&self, percent: f64) {
        self.pane().apply_zoom_percent(percent);
    }

    pub fn open_search(&self) {
        self.imp().open_search();
    }

    pub fn handle_search_key(
        &self,
        keyval: gtk::gdk::Key,
        modifier: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        self.imp().handle_search_key(keyval, modifier)
    }

    pub(crate) fn handle_reader_key(
        &self,
        keyval: gtk::gdk::Key,
        modifier: gtk::gdk::ModifierType,
        context: ReaderKeyContext,
    ) -> glib::Propagation {
        self.imp().handle_reader_key(keyval, modifier, context)
    }

    pub fn target_page(&self, page_num: u32) -> Option<u32> {
        self.pane().target_page(page_num)
    }

    pub fn redraw_pages(&self) {
        self.pane().redraw_pages();
    }

    pub fn release_renders(&self) {
        crate::page::clear_document_renders(self.document().id());
        self.document().clear_render_jobs();
    }

    pub fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(self.root().and_downcast::<gtk::Window>().as_ref());
    }

    pub(crate) fn pane(&self) -> &DocumentPane {
        self.imp().pane()
    }
}
