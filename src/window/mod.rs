mod imp;

use glib::Object;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::{gio, glib, Application};

use crate::state::State;

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

    pub fn state(&self) -> &State {
        self.imp().state.as_ref()
    }

    pub fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}
