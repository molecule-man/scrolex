// Window chrome: the header bar, the settings menu, and the file chooser. The document itself
// lives in DocumentView.
use std::cell::RefCell;
use std::sync::OnceLock;

use glib::clone;
use glib::subclass::InitializingObject;
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, Button, CompositeTemplate, ToggleButton};

use crate::document_view::DocumentView;

// File types MuPDF opens. The chooser offers these before "All files".
const SUPPORTED_SUFFIXES: &[&str] = &[
    "pdf", "xps", "oxps", "epub", "mobi", "fb2", "cbz", "svg", "txt", "png", "jpg", "jpeg", "jp2",
    "jpx", "gif", "tif", "tiff", "bmp", "pnm", "pgm", "ppm", "pbm", "pam",
];

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/andr2i/scrolex/app.ui")]
pub struct Window {
    #[template_child]
    pub document: TemplateChild<DocumentView>,

    #[template_child]
    pub btn_crop: TemplateChild<ToggleButton>,
    #[template_child]
    pub btn_fit_height: TemplateChild<ToggleButton>,
    #[template_child]
    pub btn_animate_scroll: TemplateChild<ToggleButton>,
    #[template_child]
    pub btn_toc: TemplateChild<ToggleButton>,
    #[template_child]
    pub spin_threads: TemplateChild<gtk::SpinButton>,
    #[template_child]
    pub spin_cache: TemplateChild<gtk::SpinButton>,
    #[template_child]
    pub entry_page_num: TemplateChild<gtk::Entry>,
    #[template_child]
    pub entry_zoom: TemplateChild<gtk::Entry>,

    // The document the header bar acts on.
    active_document: RefCell<Option<DocumentView>>,

    // Two-way header bindings, dropped when the active document changes.
    header_bindings: RefCell<Vec<glib::Binding>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "MyApp";
    type Type = super::Window;
    type ParentType = gtk::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
        klass.bind_template_instance_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for Window {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecObject::builder::<DocumentView>("active-document")
                    .read_only()
                    .build(),
            ]
        })
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "active-document" => self.active_document.borrow().to_value(),
            name => unimplemented!("unknown property {name}"),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        if let Some(editable) = self.entry_page_num.delegate() {
            editable.connect_insert_text(|entry, s, _| {
                for c in s.chars() {
                    if !c.is_numeric() {
                        entry.stop_signal_emission_by_name("insert-text");
                    }
                }
            });
        }

        self.setup_thread_setting();
        self.setup_cache_setting();
        self.setup_animate_scroll();
        self.setup_drop_target();
        self.setup_search_keys();
        self.connect_document(&self.document.get());
        self.set_active_document(&self.document.get());

        // Drop the render-pool state when the window closes, so its entries don't linger.
        self.obj().connect_close_request(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                imp.document.release_renders();
                glib::Propagation::Proceed
            }
        ));
    }
}

