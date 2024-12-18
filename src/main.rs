#![warn(
    rust_2018_idioms,
    future_incompatible,
    nonstandard_style,
    unused_lifetimes,
    clippy::pedantic
)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![expect(clippy::cast_possible_wrap)]
#![expect(clippy::cast_sign_loss)]
#![expect(clippy::cast_possible_truncation)]

use std::ffi::OsString;
use std::path::PathBuf;

use gtk::gdk::Display;
use gtk::glib::Uri;
use gtk::{gio::ApplicationFlags, glib, glib::clone, Application};
use gtk::{prelude::*, CssProvider};

//mod bg_job;
//mod jump_stack;
//mod links;
//mod page;
//mod poppler;
//mod state;
//mod window;
use scrolex::page;
use scrolex::window;

const APP_ID: &str = "com.andr2i.scrolex";

fn main() -> glib::ExitCode {
    #[cfg(feature = "logging")]
    {
        env_logger::init();
        gtk::glib::log_set_default_handler(gtk::glib::rust_log_handler);
    }

    // register types for usage in templates
    page::PageNumber::static_type();
    page::Page::static_type();

    gtk::gio::resources_register_include!("scrolex-ui.gresource")
        .expect("Failed to register resources");

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(ApplicationFlags::HANDLES_OPEN | ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.connect_startup(|_| {
        load_css();
    });
    app.connect_command_line(|app, cmd| {
        build_ui(app, &cmd.arguments());
        0
    });
    app.run_with_args(&std::env::args().collect::<Vec<_>>())
}

fn load_css() {
    // Load the CSS file and add it to the provider
    let provider = CssProvider::new();
    provider.load_from_string(include_str!("../ui/style.css"));

    // Add the provider to the default screen
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display."),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(app: &Application, args: &[OsString]) {
    let window = window::Window::new(app);
    window.set_widget_name("main");

    let state = window.state();

    app.connect_shutdown(clone!(
        #[strong]
        state,
        move |_| {
            if let Err(err) = state.save() {
                eprintln!("Error saving state: {err}");
            }
        }
    ));

    if let Some(fname) = args.get(1) {
        match from_str_to_uri(fname) {
            Ok(uri) => {
                state
                    .load(&gtk::gio::File::for_uri(&uri))
                    .unwrap_or_else(|err| {
                        window.show_error_dialog(&format!("Error loading file: {err}"));
                    });
            }
            Err(err) => {
                window.show_error_dialog(&format!("Invalid file name: {fname:?}. Error: {err}"));
            }
        }
    }

    window.present();
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
        format!("File not found: {oss:?}"),
    ))
}
