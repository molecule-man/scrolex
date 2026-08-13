// Window chrome: the header bar, the settings menu, and the file chooser. The document itself
// lives in DocumentView.
use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use glib::clone;
use glib::subclass::InitializingObject;
use gtk::glib::subclass::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, Button, CompositeTemplate, ToggleButton};

use crate::document_view::DocumentView;

// Tabs stop being a useful way to hold documents well before this. The cap also limits each
// document's render state and widget tree.
const MAX_DOCUMENTS: u32 = 16;

// File types MuPDF opens. The chooser offers these before "All files".
const SUPPORTED_SUFFIXES: &[&str] = &[
    "pdf", "xps", "oxps", "epub", "mobi", "fb2", "cbz", "svg", "txt", "png", "jpg", "jpeg", "jp2",
    "jpx", "gif", "tif", "tiff", "bmp", "pnm", "pgm", "ppm", "pbm", "pam",
];

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/andr2i/scrolex/app.ui")]
pub struct Window {
    // The only document collection. Nothing else keeps a list.
    #[template_child]
    pub notebook: TemplateChild<gtk::Notebook>,

    #[template_child]
    pub btn_add_tab: TemplateChild<Button>,
    #[template_child]
    pub btn_menu_add_tab: TemplateChild<Button>,
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
    pub label_title: TemplateChild<gtk::Label>,
    #[template_child]
    pub entry_page_num: TemplateChild<gtk::Entry>,
    #[template_child]
    pub entry_zoom: TemplateChild<gtk::Entry>,

    // The document the header bar acts on.
    active_document: RefCell<Option<DocumentView>>,

    // Two-way header bindings, dropped when the active document changes.
    header_bindings: RefCell<Vec<glib::Binding>>,

    // Prevent global animate-scroll updates from calling each other.
    animate_scroll_sync: Cell<bool>,

    // Prevent application-wide spin updates from saving the same value again.
    setting_controls_sync: Cell<bool>,

    render_threads: Cell<usize>,
    render_cache_mb: Cell<usize>,
    preview_cache_pages: Cell<usize>,
    animate_scroll: Cell<bool>,
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

        let config = crate::config::load_config();
        self.render_threads.set(config.render_threads);
        self.render_cache_mb.set(config.render_cache_mb);
        self.preview_cache_pages.set(config.preview_cache_pages);
        self.animate_scroll.set(config.animate_scroll);

        if let Some(editable) = self.entry_page_num.delegate() {
            editable.connect_insert_text(|entry, s, _| {
                for c in s.chars() {
                    if !c.is_numeric() {
                        entry.stop_signal_emission_by_name("insert-text");
                    }
                }
            });
        }

        self.setup_notebook();
        self.add_document();
        self.setup_thread_setting();
        self.setup_cache_setting();
        self.setup_drop_target();
        self.setup_window_keys();

        // Drop each document's render-pool state when the window closes.
        self.obj().connect_close_request(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                for document in imp.documents() {
                    document.release_renders();
                }
                let current = imp.obj().clone();
                let documents = imp
                    .application_windows()
                    .into_iter()
                    .filter(|window| window != &current)
                    .flat_map(|window| window.imp().documents())
                    .collect();
                imp.share_cache_budgets_between(documents);
                glib::Propagation::Proceed
            }
        ));
    }
}

