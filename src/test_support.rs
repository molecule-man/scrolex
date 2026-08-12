// Shared harness for the widget tests in window and document_view.
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;

// Size every test window shares.
pub const TEST_WINDOW: (i32, i32) = (900, 700);

// A window under test. The assertions mostly read the document, so it derefs to that; the window
// and the header bar have their own accessors.
pub struct TestWindow {
    window: crate::window::Window,
    document: crate::document_view::DocumentView,
}

impl std::ops::Deref for TestWindow {
    type Target = crate::document_view::DocumentView;

    fn deref(&self) -> &Self::Target {
        &self.document
    }
}

impl TestWindow {
    pub fn present(&self) {
        self.window.present();
    }

    pub fn close(&self) {
        self.window.close();
    }

    pub fn set_default_size(&self, width: i32, height: i32) {
        self.window.set_default_size(width, height);
    }

    // Header-bar children the document does not own.
    pub fn header(&self) -> &crate::window::imp::Window {
        self.window.imp()
    }
}

pub fn window() -> TestWindow {
    // before the window: it reads the settings while it builds
    crate::config::use_scratch_config();
    crate::state::use_scratch_state_dir();
    gtk::gio::resources_register_include!("scrolex-ui.gresource").expect("ui resources");
    crate::state::State::static_type();
    crate::page::PageNumber::static_type();
    crate::page::Page::static_type();
    crate::document_view::DocumentView::static_type();
    load_css();

    let window: crate::window::Window = gtk::glib::Object::new();
    // the stylesheet keys off this name, as in main
    window.set_widget_name("main");
    window.set_default_size(TEST_WINDOW.0, TEST_WINDOW.1);
    let document = window.active_document();
    TestWindow { window, document }
}

pub fn fixture(name: &str) -> gtk::gio::File {
    gtk::gio::File::for_path(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
}

// outline.pdf has 3 pages
pub fn loaded_window() -> TestWindow {
    let window = window();
    window.set_default_size(900, 700);
    window.present();
    window.state().load(&fixture("outline.pdf"));
    wait_until(|| window.imp().mapped_page(0).is_some());
    wait_until(|| window.imp().selection.n_items() == 3);

    window
}

// Type a zoom into the header entry and apply it, as the reader does.
pub fn type_zoom(window: &TestWindow, percent: &str) {
    let entry = window.header().entry_zoom.get();
    entry.set_text(percent);
    entry.emit_activate();
}

// The app's own stylesheet, so the tests measure the widgets the reader gets.
pub fn load_css() {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(include_str!("../ui/style.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().expect("display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

pub fn wait_until(mut ready: impl FnMut() -> bool) {
    let context = gtk::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for GTK");
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}
