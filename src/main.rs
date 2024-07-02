use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{gio::ApplicationFlags, glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags};
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
        loader.borrow_mut().load(Path::new(fname));
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
        clone!(@strong loader => move |file| {
            if let Ok(file) = file {
                if let Some(Ok(path)) = file.path().map(|p| p.canonicalize()) {
                    loader.borrow_mut().load(&path);
                }
            }
        }),
    );
}

struct Loaded {
    pm: Rc<RefCell<page::PageManager>>,
    path: Rc<RefCell<PathBuf>>,
}

struct Loader {
    loaded: Option<Loaded>,
    init: Init,
}

impl Loader {
    fn new(init: Init) -> Self {
        Self { init, loaded: None }
    }

    fn load(&mut self, path: &Path) {
        if let Ok(canonical_path) = path.canonicalize() {
            let mut loaded = None;
            std::mem::swap(&mut self.loaded, &mut loaded);
            match loaded {
                Some(mut loaded_instance) => {
                    self.reload(&mut loaded_instance, &canonical_path);
                    self.loaded = Some(loaded_instance);
                }
                None => {
                    self.initialize(&canonical_path);
                }
            }
        }
    }

    fn reload(&mut self, loaded: &mut Loaded, path: &Path) {
        if let Err(err) = state::save(
            loaded.path.borrow().as_path(),
            &loaded.pm.borrow().current_state(),
        ) {
            eprintln!("Error saving state: {}", err);
        }

        if let Ok(doc) = Document::from_file(&format!("file://{}", path.to_str().unwrap()), None) {
            loaded.pm.borrow_mut().reload(doc, state::load(path));
            loaded.path.replace(path.to_path_buf());
        }
    }

    fn initialize(&mut self, path: &Path) {
        if let Ok(doc) = Document::from_file(&format!("file://{}", path.to_str().unwrap()), None) {
            let path_buf = Rc::new(RefCell::new(path.to_path_buf()));
            let pm = self.init.init(doc, clone!(@strong path_buf => move |pm| {
                if let Err(err) = state::save(path_buf.borrow().as_path(), &pm.current_state()) {
                    eprintln!("Error saving state: {}", err);
                }
            }));
            pm.borrow_mut().load(state::load(path));
            self.loaded = Some(Loaded { pm, path: path_buf });
        }
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
        shutdown_fn: impl Fn(&page::PageManager) + 'static,
    ) -> Rc<RefCell<page::PageManager>> {
        let pages_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .build();

        let pm = Rc::new(RefCell::new(page::PageManager::new(doc, pages_box.clone())));
        let scroll_win = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&pages_box)
            .build();

        self.window.set_child(Some(&scroll_win));

        self.add_header_buttons(&pm);

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

    fn add_header_buttons(&self, pm: &Rc<RefCell<page::PageManager>>) {
        self.header_bar.pack_start(&self.button(
            "zoom-out",
            clone!(@strong pm => move |_| {
                pm.borrow_mut().apply_zoom(1. / 1.1);
            }),
        ));
        self.header_bar.pack_start(&self.button(
            "zoom-in",
            clone!(@strong pm => move |_| {
                pm.borrow_mut().apply_zoom(1.1);
            }),
        ));

        self.header_bar.pack_end(&self.button(
            "pan-end",
            clone!(@strong pm => move |_| {
                pm.borrow_mut().adjust_crop(0, 1);
            }),
        ));
        self.header_bar
            .pack_end(&gtk::Label::new(Some("Right crop")));
        self.header_bar.pack_end(&self.button(
            "pan-start",
            clone!(@strong pm => move |_| {
                pm.borrow_mut().adjust_crop(0, -1);
            }),
        ));

        self.header_bar.pack_end(&self.button(
            "pan-end",
            clone!(@strong pm => move |_| {
                pm.borrow_mut().adjust_crop(1, 0);
            }),
        ));
        self.header_bar
            .pack_end(&gtk::Label::new(Some("Left crop")));
        self.header_bar.pack_end(&self.button(
            "pan-start",
            clone!(@strong pm => move |_| {
                pm.borrow_mut().adjust_crop(-1, 0);
            }),
        ));
    }

    fn button(&self, icon: &str, on_click: impl Fn(&Button) + 'static) -> Button {
        let button = Button::from_icon_name(icon);
        button.connect_clicked(on_click);
        button
    }
}
