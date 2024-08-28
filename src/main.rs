use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::gdk::BUTTON_MIDDLE;
use gtk::glib::{closure_local, Uri};
use gtk::{gio::ApplicationFlags, glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags, ScrolledWindow};

mod page;
mod state;

const APP_ID: &str = "com.andr2i.hallyview";

fn main() -> glib::ExitCode {
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(ApplicationFlags::HANDLES_OPEN | ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.connect_command_line(|app, cmd| {
        build_ui(app, cmd.arguments());
        0
    });
    app.run_with_args(&std::env::args().collect::<Vec<_>>())
}

fn build_ui(app: &Application, args: Vec<OsString>) {
    let header_bar = gtk::HeaderBar::builder().build();
    let open_button = Button::from_icon_name("document-open");

    header_bar.pack_start(&open_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .build();

    window.set_titlebar(Some(&header_bar));

    let scroll_win = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

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

    window.set_child(Some(&scroll_win));

    let ui = Rc::new(RefCell::new(UI {
        window: scroll_win,
        header_bar,
        app: app.clone(),
        state: None,
    }));

    if let Some(fname) = args.get(1) {
        match from_str_to_uri(fname) {
            Ok(uri) => {
                ui.borrow_mut()
                    .load(&gtk::gio::File::for_uri(&uri))
                    .unwrap_or_else(|err| {
                        show_error_dialog(app, &format!("Error loading file: {}", err));
                    });
            }
            Err(err) => {
                show_error_dialog(
                    app,
                    &format!("Invalid file name: {:?}. Error: {}", fname, err),
                );
            }
        }
    }

    open_button.connect_clicked(clone!(
        #[strong]
        ui,
        #[weak]
        app,
        move |_| {
            open_file_dialog(&app, &ui);
        },
    ));

    window.present();
}

fn open_file_dialog(app: &Application, ui: &Rc<RefCell<UI>>) {
    let dialog = gtk::FileDialog::builder()
        .title("Open PDF File")
        .modal(true)
        .build();

    dialog.open(
        app.active_window().as_ref(),
        gtk::gio::Cancellable::NONE,
        clone!(
            #[strong]
            ui,
            #[strong]
            app,
            move |file| {
                match file {
                    Ok(file) => {
                        ui.borrow_mut().load(&file).unwrap_or_else(|err| {
                            show_error_dialog(&app, &format!("Error loading file: {}", err));
                        });
                    }
                    Err(err) => {
                        show_error_dialog(&app, &format!("Error opening file: {}", err));
                    }
                }
            }
        ),
    );
}

struct UI {
    window: ScrolledWindow,
    header_bar: gtk::HeaderBar,
    app: Application,
    // TODO use OnceCell when try_get_or_init is stable
    state: Option<state::State>,
}

impl UI {
    fn load(&mut self, f: &gtk::gio::File) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(state) = &self.state {
            state.load(f)?;
        } else {
            let state = self.init()?;
            state.load(f)?;
            self.state = Some(state.clone());
        };
        Ok(())
    }

    fn init(&self) -> Result<state::State, Box<dyn std::error::Error>> {
        let state = state::State::new();
        let model = gtk::gio::ListStore::new::<page::PageNumber>();
        let factory = gtk::SignalListItemFactory::new();
        let selection = gtk::SingleSelection::new(Some(model.clone()));
        let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        list_view.set_hexpand(true);
        list_view.set_orientation(gtk::Orientation::Horizontal);
        self.window.set_child(Some(&list_view));

        let scroll_controller = gtk::EventControllerScroll::new(
            EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
        );
        scroll_controller.connect_scroll(clone!(
            #[weak]
            selection,
            #[weak]
            model,
            #[weak(rename_to = window)]
            self.window,
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
                        + 4.0; // 2px is border. TODO: figure out how to un-hardcode this
                    window.hadjustment().set_value(current_pos - width);
                } else {
                    let width = selection
                        .selected_item()
                        .unwrap()
                        .downcast::<page::PageNumber>()
                        .unwrap()
                        .width() as f64
                        + 4.0; // 2px is border. TODO: figure out how to un-hardcode this

                    // scroll right
                    selection
                        .select_item((selection.selected() + 1).min(model.n_items() - 1), true);
                    window.hadjustment().set_value(current_pos + width);
                }

                glib::Propagation::Stop
            }
        ));
        list_view.add_controller(scroll_controller);

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
            .bind(&state, "page", gtk::Widget::NONE);

        let zoom_out_btn = Button::from_icon_name("zoom-out");
        zoom_out_btn.connect_clicked(clone!(
            #[weak]
            state,
            move |_| {
                state.set_zoom(state.zoom() / 1.1);
            }
        ));

        let zoom_in_btn = Button::from_icon_name("zoom-in");
        zoom_in_btn.connect_clicked(clone!(
            #[weak]
            state,
            move |_| {
                state.set_zoom(state.zoom() * 1.1);
            }
        ));

        let crop_btn = gtk::ToggleButton::builder()
            .icon_name("object-flip-horizontal")
            .build();

        crop_btn
            .bind_property("active", &state, "crop")
            .bidirectional()
            .build();

        self.header_bar.pack_start(&zoom_out_btn);
        self.header_bar.pack_start(&zoom_in_btn);
        self.header_bar.pack_end(&crop_btn);

        factory.connect_setup(clone!(
            #[weak]
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
            #[weak]
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

        self.app.connect_shutdown(clone!(
            #[strong]
            state,
            move |_| {
                if let Err(err) = state.save() {
                    eprintln!("Error saving state: {}", err);
                }
            }
        ));

        Ok(state)
    }
}

fn show_error_dialog(app: &Application, message: &str) {
    gtk::AlertDialog::builder()
        .message(message)
        .build()
        .show(app.active_window().as_ref());
}

fn from_str_to_uri(oss: &OsString) -> Result<String, std::io::Error> {
    if let Ok(u) = Uri::parse(&oss.to_string_lossy(), glib::UriFlags::NONE) {
        return Ok(u.to_string());
    }

    let path = PathBuf::from(&oss).canonicalize()?;
    if path.is_file() {
        return Ok(format!("file://{}", path.to_string_lossy()));
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("File not found: {:?}", oss),
    ))
}