#[gtk::template_callbacks]
impl Window {
    // Capture phase, on the window: Ctrl+F and F3 must reach the document wherever the focus
    // sits, including the header entries. Capture also stops Escape from double-firing the
    // search entry's own stop-search.
    fn setup_search_keys(&self) {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        key.connect_key_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _keycode, modifier| {
                imp.active_document().handle_search_key(keyval, modifier)
            }
        ));
        self.obj().add_controller(key);
    }

    // Per-document wiring, done once for every document the window owns.
    fn connect_document(&self, document: &DocumentView) {
        document.connect_closure(
            "open-requested",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |document: &DocumentView| imp.open_document_into(document)
            ),
        );
    }

    // The document the header bar and the menu act on.
    pub(crate) fn active_document(&self) -> DocumentView {
        self.active_document
            .borrow()
            .clone()
            .unwrap_or_else(|| self.document.get())
    }

    // Point the header bar at a document. The lookup chains in app.ui follow the
    // active-document property on their own; the bindings below cannot, so they are rebuilt here.
    pub(crate) fn set_active_document(&self, document: &DocumentView) {
        if self.active_document.borrow().as_ref() == Some(document) {
            return;
        }
        self.active_document.replace(Some(document.clone()));
        self.obj().notify("active-document");
        self.bind_header_to_document(document);
    }

    // Header controls that carry state both ways, which GtkBuilder cannot chain through a
    // property lookup.
    fn bind_header_to_document(&self, document: &DocumentView) {
        for binding in self.header_bindings.take() {
            binding.unbind();
        }
        let state = document.state();

        let bindings = vec![
            state
                .bind_property("crop", &*self.btn_crop, "active")
                .bidirectional()
                .sync_create()
                .build(),
            state
                .bind_property("animate-scroll", &*self.btn_animate_scroll, "active")
                .bidirectional()
                .sync_create()
                .build(),
            state
                .bind_property("fit-height", &*self.btn_fit_height, "active")
                .bidirectional()
                .sync_create()
                .build(),
            document
                .bind_property("toc-visible", &*self.btn_toc, "active")
                .bidirectional()
                .sync_create()
                .build(),
            document
                .bind_property("has-toc", &*self.btn_toc, "sensitive")
                .sync_create()
                .build(),
        ];

        self.header_bindings.replace(bindings);
    }

    #[template_callback]
    fn open_document(&self) {
        self.open_document_into(&self.active_document());
    }

    fn open_document_into(&self, document: &DocumentView) {
        let supported = gtk::FileFilter::new();
        supported.set_name(Some("Supported documents"));
        for suffix in SUPPORTED_SUFFIXES {
            supported.add_suffix(suffix);
        }
        let all = gtk::FileFilter::new();
        all.set_name(Some("All files"));
        all.add_pattern("*");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&supported);
        filters.append(&all);

        let dialog = gtk::FileDialog::builder()
            .title("Open Document")
            .modal(true)
            .filters(&filters)
            .default_filter(&supported)
            .build();

        let obj = self.obj();
        let document = document.clone();
        dialog.open(
            Some(obj.as_ref()),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[strong]
                document,
                #[strong]
                obj,
                move |file| match file {
                    Ok(file) => document.state().load(&file),
                    Err(err) => {
                        obj.show_error_dialog(&format!("Error opening file: {err}"));
                    }
                },
            ),
        );
    }

    fn setup_drop_target(&self) {
        let drop_target = gtk::DropTarget::new(
            gtk::gdk::FileList::static_type(),
            gtk::gdk::DragAction::COPY,
        );

        drop_target.connect_drop(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                let Ok(files) = value.get::<gtk::gdk::FileList>() else {
                    return false;
                };
                let Some(file) = files.files().into_iter().next() else {
                    return false;
                };

                imp.active_document().state().load(&file);
                true
            }
        ));

        self.obj().add_controller(drop_target);
    }

    // Load the render-thread setting into the spin button and pool, and persist any user change.
    fn setup_thread_setting(&self) {
        let max = crate::config::max_render_threads();
        let threads = crate::config::load_config().render_threads;
        self.spin_threads.set_range(1.0, max as f64);
        self.spin_threads.set_increments(1.0, 1.0);
        self.spin_threads.set_value(threads as f64);
        self.apply_render_threads(threads);

        self.spin_threads.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |spin| {
                let n = spin.value() as usize;
                imp.apply_render_threads(n);
                let mut config = crate::config::load_config();
                config.render_threads = n;
                if let Err(e) = crate::config::save_config(&config) {
                    eprintln!("Error saving config: {e}");
                }
            }
        ));
    }

    fn apply_render_threads(&self, n: usize) {
        log::info!("Render threads: {n}");
        self.document.state().set_render_threads(n);
        crate::page::set_render_threads(n);
    }

    fn setup_cache_setting(&self) {
        let mb = crate::config::load_config().render_cache_mb;
        self.spin_cache.set_range(
            crate::config::MIN_RENDER_CACHE_MB as f64,
            crate::config::MAX_RENDER_CACHE_MB as f64,
        );
        self.spin_cache.set_increments(32.0, 64.0);
        self.spin_cache.set_value(mb as f64);
        self.document.state().set_render_cache_mb(mb);

        self.spin_cache.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |spin| {
                let mb = spin.value() as usize;
                imp.document.state().set_render_cache_mb(mb);
                let mut config = crate::config::load_config();
                config.render_cache_mb = mb;
                if let Err(e) = crate::config::save_config(&config) {
                    eprintln!("Error saving config: {e}");
                }
            }
        ));
    }

    fn setup_animate_scroll(&self) {
        let state = self.document.state();
        state.set_animate_scroll(crate::config::load_config().animate_scroll);

        state.connect_notify_local(Some("animate-scroll"), |state, _| {
            let mut config = crate::config::load_config();
            config.animate_scroll = state.animate_scroll();
            if let Err(e) = crate::config::save_config(&config) {
                eprintln!("Error saving config: {e}");
            }
        });
    }

    #[template_callback]
    fn zoom_in(&self) {
        self.active_document().zoom_in();
    }

    #[template_callback]
    fn zoom_out(&self) {
        self.active_document().zoom_out();
    }

    #[template_callback]
    fn jump_back(&self) {
        self.active_document().jump_back();
    }

    #[template_callback]
    fn jump_forward(&self) {
        self.active_document().jump_forward();
    }

    #[template_callback]
    fn handle_page_number_entered(&self, entry: &gtk::Entry) {
        let Ok(page_num) = entry.text().parse::<u32>() else {
            return;
        };

        self.active_document().goto_page(page_num);
    }

    #[template_callback]
    fn handle_page_number_icon_pressed(&self, _: gtk::EntryIconPosition, entry: &gtk::Entry) {
        self.handle_page_number_entered(entry);
    }

    #[template_callback]
    fn handle_zoom_entry(&self, entry: &gtk::Entry) {
        let Ok(percent) = entry.text().parse::<f64>() else {
            return;
        };

        self.active_document().apply_zoom_percent(percent);
    }

    #[template_callback]
    fn handle_zoom_entry_icon(&self, _: gtk::EntryIconPosition, entry: &gtk::Entry) {
        self.handle_zoom_entry(entry);
    }

    #[template_callback]
    fn menu_search(&self, btn: &Button) {
        dismiss_menu(btn);
        self.active_document().open_search();
    }

    #[template_callback]
    fn menu_about(&self, btn: &Button) {
        dismiss_menu(btn);
        crate::about::present(self.obj().upcast_ref());
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn can_jump_back(&self, prev_page: u32) -> bool {
        prev_page > 0
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn back_btn_text(&self, prev_page: u32) -> String {
        format!("Jump back to page {prev_page}")
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn can_jump_forward(&self, next_page: u32) -> bool {
        next_page > 0
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn forward_btn_text(&self, next_page: u32) -> String {
        format!("Jump forward to page {next_page}")
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn page_entry_text(&self, page: i32) -> String {
        format!("{}", page + 1)
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn zoom_entry_text(&self, zoom_value: f64) -> String {
        crate::state::zoom_percent_text(zoom_value)
    }

    // Dims the page entry's jump icon while pressing it would scroll nowhere. Runs while the
    // template is still building, so it must not touch an unbound template child.
    #[template_callback]
    fn page_jump_enabled(&self, text: &str, page: u32) -> bool {
        let Ok(page_num) = text.parse::<u32>() else {
            return false;
        };
        let Some(document) = self.document.try_get() else {
            return false;
        };
        let document: DocumentView = document;

        document
            .target_page(page_num)
            .is_some_and(|target| target != page + 1)
    }

    // Dims the zoom entry's apply icon while pressing it would not change the zoom.
    #[allow(clippy::unused_self)]
    #[template_callback]
    fn zoom_apply_enabled(&self, text: &str, zoom: f64) -> bool {
        let Ok(percent) = text.parse::<f64>() else {
            return false;
        };

        // compared at the precision the entry shows, so applying what is already displayed counts
        // as no change
        crate::state::zoom_from_percent(percent).is_some_and(|target| {
            crate::state::zoom_percent_text(target) != crate::state::zoom_percent_text(zoom)
        })
    }
}

impl WidgetImpl for Window {}
impl WindowImpl for Window {}
impl ApplicationWindowImpl for Window {}

fn dismiss_menu(btn: &Button) {
    if let Some(popover) = btn
        .ancestor(gtk::Popover::static_type())
        .and_downcast::<gtk::Popover>()
    {
        popover.popdown();
    }
}

#[cfg(test)]
mod widget_tests {
    use crate::test_support::{loaded_window, wait_until};
    use gtk::prelude::*;
    use gtk::subclass::prelude::ObjectSubclassIsExt;

    // The reader can be typing a page number when they reach for Ctrl+F. The controller sits on
    // the window, not the document, so the header entries do not swallow it.
    #[gtk::test]
    fn search_shortcuts_reach_the_document_from_the_header_entry() {
        let window = loaded_window();
        let imp = window.imp();
        window.header().entry_page_num.grab_focus();
        assert!(!imp.search_bar.is_search_mode(), "closed at rest");

        let opened =
            window.handle_search_key(gtk::gdk::Key::f, gtk::gdk::ModifierType::CONTROL_MASK);

        assert_eq!(opened, gtk::glib::Propagation::Stop, "the window took it");
        assert!(imp.search_bar.is_search_mode(), "search opened");

        let closed =
            window.handle_search_key(gtk::gdk::Key::Escape, gtk::gdk::ModifierType::empty());

        assert_eq!(closed, gtk::glib::Propagation::Stop);
        assert!(!imp.search_bar.is_search_mode(), "Escape closed it");
        window.close();
    }

    // Issue #53: a live icon that does nothing reads as broken. Covers the binding wiring;
    // page_jump_enabled_only_where_the_jump_moves covers the decision.
    #[gtk::test]
    fn page_jump_icon_follows_the_entry() {
        let window = loaded_window();
        let entry = window.header().entry_page_num.get();
        let icon = gtk::EntryIconPosition::Secondary;

        assert_eq!(entry.text(), "1");
        assert!(!entry.icon_is_sensitive(icon), "at rest on page 1");
        assert!(entry.secondary_icon_tooltip_text().is_some(), "tooltip set");

        entry.set_text("3");
        assert!(entry.icon_is_sensitive(icon), "another page");

        entry.set_text("1");
        assert!(!entry.icon_is_sensitive(icon), "back to the current page");

        window.close();
    }

    // handle_document_load fills the model in two stages, so a restored page leaves one item in it
    // for a moment. Count from the document, or the icon lights up for the pages not yet in there.
    #[gtk::test]
    fn page_jump_icon_ignores_a_half_filled_model() {
        let window = loaded_window();
        let imp = window.imp();
        let entry = window.header().entry_page_num.get();

        imp.model.remove_all();
        imp.model.append(&crate::page::PageNumber::new(1));
        imp.selection.set_selected(0);
        wait_until(|| window.state().page() == 1);

        assert_eq!(entry.text(), "2", "the entry follows the selected page");
        assert!(
            !entry.icon_is_sensitive(gtk::EntryIconPosition::Secondary),
            "page 2 of 3 with one item in the model"
        );

        window.close();
    }

    #[gtk::test]
    fn page_jump_enabled_only_where_the_jump_moves() {
        let window = loaded_window();
        let header = window.header();

        assert!(header.page_jump_enabled("3", 0), "page 3 while on page 1");
        assert!(!header.page_jump_enabled("1", 0), "page 1 while on page 1");
        assert!(!header.page_jump_enabled("3", 2), "page 3 while on page 3");
        assert!(!header.page_jump_enabled("", 0), "no number");
        assert!(!header.page_jump_enabled("abc", 0), "not a number");
        assert!(
            header.page_jump_enabled("9999", 0),
            "clamped to the last page"
        );
        assert!(
            !header.page_jump_enabled("9999", 2),
            "clamped to the page we are on"
        );
        // 0 lands on page 1
        assert!(!header.page_jump_enabled("0", 0), "page 0 while on page 1");
        assert!(header.page_jump_enabled("0", 1), "page 0 while on page 2");

        window.close();
    }

    #[gtk::test]
    fn zoom_apply_icon_follows_the_entry() {
        let window = loaded_window();
        let entry = window.header().entry_zoom.get();
        let icon = gtk::EntryIconPosition::Secondary;

        assert_eq!(entry.text(), "100");
        assert!(!entry.icon_is_sensitive(icon), "at rest at 100%");
        assert!(entry.secondary_icon_tooltip_text().is_some(), "tooltip set");

        entry.set_text("150");
        assert!(entry.icon_is_sensitive(icon), "another zoom level");

        entry.set_text("100");
        assert!(!entry.icon_is_sensitive(icon), "back to the current zoom");

        window.close();
    }

    #[gtk::test]
    fn zoom_apply_enabled_only_where_the_zoom_changes() {
        let window = loaded_window();
        let header = window.header();

        assert!(header.zoom_apply_enabled("150", 1.0), "150% while at 100%");
        assert!(!header.zoom_apply_enabled("100", 1.0), "100% while at 100%");
        assert!(!header.zoom_apply_enabled("", 1.0), "no number");
        assert!(!header.zoom_apply_enabled("abc", 1.0), "not a number");
        assert!(
            !header.zoom_apply_enabled("4", 1.0),
            "below the smallest zoom"
        );
        assert!(header.zoom_apply_enabled("5", 1.0), "the smallest zoom");
        assert!(
            header.zoom_apply_enabled("9999", 1.0),
            "clamped to the largest zoom"
        );
        assert!(
            !header.zoom_apply_enabled("9999", 10.0),
            "clamped to the current zoom"
        );
        assert!(
            !header.zoom_apply_enabled(&header.zoom_entry_text(0.07), 0.07),
            "7% round-trips as no change"
        );
        // the entry rounds, so what it shows for an odd zoom must still read as no change
        let odd = 3.331_061_493_552_564;
        assert_eq!(header.zoom_entry_text(odd), "333.11");
        assert!(
            !header.zoom_apply_enabled(&header.zoom_entry_text(odd), odd),
            "the rounded percent the entry shows"
        );
        assert!(
            header.zoom_apply_enabled("333.2", odd),
            "a percent that differs at two decimals"
        );

        window.close();
    }
}
