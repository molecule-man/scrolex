use std::cell::RefCell;
use std::rc::Rc;

use glib::clone;
use glib::subclass::InitializingObject;
use gtk::gdk::{EventSequence, Key, ModifierType};
use gtk::glib::subclass::prelude::*;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::glib::{closure, closure_local};
use gtk::subclass::prelude::*;
use gtk::{
    glib, Button, CompositeTemplate, ListView, ScrolledWindow, SingleSelection, ToggleButton,
};
use gtk::{prelude::*, GestureClick};

use crate::page;
use crate::poppler::{Dest, DestExt};
use crate::render::Renderer;
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
    pub btn_jump_back: TemplateChild<Button>,
    #[template_child]
    pub scrolledwindow: TemplateChild<ScrolledWindow>,
    #[template_child]
    pub listview: TemplateChild<ListView>,
    #[template_child]
    pub entry_page_num: TemplateChild<gtk::Entry>,

    drag_coords: RefCell<Option<(f64, f64)>>,
    drag_cursor: RefCell<Option<gtk::gdk::Cursor>>,
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
        self.parent_constructed();

        let state: &State = self.state.as_ref();
        let factory = gtk::SignalListItemFactory::new();
        let renderer = Rc::new(RefCell::new(Renderer::new()));

        self.listview.set_factory(Some(&factory));
        let pn_expr = self
            .selection
            .property_expression("selected-item")
            .chain_property::<page::PageNumber>("page_number");

        pn_expr.bind(state, "page", gtk::Widget::NONE);

        let entry_page_num: &gtk::Entry = self.entry_page_num.as_ref();
        pn_expr
            .chain_closure::<String>(closure!(move |_: Option<glib::Object>, page_num: i32| {
                format!("{}", page_num + 1)
            }))
            .bind(entry_page_num, "text", gtk::Widget::NONE);

        let btn_jump_back: &gtk::Button = self.btn_jump_back.as_ref();
        let prev_page_expr = state.property_expression("prev_page");
        prev_page_expr
            .chain_closure::<String>(closure!(move |_: Option<glib::Object>, page_num: u32| {
                format!("Jump back to page {}", page_num)
            }))
            .bind(btn_jump_back, "tooltip-text", gtk::Widget::NONE);
        prev_page_expr
            .chain_closure::<bool>(closure!(move |_: Option<glib::Object>, page_num: u32| {
                page_num > 0
            }))
            .bind(btn_jump_back, "sensitive", gtk::Widget::NONE);

        state.connect_closure(
            "before-load",
            false,
            closure_local!(
                #[strong]
                renderer,
                move |_: &State| {
                    renderer.borrow().clear_cache();
                }
            ),
        );

        factory.connect_setup(clone!(
            #[weak]
            state,
            #[strong(rename_to = obj)]
            self.obj(),
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let page = &page::Page::new();

                state
                    .bind_property("crop", page, "crop")
                    .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                    .build();

                state
                    .bind_property("zoom", page, "zoom")
                    .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                    .build();

                state
                    .bind_property("uri", page, "uri")
                    .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                    .build();

                page.connect_closure(
                    "named-link-clicked",
                    false,
                    closure_local!(
                        #[strong]
                        obj,
                        #[weak]
                        state,
                        move |_: &crate::page::Page, dest_name: &str| {
                            if let Some(doc) = state.doc() {
                                let Some(dest) = doc.find_dest(dest_name) else {
                                    return;
                                };

                                let Dest::Xyz(page_num) = dest.to_dest() else {
                                    return;
                                };

                                obj.imp().goto_page(page_num as u32);
                            }
                        }
                    ),
                );

                list_item.set_child(Some(page));
            }
        ));

        factory.connect_bind(clone!(
            #[weak]
            state,
            #[strong]
            renderer,
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let page_number = list_item.item().and_downcast::<page::PageNumber>().unwrap();
                let page = list_item
                    .child()
                    .and_downcast::<crate::page::Page>()
                    .unwrap();

                if let Some(doc) = state.doc() {
                    if let Some(poppler_page) = doc.page(page_number.page_number()) {
                        page.bind(&page_number, &poppler_page, renderer.clone());
                    }
                }
            }
        ));

        if let Some(editable) = self.entry_page_num.delegate() {
            editable.connect_insert_text(|entry, s, _| {
                for c in s.chars() {
                    if !c.is_numeric() {
                        entry.stop_signal_emission_by_name("insert-text");
                    }
                }
            });
        }
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_scroll(&self, _dx: f64, dy: f64) -> glib::Propagation {
        if dy < 0.0 {
            self.prev_page();
        } else {
            self.next_page();
        }
        glib::Propagation::Stop
    }

    #[template_callback]
    fn handle_drag_start(&self, _n_press: i32, x: f64, y: f64) {
        *self.drag_coords.borrow_mut() = Some((x, y));

        if let Some(surface) = self.obj().surface() {
            *self.drag_cursor.borrow_mut() = surface.cursor();
            surface.set_cursor(gtk::gdk::Cursor::from_name("grabbing", None).as_ref());
        }
    }

    #[template_callback]
    fn handle_drag_move(&self, seq: Option<&EventSequence>, gc: &GestureClick) {
        if let Some((prev_x, _)) = *self.drag_coords.borrow() {
            if let Some((x, _)) = gc.point(seq) {
                let dx = x - prev_x;
                let hadjustment = self.scrolledwindow.hadjustment();
                hadjustment.set_value(hadjustment.value() - dx);
            }
        }
        *self.drag_coords.borrow_mut() = gc.point(seq);
    }

    #[template_callback]
    fn handle_drag_end(&self) {
        if let Some(surface) = self.obj().surface() {
            surface.set_cursor(self.drag_cursor.borrow().as_ref());
        }
    }

    #[template_callback]
    fn zoom_out(&self) {
        self.state.set_zoom(self.state.zoom() / 1.1);
    }

    #[template_callback]
    fn zoom_in(&self) {
        self.state.set_zoom(self.state.zoom() * 1.1);
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
                self.open_document();
            }
            Key::l => {
                self.next_page();
            }
            Key::h => {
                self.prev_page();
            }
            Key::bracketleft => {
                self.zoom_out();
            }
            Key::bracketright => {
                self.zoom_in();
            }
            _ => return glib::Propagation::Proceed,
        }

        glib::Propagation::Stop
    }

    #[template_callback]
    fn handle_page_number_entered(&self, entry: &gtk::Entry) {
        let Ok(page_num) = entry.text().parse::<u32>() else {
            return;
        };

        self.goto_page(page_num);
    }

    #[template_callback]
    fn handle_page_number_icon_pressed(&self, _: gtk::EntryIconPosition, entry: &gtk::Entry) {
        let Ok(page_num) = entry.text().parse::<u32>() else {
            return;
        };

        self.goto_page(page_num);
    }

    fn goto_page(&self, page_num: u32) {
        self.state.jump_list_add(self.state.page() + 1);
        self.navigate_to_page(page_num);
    }

    // same as goto_page, but doesn't add to jump list
    fn navigate_to_page(&self, page_num: u32) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        let page_num = page_num.min(selection.n_items());

        self.listview.scroll_to(
            page_num.saturating_sub(1),
            gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
            None,
        );
    }

    fn prev_page(&self) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        let current_pos = self.scrolledwindow.hadjustment().value();

        // normally I'd use list_view.scroll_to() here, but it doesn't scroll if the item
        // is already visible :(
        selection.select_item(selection.selected().saturating_sub(1), true);
        let width = selection
            .selected_item()
            .and_downcast::<page::PageNumber>()
            .unwrap()
            .width() as f64
            + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this

        self.scrolledwindow
            .hadjustment()
            .set_value(current_pos - width);
    }

    fn next_page(&self) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        let current_pos = self.scrolledwindow.hadjustment().value();

        // normally I'd use list_view.scroll_to() here, but it doesn't scroll if the item
        // is already visible :(
        let width = selection
            .selected_item()
            .and_downcast::<page::PageNumber>()
            .unwrap()
            .width() as f64
            + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this

        selection.select_item(
            (selection.selected() + 1).min(selection.n_items() - 1),
            true,
        );
        self.scrolledwindow
            .hadjustment()
            .set_value(current_pos + width);
    }

    fn ensure_ready_selection(&self) -> Option<&gtk::SingleSelection> {
        let selection: &gtk::SingleSelection = self.selection.as_ref();

        if selection.n_items() == 0 {
            return None;
        }

        selection.selected_item()?;

        Some(selection)
    }

    #[template_callback]
    fn clear_model(&self) {
        self.model.remove_all();
    }

    #[template_callback]
    fn open_document(&self) {
        let filter = gtk::FileFilter::new();
        filter.add_mime_type("application/pdf");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);

        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .filters(&filters)
            .build();

        let obj = self.obj();
        dialog.open(
            Some(obj.as_ref()),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[strong(rename_to = state)]
                self.state,
                #[strong]
                obj,
                move |file| match file {
                    Ok(file) => {
                        state.load(&file).unwrap_or_else(|err| {
                            obj.show_error_dialog(&format!("Error loading file: {}", err));
                        });
                    }
                    Err(err) => {
                        obj.show_error_dialog(&format!("Error opening file: {}", err));
                    }
                },
            ),
        );
    }

    #[template_callback]
    fn handle_document_load(&self, state: &State) {
        let Some(doc) = state.doc() else {
            return;
        };

        let model = self.model.clone();
        let selection = self.selection.clone();

        let n_pages = doc.n_pages() as u32;
        let scroll_to = state.page().min(n_pages - 1);
        let init_load_from = scroll_to.saturating_sub(1);
        let init_load_till = (scroll_to + 10).min(n_pages - 1);

        let vector: Vec<page::PageNumber> = (init_load_from as i32..init_load_till as i32)
            .map(page::PageNumber::new)
            .collect();
        model.extend_from_slice(&vector);
        selection.select_item(scroll_to - init_load_from, true);

        glib::idle_add_local(move || {
            if init_load_from > 0 {
                let vector: Vec<page::PageNumber> = (0..init_load_from as i32)
                    .map(page::PageNumber::new)
                    .collect();
                model.splice(0, 0, &vector);
            }
            if init_load_till < n_pages {
                let vector: Vec<page::PageNumber> = (init_load_till as i32..n_pages as i32)
                    .map(page::PageNumber::new)
                    .collect();
                model.extend_from_slice(&vector);
            }
            glib::ControlFlow::Break
        });
    }

    #[template_callback]
    fn jump_back(&self) {
        if let Some(page) = self.state.jump_list_pop() {
            self.navigate_to_page(page);
        }
    }
}

// Trait shared by all widgets
impl WidgetImpl for Window {}

// Trait shared by all windows
impl WindowImpl for Window {}

// Trait shared by all application windows
impl ApplicationWindowImpl for Window {}
