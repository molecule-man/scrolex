mod imp;

use glib::{clone, Object};
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::prelude::*;
use gtk::{gio, glib, Application};

use crate::page;
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
        self.setup_model();
        self.setup_factory();
    }

    pub(crate) fn scroll_view(&self, dx: f64) {
        let hadjustment = self.imp().scrolledwindow.hadjustment();
        hadjustment.set_value(hadjustment.value() - dx);
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

    fn setup_model(&self) {
        let state: &State = self.imp().state.as_ref();
        self.imp()
            .selection
            .property_expression("selected-item")
            .chain_property::<page::PageNumber>("page_number")
            .bind(state, "page", gtk::Widget::NONE);
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

    fn setup_factory(&self) {
        let state: &State = self.imp().state.as_ref();
        let factory = gtk::SignalListItemFactory::new();
        self.imp().listview.set_factory(Some(&factory));

        factory.connect_setup(clone!(
            #[weak(rename_to = state)]
            state,
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let page = page::Page::new();

                state
                    .bind_property("crop", &page, "crop")
                    .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                    .build();

                state
                    .bind_property("zoom", &page, "zoom")
                    .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                    .build();

                list_item.set_child(Some(&page));
            }
        ));

        factory.connect_bind(clone!(
            #[weak(rename_to = state)]
            state,
            move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let page_number = list_item.item().and_downcast::<page::PageNumber>().unwrap();
                let child = list_item.child().unwrap();
                let page = child.downcast_ref::<page::Page>().unwrap();

                if let Some(doc) = state.doc() {
                    if let Some(poppler_page) = doc.page(page_number.page_number()) {
                        page.bind(&page_number, &poppler_page);
                    }
                }
            }
        ));
    }

    pub(crate) fn show_error_dialog(&self, message: &str) {
        gtk::AlertDialog::builder()
            .message(message)
            .build()
            .show(Some(self));
    }
}
