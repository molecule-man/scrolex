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

    pub(crate) fn state(&self) -> &State {
        self.imp().state.as_ref()
    }

    pub(crate) fn goto_page(&self, page_number: u32) {
        self.imp().goto_page(page_number);
    }

    pub(crate) fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}

// not used. Only needed for other objects to be able to use window as property
impl Default for Window {
    fn default() -> Self {
        Object::builder().build()
    }
}
