use glib::subclass::InitializingObject;
use gtk::gio::prelude::*;
use gtk::glib::clone;
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{
    glib, Button, CompositeTemplate, EventControllerScroll, ListView, ScrolledWindow, ToggleButton,
};
use std::cell::RefCell;

// Object holding the state
#[derive(CompositeTemplate, Default, glib::Properties)]
#[template(resource = "/com/andr2i/hallyview/app.ui")]
#[properties(wrapper_type = super::Window)]
pub struct Window {
    #[template_child]
    pub btn_open: TemplateChild<Button>,
    #[template_child]
    pub btn_zoom_in: TemplateChild<Button>,
    #[template_child]
    pub btn_zoom_out: TemplateChild<Button>,
    #[template_child]
    pub btn_crop: TemplateChild<ToggleButton>,
    #[template_child]
    pub scrolledwindow: TemplateChild<ScrolledWindow>,
    #[template_child]
    pub listview: TemplateChild<ListView>,

    #[property(get, set)]
    state: RefCell<crate::state::State>,
}

// The central trait for subclassing a GObject
#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "MyApp";
    type Type = super::Window;
    type ParentType = gtk::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

// Trait shared by all GObjects
#[glib::derived_properties]
impl ObjectImpl for Window {
    fn constructed(&self) {
        // Call "constructed" on parent
        self.parent_constructed();

        //let obj = self.obj();
        //obj.setup();
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_scroll(&self, _dx: f64, dy: f64) -> glib::Propagation {
        if dy < 0.0 {
            self.obj().prev_page();
        } else {
            self.obj().next_page();
        }
        glib::Propagation::Stop
    }

    #[template_callback]
    pub fn handle_document_open(&self, _: &Button) {
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .build();

        dialog.open(
            Some(&self.obj().application().unwrap().active_window().unwrap()),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[strong(rename_to = state)]
                self.state,
                #[strong(rename_to = win)]
                self.obj(),
                move |file| match file {
                    Ok(file) => {
                        state.borrow().load(&file).unwrap_or_else(|err| {
                            win.show_error_dialog(&format!("Error loading file: {}", err));
                        });
                    }
                    Err(err) => {
                        win.show_error_dialog(&format!("Error opening file: {}", err));
                    }
                },
            ),
        );
    }

    #[template_callback]
    pub fn handle_zoom_out(&self, _: &Button) {
        let zoom = self.state.borrow().zoom();
        self.state.borrow().set_zoom(zoom / 1.1);
    }

    #[template_callback]
    pub fn handle_zoom_in(&self, _: &Button) {
        let zoom = self.state.borrow().zoom();
        self.state.borrow().set_zoom(zoom * 1.1);
    }
}

// Trait shared by all widgets
impl WidgetImpl for Window {}

// Trait shared by all windows
impl WindowImpl for Window {}

// Trait shared by all application windows
impl ApplicationWindowImpl for Window {}
