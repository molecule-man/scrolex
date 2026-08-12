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

    // The document the header bar and the menu act on.
    pub fn document(&self) -> DocumentView {
        self.imp().document.get()
    }

    pub fn apply_dark_mode(&self, enabled: bool) {
        if self.has_css_class("dark-mode") == enabled {
            return;
        }
        if enabled {
            self.add_css_class("dark-mode");
        } else {
            self.remove_css_class("dark-mode");
        }

        let document = self.document();
        document.state().invalidate_rendering();
        document.redraw_pages();
    }

    pub fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}
