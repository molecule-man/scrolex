mod imp;

use std::cell::RefCell;
use std::rc::Rc;

use glib::{clone, Object};
use gtk::glib::closure_local;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::*;
use gtk::{gio, glib, Application};

use crate::page;
use crate::page_overlay::PageOverlay;
use crate::render::Renderer;
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

    pub(crate) fn setup(&self) {
        let state: &State = self.imp().state.as_ref();
        let factory = gtk::SignalListItemFactory::new();
        let renderer = Rc::new(RefCell::new(Renderer::new()));

        self.imp().listview.set_factory(Some(&factory));
        self.imp()
            .selection
            .property_expression("selected-item")
            .chain_property::<page::PageNumber>("page_number")
            .bind(state, "page", gtk::Widget::NONE);

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
            #[weak(rename_to = listview)]
            self.imp().listview,
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let overlay = crate::page_overlay::PageOverlay::new();
                let page: &crate::page::Page = overlay.imp().page.as_ref();

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

                overlay.connect_closure(
                    "page-link-clicked",
                    false,
                    closure_local!(move |_: &PageOverlay, page_num: i32| {
                        listview.scroll_to(
                            (page_num as u32).saturating_sub(1),
                            gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                            None,
                        );
                    }),
                );

                list_item.set_child(Some(&overlay));
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
                let overlay = list_item
                    .child()
                    .and_downcast::<crate::page_overlay::PageOverlay>()
                    .unwrap();
                let page: &crate::page::Page = overlay.imp().page.as_ref();

                if let Some(doc) = state.doc() {
                    if let Some(poppler_page) = doc.page(page_number.page_number()) {
                        page.bind(&page_number, &poppler_page, renderer.clone());
                        overlay.bind(&poppler_page, &doc);
                    }
                }
            }
        ));
    }

    pub(crate) fn scroll_view(&self, dx: f64) {
        let hadjustment = self.imp().scrolledwindow.hadjustment();
        hadjustment.set_value(hadjustment.value() - dx);
    }

    #[template_callback]
    pub(crate) fn zoom_out(&self) {
        self.imp().state.set_zoom(self.imp().state.zoom() / 1.1);
    }

    #[template_callback]
    pub(crate) fn zoom_in(&self) {
        self.imp().state.set_zoom(self.imp().state.zoom() * 1.1);
    }

    pub(crate) fn prev_page(&self) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        let current_pos = self.imp().scrolledwindow.hadjustment().value();

        // normally I'd use list_view.scroll_to() here, but it doesn't scroll if the item
        // is already visible :(
        selection.select_item(selection.selected().saturating_sub(1), true);
        let width = selection
            .selected_item()
            .and_downcast::<page::PageNumber>()
            .unwrap()
            .width() as f64
            + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this

        self.imp()
            .scrolledwindow
            .hadjustment()
            .set_value(current_pos - width);
    }

    pub(crate) fn next_page(&self) {
        let Some(selection) = self.ensure_ready_selection() else {
            return;
        };

        let current_pos = self.imp().scrolledwindow.hadjustment().value();

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
        self.imp()
            .scrolledwindow
            .hadjustment()
            .set_value(current_pos + width);
    }

    fn ensure_ready_selection(&self) -> Option<&gtk::SingleSelection> {
        let selection: &gtk::SingleSelection = self.imp().selection.as_ref();

        if selection.n_items() == 0 {
            return None;
        }

        selection.selected_item()?;

        Some(selection)
    }

    #[template_callback]
    pub fn open_document(&self) {
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .build();

        dialog.open(
            Some(self),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[strong(rename_to = state)]
                self.imp().state,
                #[strong(rename_to = win)]
                self,
                move |file| match file {
                    Ok(file) => {
                        state.load(&file).unwrap_or_else(|err| {
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
    pub fn clear_model(&self) {
        self.imp().model.remove_all();
    }

    #[template_callback]
    pub fn handle_document_load(&self, state: &State) {
        let Some(doc) = state.doc() else {
            return;
        };

        let model = self.imp().model.clone();
        let selection = self.imp().selection.clone();

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

    pub(crate) fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}
