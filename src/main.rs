use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags};
use poppler::Document;

mod page;
mod state;

const APP_ID: &str = "com.andr2i.hallyview";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let header_bar = gtk::HeaderBar::builder().build();

    let open_button = Button::from_icon_name("document-open");

    header_bar.pack_start(&open_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .build();

    window.set_titlebar(Some(&header_bar));

    let loader = Loader::new(Init {
        window: window.clone(),
        header_bar,
        app: app.clone(),
    });

    let loader = Rc::new(RefCell::new(loader));

    open_button.connect_clicked(clone!(@strong loader, @weak app => move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .build();


        dialog.open(app.active_window().as_ref(), gtk::gio::Cancellable::NONE, clone!(@strong loader => move |file| {
            if let Ok(file) = file {
                let path = file.path().expect("File has no path").canonicalize().unwrap();
                loader.borrow_mut().load(&path);
            }
        }))
    }));

    window.present();
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
        match &mut self.loaded {
            Some(loaded) => {
                state::save(
                    loaded.path.borrow().as_path(),
                    &loaded.pm.borrow().current_state(),
                )
                .unwrap();

                loaded.pm.borrow_mut().reload(
                    Document::from_file(&format!("file://{}", path.to_str().unwrap()), None)
                        .unwrap(),
                    state::load(path),
                );

                loaded.path.replace(path.to_path_buf());
            }
            None => {
                let doc = Document::from_file(&format!("file://{}", path.to_str().unwrap()), None)
                    .unwrap();

                let path_buf = Rc::new(RefCell::new(path.to_path_buf()));
                let path_buf_clone = path_buf.clone();

                let pm = self.init.init(doc, move |pm| {
                    state::save(path_buf_clone.borrow().as_path(), &pm.current_state()).unwrap();
                });
                pm.borrow_mut().load(state::load(path));
                self.loaded = Some(Loaded { pm, path: path_buf });
            }
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

        let zoom_out_button = Button::from_icon_name("zoom-out");
        let zoom_in_button = Button::from_icon_name("zoom-in");

        self.header_bar.pack_start(&zoom_out_button);
        self.header_bar.pack_start(&zoom_in_button);

        let crop_left_minus_button = Button::from_icon_name("pan-start");
        let crop_left_text = gtk::Label::new(Some("Left crop"));
        let crop_left_plus_button = Button::from_icon_name("pan-end");

        let crop_right_minus_button = Button::from_icon_name("pan-start");
        let crop_right_text = gtk::Label::new(Some("Right crop"));
        let crop_right_plus_button = Button::from_icon_name("pan-end");

        self.header_bar.pack_end(&crop_right_plus_button);
        self.header_bar.pack_end(&crop_right_text);
        self.header_bar.pack_end(&crop_right_minus_button);

        self.header_bar.pack_end(&crop_left_plus_button);
        self.header_bar.pack_end(&crop_left_text);
        self.header_bar.pack_end(&crop_left_minus_button);

        let pm_clone = pm.clone();
        zoom_in_button.connect_clicked(move |_| {
            pm_clone.borrow_mut().apply_zoom(1.1);
        });

        let pm_clone = pm.clone();
        zoom_out_button.connect_clicked(move |_| {
            pm_clone.borrow_mut().apply_zoom(1. / 1.1);
        });

        let pm_clone = pm.clone();
        crop_left_plus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(1, 0));

        let pm_clone = pm.clone();
        crop_left_minus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(-1, 0));

        let pm_clone = pm.clone();
        crop_right_plus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(0, 1));

        let pm_clone = pm.clone();
        crop_right_minus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(0, -1));

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
}