#[gtk::template_callbacks]
impl Window {
    // Capture phase, on the window: the tab, fullscreen, and search keys must work wherever the
    // focus sits, including the header entries and the contents panel. Capture also stops Escape
    // from double-firing the search entry's own stop-search.
    fn setup_window_keys(&self) {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        key.connect_key_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _keycode, modifier| imp.handle_window_key(keyval, modifier)
        ));
        self.obj().add_controller(key);
    }

    pub(crate) fn handle_window_key(
        &self,
        keyval: gtk::gdk::Key,
        modifier: gtk::gdk::ModifierType,
    ) -> glib::Propagation {
        use gtk::gdk::Key;

        let control = gtk::gdk::ModifierType::CONTROL_MASK;
        let control_shift = control | gtk::gdk::ModifierType::SHIFT_MASK;
        let modifiers = modifier & gtk::accelerator_get_default_mod_mask();
        if modifiers.is_empty() && keyval == Key::F11 {
            let window = self.obj();
            window.set_fullscreened(!window.is_fullscreen());
            return glib::Propagation::Stop;
        }
        if modifiers == control {
            match keyval {
                Key::t | Key::T => {
                    self.add_tab();
                    return glib::Propagation::Stop;
                }
                // The last document has no tab left to fall back to, so the window goes with it.
                Key::w | Key::W => {
                    match self.active_document() {
                        Some(document) if self.notebook.n_pages() > 1 => {
                            self.close_document(&document);
                        }
                        _ => self.obj().close(),
                    }
                    return glib::Propagation::Stop;
                }
                Key::Page_Down | Key::KP_Page_Down | Key::Tab => {
                    self.switch_document(1);
                    return glib::Propagation::Stop;
                }
                Key::Page_Up | Key::KP_Page_Up => {
                    self.switch_document(-1);
                    return glib::Propagation::Stop;
                }
                _ => {}
            }
        }
        // Most layouts send ISO_Left_Tab for Shift+Tab, but not all of them.
        if modifiers == control_shift && matches!(keyval, Key::ISO_Left_Tab | Key::Tab) {
            self.switch_document(-1);
            return glib::Propagation::Stop;
        }

        let doc = self.active_document();
        let taken_doc = doc.as_ref().map_or(glib::Propagation::Proceed, |document| {
            document.handle_search_key(keyval, modifier)
        });
        if matches!(taken_doc, glib::Propagation::Stop) {
            return taken_doc;
        }

        let selected = doc.is_some_and(|doc| doc.state().has_selection());
        if modifiers.is_empty() && keyval == Key::Escape && !selected && self.obj().is_fullscreen()
        {
            self.obj().set_fullscreened(false);
            return glib::Propagation::Stop;
        }

        taken_doc
    }

    fn setup_notebook(&self) {
        self.notebook.connect_switch_page(clone!(
            #[weak(rename_to = imp)]
            self,
            move |_, page, _| {
                if let Some(document) = page.downcast_ref::<DocumentView>() {
                    imp.set_active_document(document);
                }
            }
        ));
    }

    fn at_document_limit(&self) -> bool {
        self.notebook.n_pages() >= MAX_DOCUMENTS
    }

    // Add an empty document and show it, or None at the limit. The caller loads a file into it.
    pub(crate) fn add_document(&self) -> Option<DocumentView> {
        if self.at_document_limit() {
            return None;
        }

        let document = DocumentView::new();

        let state = document.state();
        state.set_render_threads(self.render_threads.get());
        state.set_animate_scroll(self.animate_scroll.get());
        state.connect_animate_scroll_notify(clone!(
            #[weak(rename_to = imp)]
            self,
            move |state| imp.apply_animate_scroll(state.animate_scroll())
        ));
        document.connect_closure(
            "open-requested",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |document: &DocumentView| imp.open_document_into(document)
            ),
        );

        let page = self
            .notebook
            .append_page(&document, Some(&self.tab_label(&document)));
        // Tabs share the bar and ellipsize as more of them open.
        self.notebook.page(&document).set_tab_expand(true);
        self.notebook.set_current_page(Some(page));
        self.update_tab_bar();
        self.share_cache_budgets();

        Some(document)
    }

    // The window keeps one document at all times, so the last tab does not close. The notebook
    // picks the replacement tab and its switch-page moves the header.
    pub(crate) fn close_document(&self, document: &DocumentView) {
        if self.notebook.n_pages() <= 1 {
            return;
        }
        let Some(page) = self.notebook.page_num(document) else {
            return;
        };

        let state = document.state();
        if !state.uri().is_empty() {
            if let Err(err) = state.save() {
                eprintln!("Error saving state for {}: {err}", state.uri());
            }
        }
        self.notebook.remove_page(Some(page));
        self.update_tab_bar();
        self.share_cache_budgets();
    }

    fn switch_document(&self, step: i32) {
        let count = self.notebook.n_pages() as i32;
        if count < 2 {
            return;
        }
        let current = self.notebook.current_page().unwrap_or(0) as i32;
        let next = (current + step).rem_euclid(count);
        self.notebook.set_current_page(Some(next as u32));
    }

    // GtkNotebook neither shrinks a tab nor closes one, so the label carries both.
    fn tab_label(&self, document: &DocumentView) -> gtk::Widget {
        let name = gtk::Label::new(None);
        // width-chars is the floor a tab never shrinks past, max-width-chars the ceiling it never
        // grows past. Between them the name ellipsizes to whatever the tab bar can spare.
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_width_chars(10);
        name.set_max_width_chars(24);
        name.set_hexpand(true);
        name.set_xalign(0.0);

        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_focus_on_click(false);
        close.set_tooltip_text(Some("Close this document (Ctrl+W)"));
        close.connect_clicked(clone!(
            #[weak(rename_to = imp)]
            self,
            #[weak]
            document,
            move |_| imp.close_document(&document)
        ));

        let label = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        label.append(&name);
        label.append(&close);

        let state = document.state();
        state
            .bind_property("uri", &name, "label")
            .transform_to(|_, uri: String| Some(display_name(&uri)))
            .sync_create()
            .build();
        state
            .bind_property("uri", &label, "tooltip-text")
            .transform_to(|_, uri: String| Some(document_path(&uri)))
            .sync_create()
            .build();

        label.upcast()
    }

    pub(crate) fn documents(&self) -> Vec<DocumentView> {
        (0..self.notebook.n_pages())
            .filter_map(|i| self.notebook.nth_page(Some(i)))
            .filter_map(|page| page.downcast::<DocumentView>().ok())
            .collect()
    }

    fn application_windows(&self) -> Vec<super::Window> {
        let mut windows = self.obj().application().map_or_else(
            || vec![self.obj().clone()],
            |application| {
                application
                    .windows()
                    .into_iter()
                    .filter_map(|window| window.downcast::<super::Window>().ok())
                    .collect()
            },
        );
        let current = self.obj().clone();
        if !windows.contains(&current) {
            windows.push(current);
        }
        windows
    }

    fn application_documents(&self) -> Vec<DocumentView> {
        self.application_windows()
            .into_iter()
            .flat_map(|window| window.imp().documents())
            .collect()
    }

    pub(crate) fn inherit_application_settings(&self) {
        let current = self.obj().clone();
        let Some(source) = self
            .application_windows()
            .into_iter()
            .find(|window| window != &current)
        else {
            return;
        };
        let source = source.imp();
        let render_threads = source.render_threads.get();
        let render_cache_mb = source.render_cache_mb.get();
        let preview_cache_pages = source.preview_cache_pages.get();
        let animate_scroll = source.animate_scroll.get();

        self.preview_cache_pages.set(preview_cache_pages);
        self.apply_render_threads(render_threads);
        self.apply_cache_budgets(render_cache_mb);

        self.animate_scroll_sync.set(true);
        self.animate_scroll.set(animate_scroll);
        for document in self.documents() {
            document.state().set_animate_scroll(animate_scroll);
        }
        self.animate_scroll_sync.set(false);
    }

    // The document the header bar and the menu act on. None while GtkBuilder still builds the
    // template, before the first document exists.
    pub(crate) fn active_document(&self) -> Option<DocumentView> {
        self.active_document.borrow().clone()
    }

    // A single document needs no tab bar.
    fn update_tab_bar(&self) {
        self.notebook.set_show_tabs(self.notebook.n_pages() > 1);
        let can_add = !self.at_document_limit();
        self.btn_add_tab.set_sensitive(can_add);
        self.btn_menu_add_tab.set_sensitive(can_add);
    }

    // Point the header bar at a document. The lookup chains in app.ui follow the
    // active-document property on their own; the bindings below cannot, so they are rebuilt here.
    fn set_active_document(&self, document: &DocumentView) {
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
            state
                .bind_property("uri", &*self.obj(), "title")
                .transform_to(|_, uri: String| Some(window_title(&uri)))
                .sync_create()
                .build(),
        ];

        self.header_bindings.replace(bindings);
    }

    #[template_callback]
    fn open_document(&self) {
        if let Some(document) = self.active_document() {
            self.open_document_into(&document);
        }
    }

    fn open_document_into(&self, document: &DocumentView) {
        let document = document.clone();
        self.choose_document(move |file| document.state().load(&file));
    }

    #[template_callback]
    fn add_tab(&self) {
        if self.at_document_limit() {
            return;
        }
        self.choose_document(clone!(
            #[weak(rename_to = imp)]
            self,
            move |file| imp.open_in_new_tab(&file)
        ));
    }

    #[template_callback]
    fn menu_add_tab(&self, btn: &Button) {
        dismiss_menu(btn);
        self.add_tab();
    }

    // Fill an idle empty tab instead of adding another tab.
    pub(crate) fn open_in_new_tab(&self, file: &gtk::gio::File) {
        let document = match self.active_document() {
            Some(active) if active.state().n_pages() == 0 && !active.is_loading() => Some(active),
            _ => self.add_document(),
        };
        if let Some(document) = document {
            document.state().load(file);
        }
    }

    // Ask the reader for a document. A dismissed chooser changes nothing.
    fn choose_document(&self, load: impl FnOnce(gtk::gio::File) + 'static) {
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
        dialog.open(
            Some(obj.as_ref()),
            gtk::gio::Cancellable::NONE,
            clone!(
                #[strong]
                obj,
                move |file| match file {
                    Ok(file) => load(file),
                    Err(err) if err.matches(gtk::DialogError::Dismissed) => {}
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
                let Some(document) = imp.active_document() else {
                    return false;
                };

                document.state().load(&file);
                true
            }
        ));

        self.obj().add_controller(drop_target);
    }

    // Load the render-thread setting into the spin button and pool, and persist any user change.
    fn setup_thread_setting(&self) {
        let max = crate::config::max_render_threads();
        let threads = self.render_threads.get();
        self.spin_threads.set_range(1.0, max as f64);
        self.spin_threads.set_increments(1.0, 1.0);
        self.spin_threads.set_value(threads as f64);
        self.apply_render_threads(threads);

        self.spin_threads.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |spin| {
                if imp.setting_controls_sync.get() {
                    return;
                }
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
        let windows = self.application_windows();
        for window in &windows {
            let imp = window.imp();
            imp.setting_controls_sync.set(true);
            imp.render_threads.set(n);
            imp.spin_threads.set_value(n as f64);
        }
        for document in self.application_documents() {
            document.state().set_render_threads(n);
        }
        for window in windows {
            window.imp().setting_controls_sync.set(false);
        }
        crate::page::set_render_threads(n);
    }

    fn setup_cache_setting(&self) {
        let mb = self.render_cache_mb.get();
        self.spin_cache.set_range(
            crate::config::MIN_RENDER_CACHE_MB as f64,
            crate::config::MAX_RENDER_CACHE_MB as f64,
        );
        self.spin_cache.set_increments(32.0, 64.0);
        self.spin_cache.set_value(mb as f64);
        self.apply_cache_budgets(mb);

        self.spin_cache.connect_value_changed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |spin| {
                if imp.setting_controls_sync.get() {
                    return;
                }
                let mb = spin.value() as usize;
                imp.apply_cache_budgets(mb);
                let mut config = crate::config::load_config();
                config.render_cache_mb = mb;
                if let Err(e) = crate::config::save_config(&config) {
                    eprintln!("Error saving config: {e}");
                }
            }
        ));
    }

    fn apply_cache_budgets(&self, render_cache_mb: usize) {
        let preview_cache_pages = self.preview_cache_pages.get();
        let windows = self.application_windows();
        for window in &windows {
            let imp = window.imp();
            imp.setting_controls_sync.set(true);
            imp.render_cache_mb.set(render_cache_mb);
            imp.preview_cache_pages.set(preview_cache_pages);
            imp.spin_cache.set_value(render_cache_mb as f64);
        }
        for window in windows {
            window.imp().setting_controls_sync.set(false);
        }
        self.share_cache_budgets();
    }

    // The configured cache sizes are application totals, not per-document ones. Four tabs must not
    // claim four budgets. Run this whenever the document count or the setting changes.
    pub(crate) fn share_cache_budgets(&self) {
        self.share_cache_budgets_between(self.application_documents());
    }

    fn share_cache_budgets_between(&self, documents: Vec<DocumentView>) {
        let count = documents.len().max(1);

        let render_bytes = self.render_cache_mb.get() * 1024 * 1024 / count;
        // A document with no preview budget re-renders the same preview forever.
        let preview_pages = (self.preview_cache_pages.get() / count).max(1);

        for document in documents {
            document.state().set_render_cache_bytes(render_bytes);
            document.state().set_preview_cache_pages(preview_pages);
        }
    }

    fn apply_animate_scroll(&self, enabled: bool) {
        if self.animate_scroll_sync.replace(true) {
            return;
        }

        let windows = self.application_windows();
        for window in &windows {
            let imp = window.imp();
            imp.animate_scroll_sync.set(true);
            imp.animate_scroll.set(enabled);
        }
        for document in self.application_documents() {
            if document.state().animate_scroll() != enabled {
                document.state().set_animate_scroll(enabled);
            }
        }
        for window in windows {
            window.imp().animate_scroll_sync.set(false);
        }

        let mut config = crate::config::load_config();
        config.animate_scroll = enabled;
        if let Err(e) = crate::config::save_config(&config) {
            eprintln!("Error saving config: {e}");
        }
    }

    #[template_callback]
    fn zoom_in(&self) {
        if let Some(document) = self.active_document() {
            document.zoom_in();
        }
    }

    #[template_callback]
    fn zoom_out(&self) {
        if let Some(document) = self.active_document() {
            document.zoom_out();
        }
    }

    #[template_callback]
    fn jump_back(&self) {
        if let Some(document) = self.active_document() {
            document.jump_back();
        }
    }

    #[template_callback]
    fn jump_forward(&self) {
        if let Some(document) = self.active_document() {
            document.jump_forward();
        }
    }

    #[template_callback]
    fn handle_page_number_entered(&self, entry: &gtk::Entry) {
        let Ok(page_num) = entry.text().parse::<u32>() else {
            return;
        };
        let Some(document) = self.active_document() else {
            return;
        };

        document.goto_page(page_num);
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
        let Some(document) = self.active_document() else {
            return;
        };

        document.apply_zoom_percent(percent);
    }

    #[template_callback]
    fn handle_zoom_entry_icon(&self, _: gtk::EntryIconPosition, entry: &gtk::Entry) {
        self.handle_zoom_entry(entry);
    }

    #[template_callback]
    fn menu_search(&self, btn: &Button) {
        dismiss_menu(btn);
        if let Some(document) = self.active_document() {
            document.open_search();
        }
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
    fn document_title(&self, uri: &str) -> String {
        display_name(uri)
    }

    #[allow(clippy::unused_self)]
    #[template_callback]
    fn document_tooltip(&self, uri: &str) -> String {
        document_path(uri)
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
        self.active_document()
            .and_then(|document| document.target_page(page_num))
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

// Document name for the tab and the header title. A prompt until the reader opens something.
fn display_name(uri: &str) -> String {
    if uri.is_empty() {
        return "Open a Document".to_string();
    }

    gtk::gio::File::for_uri(uri).basename().map_or_else(
        || uri.to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

// Window title, for the task bar and the window switcher.
fn window_title(uri: &str) -> String {
    if uri.is_empty() {
        return "Scrolex PDF Viewer".to_string();
    }

    format!("{} — Scrolex", display_name(uri))
}

// Tab tooltip: the local path where there is one, the URI otherwise.
fn document_path(uri: &str) -> String {
    if uri.is_empty() {
        return "No document".to_string();
    }

    gtk::gio::File::for_uri(uri).path().map_or_else(
        || uri.to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

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
    use crate::document_view::DocumentView;
    use crate::page::PageNumber;
    use crate::test_support::{
        fixture, init, loaded_window, portal_warning_count, wait_until, window as test_window,
    };
    use gtk::prelude::*;
    use gtk::subclass::prelude::{ObjectSubclassExt, ObjectSubclassIsExt};

    fn set_document_page(document: &DocumentView, count: u32, selected: u32) {
        let selection = document.property::<gtk::SingleSelection>("selection");
        let model = selection
            .model()
            .and_downcast::<gtk::gio::ListStore>()
            .expect("document model");
        model.remove_all();
        for page in 0..count {
            model.append(&PageNumber::new(page as i32));
        }
        document.state().set_n_pages(count as i32);
        selection.set_selected(selected);
    }

    #[gtk::test]
    fn header_follows_the_active_document() {
        let window = loaded_window();
        let first = window.header().active_document().expect("first document");
        let second = window.header().add_document().expect("a tab");
        set_document_page(&second, 10, 9);
        second.state().set_zoom(2.0);
        second.state().set_crop(true);
        second.state().set_fit_height(true);
        second.state().set_prev_page(4);

        wait_until(|| window.header().entry_page_num.text() == "10");

        assert_eq!(window.header().entry_zoom.text(), "200");
        assert!(window.header().btn_crop.is_active());
        assert!(window.header().btn_fit_height.is_active());
        assert!(
            !window.header().page_jump_enabled("10", 9),
            "the current page does not enable the jump icon"
        );

        window.header().notebook.set_current_page(Some(0));
        wait_until(|| window.header().entry_page_num.text() == "1");
        assert_eq!(
            window.header().active_document().as_ref(),
            Some(&first),
            "the notebook page drives the header"
        );
        assert_eq!(window.header().entry_zoom.text(), "100");
        assert!(!window.header().btn_crop.is_active());
        assert!(!window.header().btn_fit_height.is_active());
        window.close();
    }

    #[gtk::test]
    fn the_plus_button_opens_a_second_document() {
        let window = loaded_window();
        let notebook = window.header().notebook.get();

        window.header().open_in_new_tab(&fixture("no_outline.pdf"));

        assert_eq!(notebook.n_pages(), 2, "a loaded document keeps its own tab");
        let second = window.header().active_document().expect("the new document");
        wait_until(|| second.state().n_pages() > 0);
        assert_eq!(tab_title(&notebook, 0), "outline.pdf");
        assert_eq!(tab_title(&notebook, 1), "no_outline.pdf");

        window.close();
    }

    #[gtk::test]
    fn the_header_shows_the_active_document_name() {
        let window = loaded_window();
        let header = window.header();
        wait_until(|| header.label_title.label() == "outline.pdf");
        assert_eq!(window.title(), "outline.pdf — Scrolex");

        header.open_in_new_tab(&fixture("no_outline.pdf"));
        wait_until(|| header.label_title.label() == "no_outline.pdf");
        assert_eq!(window.title(), "no_outline.pdf — Scrolex");

        header.notebook.set_current_page(Some(0));
        wait_until(|| header.label_title.label() == "outline.pdf");
        assert_eq!(window.title(), "outline.pdf — Scrolex");

        window.close();
    }

    // The name the reader reads on the tab, so a broken binding cannot pass.
    fn tab_title(notebook: &gtk::Notebook, page: u32) -> String {
        let child = notebook.nth_page(Some(page)).expect("a page");
        notebook
            .tab_label(&child)
            .and_downcast::<gtk::Box>()
            .and_then(|label| label.first_child())
            .and_downcast::<gtk::Label>()
            .expect("the tab name")
            .label()
            .to_string()
    }

    #[gtk::test]
    fn an_empty_document_loads_in_place() {
        let window = test_window();
        let notebook = window.header().notebook.get();
        let empty = window.header().active_document().expect("the empty view");

        window.header().open_in_new_tab(&fixture("outline.pdf"));

        assert_eq!(notebook.n_pages(), 1, "the empty view takes the document");
        wait_until(|| empty.state().n_pages() > 0);
        assert_eq!(tab_title(&notebook, 0), "outline.pdf");

        window.close();
    }

    #[gtk::test]
    fn a_pending_load_keeps_its_tab() {
        let window = test_window();
        let first = window.header().active_document().expect("the empty view");
        first.state().load(&fixture("outline.pdf"));
        assert!(first.is_loading());

        window.header().open_in_new_tab(&fixture("no_outline.pdf"));

        assert_eq!(window.header().notebook.n_pages(), 2);
        assert!(
            first.is_loading(),
            "the second load did not cancel the first"
        );
        window.close();
    }

    #[gtk::test]
    fn closing_the_active_tab_falls_back_to_its_neighbour() {
        let window = loaded_window();
        let notebook = window.header().notebook.get();
        let first = window.header().active_document().expect("first document");
        let second = window.header().add_document().expect("a tab");

        window.header().close_document(&second);

        assert_eq!(notebook.n_pages(), 1);
        assert!(!notebook.shows_tabs(), "one document hides the tab bar");
        assert_eq!(
            window.header().active_document().as_ref(),
            Some(&first),
            "the header follows the remaining document"
        );

        window.close();
    }

    #[gtk::test]
    fn closing_an_inactive_tab_keeps_the_active_one() {
        let window = loaded_window();
        let first = window.header().active_document().expect("first document");
        window.header().add_document().expect("a tab");
        let third = window.header().add_document().expect("a tab");

        window.header().close_document(&first);

        assert_eq!(window.header().notebook.n_pages(), 2);
        assert_eq!(
            window.header().active_document().as_ref(),
            Some(&third),
            "closing another tab does not move the reader"
        );

        window.close();
    }

    #[gtk::test]
    fn the_last_document_does_not_close() {
        let window = loaded_window();
        let only = window.header().active_document().expect("a document");

        window.header().close_document(&only);

        assert_eq!(window.header().notebook.n_pages(), 1);
        assert_eq!(window.header().active_document().as_ref(), Some(&only));

        window.close();
    }

    #[gtk::test]
    fn closing_a_tab_saves_its_state() {
        let window = loaded_window();
        let second = window.header().add_document().expect("a tab");
        second.state().load(&fixture("no_outline.pdf"));
        wait_until(|| second.state().n_pages() > 0);
        second.state().set_crop(true);

        window.header().close_document(&second);

        let reopened = window.header().add_document().expect("a tab");
        reopened.state().load(&fixture("no_outline.pdf"));
        wait_until(|| reopened.state().n_pages() > 0);
        assert!(reopened.state().crop(), "the closed tab saved its state");

        window.close();
    }

    #[gtk::test]
    fn open_documents_share_the_configured_cache() {
        let window = loaded_window();
        let first = window.header().active_document().expect("first document");
        let total_mb = 96;
        let total_previews = crate::config::load_config().preview_cache_pages;
        window.header().spin_cache.set_value(total_mb as f64);

        let budgets = |document: &DocumentView| {
            (
                document.state().render_cache().borrow().budget_bytes(),
                document.state().preview_cache().borrow().budget_bytes(),
            )
        };
        let whole = (
            total_mb * 1024 * 1024,
            crate::state::preview_cache_budget(total_previews),
        );
        assert_eq!(budgets(&first), whole, "one document holds the whole cache");

        let second = window.header().add_document().expect("a tab");
        let half = (
            whole.0 / 2,
            crate::state::preview_cache_budget(total_previews / 2),
        );
        assert_eq!(budgets(&first), half, "two documents halve it");
        assert_eq!(budgets(&second), half);

        window.header().close_document(&second);
        assert_eq!(budgets(&first), whole, "closing gives the share back");

        window.close();
    }

    #[gtk::test]
    fn ctrl_w_closes_the_active_tab() {
        let window = loaded_window();
        let first = window.header().active_document().expect("first document");
        window.header().add_document().expect("a tab");
        assert_eq!(window.header().notebook.n_pages(), 2);

        let taken = window
            .header()
            .handle_window_key(gtk::gdk::Key::w, gtk::gdk::ModifierType::CONTROL_MASK);

        assert_eq!(taken, gtk::glib::Propagation::Stop, "the window took it");
        assert_eq!(window.header().notebook.n_pages(), 1);
        assert_eq!(window.header().active_document().as_ref(), Some(&first));

        window.close();
    }

    #[gtk::test]
    fn ctrl_w_on_the_last_document_closes_the_window() {
        let window = loaded_window();
        assert_eq!(window.header().notebook.n_pages(), 1);
        assert!(window.header().obj().is_visible());

        window
            .header()
            .handle_window_key(gtk::gdk::Key::w, gtk::gdk::ModifierType::CONTROL_MASK);

        assert!(!window.header().obj().is_visible(), "the window closed");
    }

    #[gtk::test]
    fn ctrl_w_closes_a_window_with_nothing_open() {
        let window = test_window();
        window.present();
        wait_until(|| window.header().obj().is_visible());
        assert_eq!(window.state().n_pages(), 0, "the empty view");

        window
            .header()
            .handle_window_key(gtk::gdk::Key::w, gtk::gdk::ModifierType::CONTROL_MASK);

        assert!(!window.header().obj().is_visible(), "the window closed");
    }

    #[gtk::test]
    fn f11_does_not_reach_the_document() {
        let window = loaded_window();
        let header = window.header();

        let propagation =
            header.handle_window_key(gtk::gdk::Key::F11, gtk::gdk::ModifierType::empty());

        assert_eq!(propagation, gtk::glib::Propagation::Stop);
        window.close();
    }

    #[gtk::test]
    fn escape_reaches_the_document_when_not_fullscreen() {
        let window = loaded_window();
        let header = window.header();
        assert!(!header.obj().is_fullscreen());

        let propagation =
            header.handle_window_key(gtk::gdk::Key::Escape, gtk::gdk::ModifierType::empty());

        assert_eq!(propagation, gtk::glib::Propagation::Proceed);
        window.close();
    }

    #[gtk::test]
    fn ctrl_page_keys_switch_tabs_and_wrap() {
        let window = loaded_window();
        let header = window.header();
        header.add_document().expect("a tab");
        header.add_document().expect("a tab");

        let press = |key| header.handle_window_key(key, gtk::gdk::ModifierType::CONTROL_MASK);

        for key in [gtk::gdk::Key::Page_Down, gtk::gdk::Key::KP_Page_Down] {
            header.notebook.set_current_page(Some(2));
            assert_eq!(press(key), gtk::glib::Propagation::Stop);
            assert_eq!(header.notebook.current_page(), Some(0));
        }
        for key in [gtk::gdk::Key::Page_Up, gtk::gdk::Key::KP_Page_Up] {
            header.notebook.set_current_page(Some(0));
            assert_eq!(press(key), gtk::glib::Propagation::Stop);
            assert_eq!(header.notebook.current_page(), Some(2));
        }

        window.close();
    }

    #[gtk::test]
    fn ctrl_tab_walks_the_tabs_both_ways() {
        let window = loaded_window();
        let header = window.header();
        header.add_document().expect("a tab");

        header.notebook.set_current_page(Some(1));
        assert_eq!(
            header.handle_window_key(gtk::gdk::Key::Tab, gtk::gdk::ModifierType::CONTROL_MASK),
            gtk::glib::Propagation::Stop
        );
        assert_eq!(header.notebook.current_page(), Some(0));

        let control_shift =
            gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK;
        for key in [gtk::gdk::Key::ISO_Left_Tab, gtk::gdk::Key::Tab] {
            header.notebook.set_current_page(Some(0));
            assert_eq!(
                header.handle_window_key(key, control_shift),
                gtk::glib::Propagation::Stop
            );
            assert_eq!(header.notebook.current_page(), Some(1));
        }

        window.close();
    }

    #[gtk::test]
    fn a_single_tab_does_not_switch() {
        let window = loaded_window();
        let header = window.header();
        assert_eq!(header.notebook.n_pages(), 1);

        assert_eq!(
            header.handle_window_key(
                gtk::gdk::Key::Page_Down,
                gtk::gdk::ModifierType::CONTROL_MASK,
            ),
            gtk::glib::Propagation::Stop
        );

        assert_eq!(header.notebook.current_page(), Some(0));
        window.close();
    }

    #[gtk::test]
    fn ctrl_shift_w_does_not_close_a_tab() {
        let window = loaded_window();
        window.header().add_document().expect("a tab");

        let taken = window.header().handle_window_key(
            gtk::gdk::Key::w,
            gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK,
        );

        assert_eq!(taken, gtk::glib::Propagation::Proceed);
        assert_eq!(window.header().notebook.n_pages(), 2);
        window.close();
    }

    #[gtk::test]
    fn the_tab_limit_stops_at_max_documents() {
        let window = loaded_window();
        let header = window.header();
        let add_is_sensitive =
            || header.btn_add_tab.is_sensitive() && header.btn_menu_add_tab.is_sensitive();

        while header.notebook.n_pages() < super::MAX_DOCUMENTS {
            assert!(add_is_sensitive(), "room for another tab");
            header.add_document().expect("a tab");
        }

        assert_eq!(header.notebook.n_pages(), super::MAX_DOCUMENTS);
        assert!(!add_is_sensitive(), "the buttons dim");
        assert!(header.add_document().is_none(), "no tab past the limit");
        assert_eq!(header.notebook.n_pages(), super::MAX_DOCUMENTS);

        // the chooser never opens once the limit is reached
        header.handle_window_key(gtk::gdk::Key::t, gtk::gdk::ModifierType::CONTROL_MASK);
        assert_eq!(header.notebook.n_pages(), super::MAX_DOCUMENTS);

        let last = header.active_document().expect("a document");
        header.close_document(&last);
        assert!(add_is_sensitive(), "closing frees a slot");

        window.close();
    }

    #[gtk::test]
    fn one_document_hides_the_tab_bar() {
        let window = loaded_window();
        let notebook = window.header().notebook.get();

        assert_eq!(notebook.n_pages(), 1);
        assert!(!notebook.shows_tabs(), "one document needs no tab bar");

        window.header().add_document().expect("a tab");
        assert_eq!(notebook.n_pages(), 2);
        assert!(notebook.shows_tabs(), "two documents show the tab bar");

        window.close();
    }

    #[gtk::test]
    fn add_tab_buttons_show_the_shortcut() {
        let window = loaded_window();
        let header = window.header();

        assert_eq!(
            header.notebook.action_widget(gtk::PackType::End),
            Some(header.btn_add_tab.get().upcast())
        );
        for button in [&header.btn_add_tab, &header.btn_menu_add_tab] {
            assert!(
                button
                    .tooltip_text()
                    .is_some_and(|text| text.contains("Ctrl+T")),
                "the tooltip names the shortcut"
            );
        }

        window.close();
    }

    #[gtk::test]
    fn global_settings_apply_to_every_document() {
        let window = loaded_window();
        let first = window.header().active_document().expect("first document");
        let second = window.header().add_document().expect("a tab");

        window.header().spin_threads.set_value(1.0);
        window.header().spin_cache.set_value(96.0);
        window.header().btn_animate_scroll.set_active(false);

        for document in [&first, &second] {
            assert_eq!(document.state().render_threads(), 1);
            assert!(!document.state().animate_scroll());
        }

        let first_epoch = first.state().doc_epoch();
        let second_epoch = second.state().doc_epoch();
        window.header().obj().apply_dark_mode(false);
        assert!(first.state().doc_epoch() > first_epoch);
        assert!(second.state().doc_epoch() > second_epoch);
        window.close();
    }

    #[gtk::test]
    fn application_windows_keep_settings_and_cache_in_sync() {
        init();
        let warnings = portal_warning_count();
        let application = gtk::Application::builder()
            .application_id("com.andr2i.scrolex.tests")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .register_session(false)
            .build();
        application
            .register(None::<&gtk::gio::Cancellable>)
            .unwrap();

        let first = super::super::Window::new(&application);
        first.present();
        first.imp().spin_threads.set_value(1.0);
        first.imp().spin_cache.set_value(96.0);
        let second = super::super::Window::new(&application);
        second.present();
        wait_until(|| first.is_mapped() && second.is_mapped());

        assert_eq!(second.imp().spin_threads.value(), 1.0);
        assert_eq!(second.imp().spin_cache.value(), 96.0);
        assert_eq!(
            first
                .active_document()
                .state()
                .render_cache()
                .borrow()
                .budget_bytes(),
            48 * 1024 * 1024
        );

        second.imp().spin_cache.set_value(128.0);
        assert_eq!(first.imp().spin_cache.value(), 128.0);

        second.close();
        wait_until(|| !second.is_visible());
        assert_eq!(
            first
                .active_document()
                .state()
                .render_cache()
                .borrow()
                .budget_bytes(),
            128 * 1024 * 1024
        );
        first.close();

        assert!(
            portal_warning_count() - warnings <= 1,
            "GtkApplication emitted the portal warning more than once",
        );
    }

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
