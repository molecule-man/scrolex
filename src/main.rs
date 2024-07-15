use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::glib::Uri;
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
        loader.borrow_mut().load(fname).unwrap_or_else(|err| {
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
                    loader.borrow_mut().load(file).unwrap_or_else(|err| {
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

    fn load<D: DocumentOpen>(&mut self, opener: D) -> Result<(), Box<dyn std::error::Error>> {
        let mut loaded = None;
        std::mem::swap(&mut self.loaded, &mut loaded);
        match loaded {
            Some(mut loaded_instance) => {
                self.reload(&mut loaded_instance, opener)?;
                self.loaded = Some(loaded_instance);
            }
            None => {
                self.initialize(opener)?;
            }
        }
        Ok(())
    }

    fn reload<D: DocumentOpen>(
        &mut self,
        loaded: &mut Loaded,
        opener: D,
    ) -> Result<(), DocumentOpenError> {
        if let Err(err) = state::save(&loaded.uri.borrow(), &loaded.pm.borrow().current_state()) {
            eprintln!("Error saving state: {}", err);
        }

        let doc = opener.open()?;
        let uri = opener.uri_string();
        loaded.pm.borrow_mut().reload(doc, state::load(&uri));
        loaded.uri.replace(uri);
        Ok(())
    }

    fn initialize<D: DocumentOpen>(&mut self, opener: D) -> Result<(), DocumentOpenError> {
        let doc = opener.open()?;
        let uri = opener.uri_string();
        let uri_cell = Rc::new(RefCell::new(uri.clone()));
        let pm = self.init.init(
            doc,
            clone!(@strong uri_cell => move |pm| {
                if let Err(err) = state::save(&uri_cell.borrow(), &pm.current_state()) {
                    eprintln!("Error saving state: {}", err);
                }
            }),
        );
        pm.borrow_mut().load(state::load(&uri));
        self.loaded = Some(Loaded { pm, uri: uri_cell });

        Ok(())
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

        let crop_btn = gtk::ToggleButton::builder()
            .icon_name("object-flip-horizontal")
            .build();

        crop_btn.connect_toggled(clone!(@weak pm => move |btn| {
            pm.borrow_mut().toggle_crop(btn.is_active());
        }));
        self.header_bar.pack_end(&crop_btn);
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

#[derive(Debug)]
struct DocumentOpenError {
    message: String,
}

impl std::fmt::Display for DocumentOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Error opening document: {}", self.message)
    }
}

impl std::error::Error for DocumentOpenError {}

trait DocumentOpen {
    fn uri_string(&self) -> String;
    fn open(&self) -> Result<Document, DocumentOpenError>;
}

impl DocumentOpen for gtk::gio::File {
    fn uri_string(&self) -> String {
        self.uri().to_string()
    }

    fn open(&self) -> Result<Document, DocumentOpenError> {
        Document::from_gfile(self, None, gtk::gio::Cancellable::NONE).map_err(|err| {
            DocumentOpenError {
                message: err.to_string(),
            }
        })
    }
}

impl DocumentOpen for &OsString {
    fn uri_string(&self) -> String {
        if let Ok(u) = from_str_to_uri(self) {
            return u;
        }

        String::from("unknown")
    }

    fn open(&self) -> Result<Document, DocumentOpenError> {
        let uri = from_str_to_uri(self).map_err(|err| DocumentOpenError {
            message: err.to_string(),
        })?;

        Document::from_file(&uri, None).map_err(|err| DocumentOpenError {
            message: err.to_string(),
        })
    }
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
