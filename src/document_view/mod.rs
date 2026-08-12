mod imp;

use glib::Object;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::WidgetExt;
use gtk::{gio, glib, Application};

use crate::state::State;

glib::wrapper! {
    pub struct DocumentView(ObjectSubclass<imp::DocumentView>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

#[gtk::template_callbacks]
impl DocumentView {
    pub fn new(app: &Application) -> Self {
        Object::builder().property("application", app).build()
    }

    pub fn state(&self) -> &State {
        self.imp().state.as_ref()
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
        self.state().invalidate_rendering();
        self.imp().redraw_pages();
    }

    pub fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}
