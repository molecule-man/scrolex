use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{gio::ApplicationFlags, glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags};
use page::PageManager;
use poppler::Document;

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

fn build_ui(app: &Application, args: Vec<std::ffi::OsString>) {
    let header_bar = gtk::HeaderBar::builder().build();
    let open_button = Button::from_icon_name("document-open");

    header_bar.pack_start(&open_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .build();

    window.set_titlebar(Some(&header_bar));

    let loader = Rc::new(RefCell::new(Loader::new(Init {
        window: window.clone(),
        header_bar,
        app: app.clone(),
    })));

    if let Some(fname) = args.get(1) {
        let fname = PathBuf::from(fname).canonicalize().unwrap();
        loader
            .borrow_mut()
            .load(&format!("file://{}", fname.to_str().unwrap()))
            .unwrap_or_else(|err| {
                show_error_dialog(app, &format!("Error loading file: {}", err));
            });
    }

    open_button.connect_clicked(clone!(@strong loader, @weak app => move |_| {
        open_file_dialog(&app, &loader);
    }));

    window.present();
}

fn open_file_dialog(app: &Application, loader: &Rc<RefCell<Loader>>) {
    let dialog = gtk::FileDialog::builder()
        .title("Open PDF File")
        .modal(true)
        .build();

    dialog.open(
        app.active_window().as_ref(),
        gtk::gio::Cancellable::NONE,
        clone!(@strong loader, @strong app => move |file| {
            match file {
                Ok(file) => {
                    loader.borrow_mut().load(&file.uri()).unwrap_or_else(|err| {
                        show_error_dialog(&app, &format!("Error loading file: {}", err));
                    });
                }
                Err(err) => {
                    show_error_dialog(&app, &format!("Error opening file: {}", err));
                }
            }
        }),
    );
}

struct Loaded {
    pm: Rc<RefCell<PageManager>>,
    uri: Rc<RefCell<String>>,
}

struct Loader {
    loaded: Option<Loaded>,
    init: Init,
}

impl Loader {
    fn new(init: Init) -> Self {
        Self { init, loaded: None }
    }

    fn load(&mut self, uri: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut loaded = None;
        std::mem::swap(&mut self.loaded, &mut loaded);
        match loaded {
            Some(mut loaded_instance) => {
                self.reload(&mut loaded_instance, uri)?;
                self.loaded = Some(loaded_instance);
            }
            None => {
                self.initialize(uri)?;
            }
        }
        Ok(())
    }

    fn reload(&mut self, loaded: &mut Loaded, uri: &str) -> Result<(), glib::Error> {
        if let Err(err) = state::save(&loaded.uri.borrow(), &loaded.pm.borrow().current_state()) {
            eprintln!("Error saving state: {}", err);
        }

        let doc = Document::from_file(uri, None)?;
        loaded.pm.borrow_mut().reload(doc, state::load(uri));
        loaded.uri.replace(uri.to_owned());
        Ok(())
    }

    fn initialize(&mut self, uri: &str) -> Result<(), glib::Error> {
        let doc = Document::from_file(uri, None)?;
        let uri_cell = Rc::new(RefCell::new(uri.to_owned()));
        let pm = self.init.init(
            doc,
            clone!(@strong uri_cell => move |pm| {
                if let Err(err) = state::save(&uri_cell.borrow(), &pm.current_state()) {
                    eprintln!("Error saving state: {}", err);
                }
            }),
        );
        pm.borrow_mut().load(state::load(uri));
        self.loaded = Some(Loaded { pm, uri: uri_cell });

        Ok(())
    }
}

struct Init {
    window: ApplicationWindow,
    header_bar: gtk::HeaderBar,
    app: Application,
}

impl Init {
    fn init(
        &self,
        doc: Document,
        shutdown_fn: impl Fn(&PageManager) + 'static,
    ) -> Rc<RefCell<PageManager>> {
        let pages_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .build();

        let pm = Rc::new(RefCell::new(PageManager::new(doc, pages_box.clone())));

        self.add_header_buttons(&pm);

        let scroll_win = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&pages_box)
            .build();

        self.window.set_child(Some(&scroll_win));

        let scroll_controller = gtk::EventControllerScroll::new(
            EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
        );
        scroll_controller.connect_scroll(clone!( @weak scroll_win, @weak pages_box, @weak pm => @default-return glib::Propagation::Stop, move |_, _dx, dy| {
            if let Some(last_page) = pages_box.last_child() {
                let increment = last_page.width();
                // scroll by one page
                if dy < 0.0 {
                    // scroll left
                    if !pm.borrow_mut().shift_loading_buffer_left() {
                        scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() - increment as f64);
                    }
                } else {
                    // scroll right
                    if !pm.borrow_mut().shift_loading_buffer_right() {
                        scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() + increment as f64);
                    }
                }
            }

            glib::Propagation::Stop
        }));
        pages_box.add_controller(scroll_controller);

        self.app.connect_shutdown(clone!(@strong pm => move |_| {
            shutdown_fn(&pm.borrow());
        }));

        pm
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

        self.header_bar
            .pack_end(&self.create_button("pan-end", pm.clone(), |pm| {
                pm.adjust_crop(0, 1);
            }));
        self.header_bar
            .pack_end(&gtk::Label::new(Some("Right crop")));
        self.header_bar
            .pack_end(&self.create_button("pan-start", pm.clone(), |pm| {
                pm.adjust_crop(0, -1);
            }));

        self.header_bar
            .pack_end(&self.create_button("pan-end", pm.clone(), |pm| {
                pm.adjust_crop(1, 0);
            }));
        self.header_bar
            .pack_end(&gtk::Label::new(Some("Left crop")));
        self.header_bar
            .pack_end(&self.create_button("pan-start", pm.clone(), |pm| {
                pm.adjust_crop(-1, 0);
            }));
    }

    fn create_button(
        &self,
        icon: &str,
        pm: Rc<RefCell<PageManager>>,
        on_click: impl Fn(&mut PageManager) + 'static,
    ) -> Button {
        let button = Button::from_icon_name(icon);
        button.connect_clicked(clone!(@weak pm => move |_| {
            on_click(&mut pm.borrow_mut());
        }));
        button
    }
}

fn show_error_dialog(app: &Application, message: &str) {
    gtk::AlertDialog::builder()
        .message(message)
        .build()
        .show(app.active_window().as_ref());
}
