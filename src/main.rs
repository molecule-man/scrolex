use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib::Uri;
use gtk::{gio::ApplicationFlags, glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags, ScrolledWindow};
use page::PageManager;

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

    window.set_child(Some(&scroll_win));

    let ui = Rc::new(RefCell::new(UI {
        window: scroll_win,
        header_bar,
        app: app.clone(),
        pm: None,
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
    pm: Option<Rc<RefCell<PageManager>>>,
}

impl UI {
    fn load(&mut self, f: &gtk::gio::File) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(pm) = &self.pm {
            pm.borrow_mut().reset(f)?;
            pm.borrow_mut().load();
        } else {
            let pm = self.init(f)?;
            pm.borrow_mut().load();
            self.pm = Some(pm);
        }
        Ok(())
    }

    fn init(
        &self,
        f: &gtk::gio::File,
    ) -> Result<Rc<RefCell<PageManager>>, Box<dyn std::error::Error>> {
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
            list_view,
            #[weak]
            selection,
            #[weak]
            model,
            #[upgrade_or]
            glib::Propagation::Stop,
            move |_, _dx, dy| {
                if dy < 0.0 {
                    // scroll left
                    list_view.scroll_to(
                        selection.selected().saturating_sub(1),
                        gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                        None,
                    );
                } else {
                    list_view.scroll_to(
                        (selection.selected() + 1).min(model.n_items() - 1),
                        gtk::ListScrollFlags::FOCUS | gtk::ListScrollFlags::SELECT,
                        None,
                    );
                }

                glib::Propagation::Stop
            }
        ));
        list_view.add_controller(scroll_controller);

        let pm = PageManager::new(list_view, f)?;
        let pm = Rc::new(RefCell::new(pm));

        self.add_header_buttons(&pm);

        self.app.connect_shutdown(clone!(
            #[strong]
            pm,
            move |_| {
                pm.borrow().store_state();
            }
        ));

        Ok(pm)
    }

    fn add_header_buttons(&self, pm: &Rc<RefCell<PageManager>>) {
        self.header_bar
            .pack_start(&self.create_button("zoom-out", pm.clone(), |pm| {
                pm.apply_zoom(1. / 1.1);
            }));
        self.header_bar
            .pack_start(&self.create_button("zoom-in", pm.clone(), |pm| {
                pm.apply_zoom(1.1);
            }));

        let crop_btn = gtk::ToggleButton::builder()
            .icon_name("object-flip-horizontal")
            .build();

        crop_btn.connect_toggled(clone!(
            #[weak]
            pm,
            move |btn| {
                pm.borrow_mut().toggle_crop(btn.is_active());
            }
        ));
        self.header_bar.pack_end(&crop_btn);
    }

    fn create_button(
        &self,
        icon: &str,
        pm: Rc<RefCell<PageManager>>,
        on_click: impl Fn(&mut PageManager) + 'static,
    ) -> Button {
        let button = Button::from_icon_name(icon);
        button.connect_clicked(clone!(
            #[weak]
            pm,
            move |_| {
                on_click(&mut pm.borrow_mut());
            }
        ));
        button
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
