use glib::subclass::InitializingObject;
use gtk::gdk::{EventSequence, Key, ModifierType};
use gtk::glib::subclass::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{
    glib, Button, CompositeTemplate, ListView, ScrolledWindow, SingleSelection, ToggleButton,
};
use gtk::{prelude::*, GestureClick};
use std::cell::RefCell;

use crate::state::State;

// Object holding the state
#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/andr2i/hallyview/app.ui")]
pub struct Window {
    #[template_child]
    pub state: TemplateChild<State>,
    #[template_child]
    pub model: TemplateChild<gtk::gio::ListStore>,
    #[template_child]
    pub selection: TemplateChild<SingleSelection>,

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

    drag_coords: RefCell<Option<(f64, f64)>>,
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
        klass.bind_template_instance_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

// Trait shared by all GObjects
impl ObjectImpl for Window {
    fn constructed(&self) {
        // Call "constructed" on parent
        self.parent_constructed();

        let obj = self.obj();
        obj.setup();
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
    fn handle_drag_start(&self, _n_press: i32, x: f64, y: f64) {
        *self.drag_coords.borrow_mut() = Some((x, y));
    }

    #[template_callback]
    fn handle_drag_move(&self, seq: Option<&EventSequence>, gc: &GestureClick) {
        if let Some((prev_x, _)) = *self.drag_coords.borrow() {
            if let Some((x, _)) = gc.point(seq) {
                self.obj().scroll_view(x - prev_x);
            }
        }
        *self.drag_coords.borrow_mut() = gc.point(seq);
    }

    #[template_callback]
    fn handle_key_press(
        &self,
        keyval: Key,
        _keycode: u32,
        _modifier: ModifierType,
    ) -> glib::Propagation {
        match keyval {
            Key::o => {
                self.obj().open_document();
            }
            Key::l => {
                self.obj().next_page();
            }
            Key::h => {
                self.obj().prev_page();
            }
            Key::bracketleft => {
                self.obj().zoom_out();
            }
            Key::bracketright => {
                self.obj().zoom_in();
            }
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    }
}

// Trait shared by all widgets
impl WidgetImpl for Window {}

// Trait shared by all windows
impl WindowImpl for Window {}

// Trait shared by all application windows
impl ApplicationWindowImpl for Window {}
