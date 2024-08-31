mod imp;

use glib::{clone, Object};
use gtk::gdk::BUTTON_MIDDLE;
use gtk::glib::closure_local;
use gtk::glib::subclass::types::ObjectSubclassIsExt;
use gtk::{gio, glib, Application};
use gtk::{prelude::*, EventControllerScrollFlags};
use std::cell::RefCell;
use std::rc::Rc;

use crate::page;
use crate::state;

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub fn new(app: &Application, state: &state::State) -> Self {
        let w: Self = Object::builder()
            .property("application", app)
            .property("state", state)
            .build();

        let (model, selection) = w.setup_model(state);
        w.setup_factory(state);
        w.setup_scroll(&model, &selection);
        w.setup_drag();

        w
    }

    fn setup_drag(&self) {
        let scroll_win = self.imp().scrolledwindow.clone();

        // Middle click drag to scroll
        let middle_click_drag = gtk::GestureClick::builder().button(BUTTON_MIDDLE).build();
        let previous_coords = Rc::new(RefCell::new(None::<(f64, f64)>));
        middle_click_drag.connect_pressed(clone!(
            #[strong]
            previous_coords,
            move |_, _, x, y| {
                *previous_coords.borrow_mut() = Some((x, y));
            }
        ));
        middle_click_drag.connect_update(clone!(
            #[strong]
            previous_coords,
            #[weak]
            scroll_win,
            move |ch, seq| {
                if let Some((prev_x, _)) = *previous_coords.borrow() {
                    if let Some((x, _)) = ch.point(seq) {
                        let dx = x - prev_x;
                        scroll_win
                            .hadjustment()
                            .set_value(scroll_win.hadjustment().value() - dx);
                    }
                }
                *previous_coords.borrow_mut() = ch.point(seq);
            }
        ));
        scroll_win.add_controller(middle_click_drag);
    }

    fn setup_scroll(&self, model: &gtk::gio::ListStore, selection: &gtk::SingleSelection) {
        let scroll_controller = gtk::EventControllerScroll::new(
            EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
        );
        scroll_controller.connect_scroll(clone!(
            #[weak]
            selection,
            #[weak]
            model,
            #[weak(rename_to = window)]
            self.imp().scrolledwindow,
            #[upgrade_or]
            glib::Propagation::Stop,
            move |_, _dx, dy| {
                let current_pos = window.hadjustment().value();

                // normally I'd use list_view.scroll_to() here, but it doesn't scroll if the item
                // is already visible :(
                if dy < 0.0 {
                    // scroll left
                    selection.select_item(selection.selected().saturating_sub(1), true);
                    let width = selection
                        .selected_item()
                        .unwrap()
                        .downcast::<page::PageNumber>()
                        .unwrap()
                        .width() as f64
                        + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this
                    window.hadjustment().set_value(current_pos - width);
                } else {
                    let width = selection
                        .selected_item()
                        .unwrap()
                        .downcast::<page::PageNumber>()
                        .unwrap()
                        .width() as f64
                        + 4.0; // 4px is padding of list item widget. TODO: figure out how to un-hardcode this

                    // scroll right
                    selection
                        .select_item((selection.selected() + 1).min(model.n_items() - 1), true);
                    window.hadjustment().set_value(current_pos + width);
                }

                glib::Propagation::Stop
            }
        ));
        self.imp().listview.add_controller(scroll_controller);
    }

    fn setup_model(&self, state: &state::State) -> (gtk::gio::ListStore, gtk::SingleSelection) {
        let model = gtk::gio::ListStore::new::<page::PageNumber>();
        let selection = gtk::SingleSelection::new(Some(model.clone()));
        self.imp().listview.set_model(Some(&selection));

        state.connect_closure(
            "before-load",
            false,
            closure_local!(
                #[weak]
                model,
                move |_: &state::State| {
                    model.remove_all();
                }
            ),
        );

        state.connect_closure(
            "loaded",
            false,
            closure_local!(
                #[weak]
                model,
                #[weak]
                selection,
                move |state: &state::State| {
                    let doc = if let Some(doc) = state.doc() {
                        doc
                    } else {
                        return;
                    };

                    let n_pages = doc.n_pages() as u32;
                    let scroll_to = state.page().min(n_pages - 1);
                    let init_load_from = scroll_to.saturating_sub(1);
                    let init_load_till = (scroll_to + 10).min(n_pages - 1);

                    let vector: Vec<page::PageNumber> = (init_load_from as i32
                        ..init_load_till as i32)
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
                            let vector: Vec<page::PageNumber> = (init_load_till as i32
                                ..n_pages as i32)
                                .map(page::PageNumber::new)
                                .collect();
                            model.extend_from_slice(&vector);
                        }
                        glib::ControlFlow::Break
                    });
                }
            ),
        );

        selection
            .property_expression("selected-item")
            .chain_property::<page::PageNumber>("page_number")
            .bind(state, "page", gtk::Widget::NONE);

        self.imp()
            .btn_crop
            .bind_property("active", state, "crop")
            .bidirectional()
            .build();

        (model, selection)
    }

    fn setup_factory(&self, state: &state::State) {
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
