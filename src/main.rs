use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{gio::ApplicationFlags, glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, ScrolledWindow};
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

    let scroll_win = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .build();

    let model = gtk::gio::ListStore::new::<page::PageNumber>();
    let factory = gtk::SignalListItemFactory::new();
    let selection = gtk::SingleSelection::new(Some(model.clone()));
    let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
    list_view.set_hexpand(true);
    list_view.set_orientation(gtk::Orientation::Horizontal);
    scroll_win.set_child(Some(&list_view));

    window.set_child(Some(&scroll_win));

    let loader = Rc::new(RefCell::new(Loader::new(Init {
        window: scroll_win,
        list_view,
        header_bar,
        app: app.clone(),
    })));

    if let Some(fname) = args.get(1) {
        loader
            .borrow_mut()
            .load(Path::new(fname))
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
                    if let Some(path) = file.path() {
                        loader.borrow_mut().load(&path).unwrap();
                    } else {
                        show_error_dialog(&app, "No path returned from file dialog");
                    }
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

    fn load(&mut self, path: &Path) -> Result<(), std::io::Error> {
        let canonical_path = path.canonicalize()?;
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
        Ok(())
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
    window: ScrolledWindow,
    list_view: gtk::ListView,
    header_bar: gtk::HeaderBar,
    app: Application,
}

impl Init {
    fn init(
        &self,
        doc: Document,
        shutdown_fn: impl Fn(&PageManager) + 'static,
    ) -> Rc<RefCell<PageManager>> {
        let pm = Rc::new(RefCell::new(PageManager::new(self.list_view.clone(), doc)));

        self.add_header_buttons(&pm);
        self.window.set_child(Some(&pm.borrow().list_view()));

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
