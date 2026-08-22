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
        self.imp().viewport()
    }

    // Open a file in this tab. Saves the position of the document it replaces.
    pub fn load(&self, file: &gio::File) {
        self.imp().load(file);
    }

    pub fn save_position(&self) -> std::io::Result<()> {
        self.viewport().save_position()
    }

    pub fn is_loading(&self) -> bool {
        self.imp().loading_spinner.is_spinning()
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

    pub fn open_search(&self) {
        self.imp().open_search();
    }

    // Ctrl+F, F3, Shift+F3, and Escape, routed from the window.
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

    // The page a jump to `page_num` would land on, 1-based. None while no document is open.
    pub fn target_page(&self, page_num: u32) -> Option<u32> {
        self.imp().target_page(page_num)
    }

    // Repaint every laid-out page, e.g. after the render colours changed.
    pub fn redraw_pages(&self) {
        self.imp().redraw_pages();
    }

    // Release this document's share of the render pool. Call before dropping the view.
    pub fn release_renders(&self) {
        let client = self.document().render_client_id();
        crate::page::clear_all_renders(client);
        crate::page::set_wanted_pages(client, None);
    }

    pub fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(self.root().and_downcast::<gtk::Window>().as_ref());
    }
}
