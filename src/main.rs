// Windows shows a console for a console-subsystem binary. Release builds are GUI only; debug
// builds keep the console so RUST_LOG output stays visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(
    rust_2018_idioms,
    future_incompatible,
    nonstandard_style,
    unused_lifetimes,
    clippy::pedantic
)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]

use std::ffi::OsString;

use gtk::gdk::Display;
use gtk::{gio::ApplicationFlags, glib, glib::clone, Application};
use gtk::{prelude::*, CssProvider};

use scrolex::config;
use scrolex::page;
use scrolex::window;

const APP_ID: &str = "com.andr2i.scrolex";
const RELEASE_NOTICE_TITLE: &str = "What's New";
const RELEASE_NOTICE_BODY: &str = "Dark mode is now available.\n\nOpen the Settings menu in the top-right corner and turn on Dark Mode.";
const RELEASE_NOTICE_BUTTON: &str = "Got It";

extern "C" {
    // POSIX _exit: terminate immediately without running atexit handlers or C++ static destructors
    // (see the shutdown handler for why we need that).
    #[link_name = "_exit"]
    fn libc_exit(status: i32) -> !;
}

fn main() -> glib::ExitCode {
    if std::env::args().any(|a| a == "-V" || a == "--version") {
        println!("scrolex {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }

    init_logging();

    // register types for usage in templates
    page::PageNumber::static_type();
    page::Page::static_type();
    scrolex::document_view::DocumentView::static_type();

    gtk::gio::resources_register_include!("scrolex-ui.gresource")
        .expect("Failed to register resources");

    let app = Application::builder()
        .application_id(APP_ID)
        .flags(ApplicationFlags::HANDLES_OPEN | ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.connect_startup(|_| {
        load_css();
    });
    setup_dark_mode(&app);
    setup_open_in_tabs(&app);
    app.connect_command_line(|app, cmd| {
        let args = cmd.arguments();
        let file = document_arg(&args).map(|arg| cmd.create_file_for_arg(arg));
        if !open_in_active_window(app, &args, file.as_ref()) {
            build_ui(app, &args, file.as_ref());
        }
        glib::ExitCode::SUCCESS
    });
    app.run_with_args(&std::env::args().collect::<Vec<_>>())
}

fn setup_dark_mode(app: &Application) {
    let enabled = config::load_config().dark_mode;
    scrolex::mupdf_render::set_dark_mode(enabled);

    let action = gtk::gio::SimpleAction::new_stateful("dark-mode", None, &enabled.to_variant());
    action.connect_activate(clone!(
        #[weak]
        app,
        move |action, _| {
            let enabled = !action
                .state()
                .and_then(|v| v.get::<bool>())
                .unwrap_or(false);
            action.set_state(&enabled.to_variant());
            scrolex::mupdf_render::set_dark_mode(enabled);

            let mut settings = config::load_config();
            settings.dark_mode = enabled;
            if let Err(err) = config::save_config(&settings) {
                eprintln!("Error saving config: {err}");
            }

            for gtk_window in app.windows() {
                if let Ok(window) = gtk_window.downcast::<window::Window>() {
                    window.apply_dark_mode(enabled);
                }
            }
        }
    ));
    app.add_action(&action);
}

fn setup_open_in_tabs(app: &Application) {
    let enabled = config::load_config().always_open_in_tabs;
    let action = gtk::gio::SimpleAction::new_stateful("open-in-tabs", None, &enabled.to_variant());
    action.connect_activate(|action, _| {
        let enabled = !action
            .state()
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false);
        action.set_state(&enabled.to_variant());

        let mut settings = config::load_config();
        settings.always_open_in_tabs = enabled;
        if let Err(err) = config::save_config(&settings) {
            eprintln!("Error saving config: {err}");
        }
    });
    app.add_action(&action);
}

// With tabs requested, a second launch adds a tab to the last active window. A full window,
// no open window, or no file argument creates a separate window.
fn open_in_active_window(
    app: &Application,
    args: &[OsString],
    file: Option<&gtk::gio::File>,
) -> bool {
    let wants_tab = tab_flag(args).unwrap_or_else(|| config::load_config().always_open_in_tabs);
    if !wants_tab || scrolex::emulate::config().is_some() {
        return false;
    }

    let Some(window) = app
        .active_window()
        .or_else(|| app.windows().into_iter().next())
        .and_downcast::<window::Window>()
    else {
        return false;
    };

    let Some(file) = file else {
        return false;
    };
    if !window.open_in_new_tab(file) {
        return false;
    }

    window.present();
    true
}

fn tab_flag(args: &[OsString]) -> Option<bool> {
    args.iter()
        .rev()
        .find_map(|arg| match arg.to_string_lossy().as_ref() {
            "--tab" => Some(true),
            "--new-window" => Some(false),
            _ => None,
        })
}

fn document_arg(args: &[OsString]) -> Option<&OsString> {
    args.iter()
        .skip(1)
        .find(|a| !a.to_string_lossy().starts_with('-'))
}

// Truncated on every launch, so it holds one session and never grows.
#[cfg(all(windows, not(debug_assertions)))]
fn session_log_file() -> Option<std::fs::File> {
    let mut path = glib::user_state_dir();
    path.push("scrolex");
    std::fs::create_dir_all(&path).ok()?;
    path.push("scrolex.log");
    std::fs::File::create(path).ok()
}

fn init_logging() {
    let verbose = std::env::args().any(|a| a == "-v" || a == "--verbose");
    let default_filter = if verbose { "scrolex=debug" } else { "warn" };

    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter));
    builder.format_timestamp_millis();

    // A windows release build has no console, so stderr goes nowhere and a bug report carries no
    // log. Write the session to a file instead. A debug build has a console, so it keeps stderr.
    #[cfg(all(windows, not(debug_assertions)))]
    if let Some(file) = session_log_file() {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    builder.init();
    gtk::glib::log_set_default_handler(gtk::glib::rust_log_handler);

    log::info!(
        "scrolex {} starting (verbose={verbose})",
        env!("CARGO_PKG_VERSION")
    );
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

fn build_ui(app: &Application, args: &[OsString], file: Option<&gtk::gio::File>) {
    let window = window::Window::new(app);
    window.set_widget_name("main");
    window.apply_dark_mode(scrolex::mupdf_render::dark_mode_enabled());

    if args.iter().any(|a| a == "-d" || a == "--debug") {
        window.add_css_class("debug");
    }

    let state = window.active_document().state().clone();

    app.connect_shutdown(clone!(
        #[strong]
        window,
        move |_| {
            let mut config = config::load_config();
            config.geometry = Some(config::Geometry {
                width: window.default_width(),
                height: window.default_height(),
                maximized: window.is_maximized(),
            });
            if let Err(err) = config::save_config(&config) {
                eprintln!("Error saving config: {err}");
            }

            for document in window.documents() {
                if let Err(err) = document.state().save() {
                    eprintln!("Error saving state for {}: {err}", document.state().uri());
                }
            }

            // The background render threads (bg_job) are detached and may be mid MuPDF render at
            // this point; a MuPDF render can't be interrupted. Terminating normally would let the
            // C library destructors free MuPDF/cairo/pixman globals out from under a
            // still-running render thread, which segfaults. State is saved above, so exit
            // immediately without running those destructors and let the OS reclaim everything.
            unsafe { libc_exit(0) };
        }
    ));

    if scrolex::emulate::config().is_some() {
        state.load(&gtk::gio::File::for_uri(scrolex::emulate::URI));
    } else if let Some(file) = file {
        state.load(file);
    }

    if let Some(geometry) = config::load_config().geometry {
        window.set_default_size(geometry.width, geometry.height);
        if geometry.maximized {
            window.maximize();
        }
    }

    window.present();
    show_release_notice(&window);
}

fn show_release_notice(window: &window::Window) {
    let notice = release_notice_id();
    if config::load_config().dismissed_notice == Some(notice) {
        return;
    }

    gtk::AlertDialog::builder()
        .message(RELEASE_NOTICE_TITLE)
        .detail(RELEASE_NOTICE_BODY)
        .buttons([RELEASE_NOTICE_BUTTON])
        .default_button(0)
        .cancel_button(0)
        .build()
        .choose(
            Some(window),
            None::<&gtk::gio::Cancellable>,
            move |result| {
                if result.is_err() {
                    return;
                }

                let mut settings = config::load_config();
                settings.dismissed_notice = Some(notice);
                if let Err(err) = config::save_config(&settings) {
                    eprintln!("Error saving config: {err}");
                }
            },
        );
}

fn release_notice_id() -> u64 {
    content_id(&[
        RELEASE_NOTICE_TITLE,
        RELEASE_NOTICE_BODY,
        RELEASE_NOTICE_BUTTON,
    ])
}

fn content_id(parts: &[&str]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn tab_flag_overrides_the_setting_and_the_last_one_wins() {
        assert_eq!(tab_flag(&args(["scrolex", "a.pdf"].as_slice())), None);
        assert_eq!(tab_flag(&args(&["scrolex", "--tab", "a.pdf"])), Some(true));
        assert_eq!(
            tab_flag(&args(&["scrolex", "--new-window", "a.pdf"])),
            Some(false)
        );
        assert_eq!(
            tab_flag(&args(&["scrolex", "--tab", "--new-window", "a.pdf"])),
            Some(false)
        );
    }

    #[test]
    fn document_arg_skips_flags() {
        assert_eq!(
            document_arg(&args(&["scrolex", "--tab", "-v", "a.pdf", "b.pdf"])),
            Some(&OsString::from("a.pdf"))
        );
        assert_eq!(document_arg(&args(&["scrolex", "--tab"])), None);
    }

    #[test]
    fn release_notice_id_is_stable_and_content_based() {
        let id = release_notice_id();
        assert_eq!(id, release_notice_id());
        assert_ne!(
            id,
            content_id(&["Changed", RELEASE_NOTICE_BODY, RELEASE_NOTICE_BUTTON])
        );
        assert_ne!(
            id,
            content_id(&[RELEASE_NOTICE_TITLE, "Changed", RELEASE_NOTICE_BUTTON])
        );
        assert_ne!(
            id,
            content_id(&[RELEASE_NOTICE_TITLE, RELEASE_NOTICE_BODY, "Changed"])
        );
    }
}
