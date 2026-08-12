pub mod imp;

use glib::Object;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::WidgetExt;
use gtk::{gio, glib, Application};

use crate::document_view::DocumentView;

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

#[gtk::template_callbacks]
impl Window {
    pub fn new(app: &Application) -> Self {
        Object::builder().property("application", app).build()
    }

    pub fn active_document(&self) -> DocumentView {
        self.imp().active_document()
    }

    pub fn documents(&self) -> Vec<DocumentView> {
        self.imp().documents()
    }

    pub fn apply_dark_mode(&self, enabled: bool) {
        if self.has_css_class("dark-mode") != enabled {
            if enabled {
                self.add_css_class("dark-mode");
            } else {
                self.remove_css_class("dark-mode");
            }
        }

        for document in self.documents() {
            document.state().invalidate_rendering();
            document.redraw_pages();
        }
    }

    pub fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}
