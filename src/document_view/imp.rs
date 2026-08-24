use std::cell::{OnceCell, RefCell};
use std::sync::OnceLock;

use futures::StreamExt;
use glib::clone;
use glib::subclass::InitializingObject;
use gtk::gdk::{Key, ModifierType};
use gtk::glib::subclass::Signal;
use gtk::subclass::prelude::*;
use gtk::{glib, prelude::*, CompositeTemplate, SearchEntry};

use super::ReaderKeyContext;
use crate::document_pane::DocumentPane;

const SEARCH_DEBOUNCE_MS: u64 = 100;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/andr2i/scrolex/document_view.ui")]
pub struct DocumentView {
    document: OnceCell<crate::document::Document>,
    pane: OnceCell<DocumentPane>,
    #[template_child]
    pane_host: TemplateChild<gtk::Box>,
    #[template_child]
    pub search_bar: TemplateChild<gtk::SearchBar>,
    #[template_child]
    search_entry: TemplateChild<gtk::SearchEntry>,
    #[template_child]
    search_status: TemplateChild<gtk::Label>,
    #[template_child]
    pub toc_revealer: TemplateChild<gtk::Revealer>,
    #[template_child]
    toc_list: TemplateChild<gtk::ListBox>,
    #[template_child]
    pub empty_view: TemplateChild<gtk::Box>,
    #[template_child]
    loading_overlay: TemplateChild<gtk::Box>,
    #[template_child]
    pub loading_spinner: TemplateChild<gtk::Spinner>,
    pub toc_pages: RefCell<Vec<Option<i32>>>,
    search_debounce: RefCell<Option<glib::SourceId>>,
}

#[glib::object_subclass]
impl ObjectSubclass for DocumentView {
    const NAME: &'static str = "DocumentView";
    type Type = super::DocumentView;
    type ParentType = gtk::Widget;

    fn class_init(klass: &mut Self::Class) {
        DocumentPane::static_type();
        klass.bind_template();
        klass.bind_template_callbacks();
        klass.bind_template_instance_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl DocumentView {
    pub(crate) fn pane(&self) -> &DocumentPane {
        self.pane.get().expect("a document view has a pane")
    }

    pub(crate) fn document(&self) -> &crate::document::Document {
        self.document.get().expect("a document view has a document")
    }

    fn content(&self) -> Option<gtk::Widget> {
        self.obj().first_child()
    }

    pub(crate) fn load(&self, file: &gtk::gio::File) {
        if self.document().n_pages() > 0 {
            if let Err(err) = self.pane().viewport().save_position() {
                log::warn!("could not save the reading position before load: {err}");
            }
        }
        self.document().load(file);
    }

    fn setup_document(&self) {
        self.document().connect_closure(
            "load-started",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |_: crate::document::Document| imp.on_load_started()
            ),
        );
        self.document().connect_closure(
            "load-failed",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |_: crate::document::Document, message: String| imp.on_load_failed(&message)
            ),
        );
        self.document().connect_closure(
            "before-load",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |_: crate::document::Document| imp.pane().clear_document()
            ),
        );
        self.document().connect_closure(
            "loaded",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |_: crate::document::Document| imp.handle_document_load()
            ),
        );
        self.document()
            .bind_property("n-pages", &*self.empty_view, "visible")
            .transform_to(|_, n_pages: i32| Some(n_pages == 0))
            .sync_create()
            .build();
    }

    fn on_load_started(&self) {
        self.pane().prepare_load();
        self.loading_spinner.start();
        self.loading_overlay.set_visible(true);
    }

    fn hide_loading(&self) {
        self.loading_overlay.set_visible(false);
        self.loading_spinner.stop();
    }

    fn on_load_failed(&self, message: &str) {
        self.hide_loading();
        self.obj()
            .show_error_dialog(&format!("Error loading file: {message}"));
    }

    fn handle_document_load(&self) {
        self.hide_loading();
        self.populate_toc();
        self.pane().finish_document_load();
    }

    fn populate_toc(&self) {
        self.toc_list.remove_all();
        let items = crate::outline::entries(&self.document().uri());
        let mut pages = Vec::with_capacity(items.len());
        for item in &items {
            let label = gtk::Label::new(Some(&item.title));
            label.set_xalign(0.0);
            label.set_wrap(true);
            label.set_margin_start(8 + item.depth as i32 * 16);
            label.set_margin_end(8);
            label.set_margin_top(3);
            label.set_margin_bottom(3);
            if item.page.is_none() {
                label.add_css_class("dim-label");
            }
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&label));
            row.set_activatable(item.page.is_some());
            self.toc_list.append(&row);
            pages.push(item.page);
        }
        self.toc_pages.replace(pages);
        self.obj().notify("has-toc");
        self.toc_revealer.set_reveal_child(false);
    }

    fn setup_toc(&self) {
        self.toc_revealer.connect_reveal_child_notify(clone!(
            #[weak(rename_to = imp)]
            self,
            move |revealer| {
                if revealer.reveals_child() {
                    imp.toc_list.grab_focus();
                } else {
                    imp.pane().focus_reader();
                }
                imp.obj().notify("toc-visible");
            }
        ));

        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, keyval, _, modifier| {
                let closes = keyval == Key::Escape
                    || (keyval == Key::t && !modifier.contains(ModifierType::CONTROL_MASK));
                if closes {
                    imp.toc_revealer.set_reveal_child(false);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        ));
        self.toc_revealer.add_controller(key);

        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |gesture, _, _, _| {
                if imp.toc_revealer.reveals_child() {
                    imp.toc_revealer.set_reveal_child(false);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        ));
        self.pane().page_area().add_controller(click);
    }

    fn setup_search(&self) {
        self.search_bar.connect_entry(&*self.search_entry);
        self.search_bar.connect_search_mode_enabled_notify(clone!(
            #[weak(rename_to = imp)]
            self,
            move |bar| {
                if !bar.is_search_mode() {
                    imp.clear_search();
                }
            }
        ));
    }

    pub(crate) fn handle_search_key(
        &self,
        keyval: Key,
        modifier: ModifierType,
    ) -> glib::Propagation {
        match keyval {
            Key::f if modifier.contains(ModifierType::CONTROL_MASK) => {
                self.open_search();
                glib::Propagation::Stop
            }
            Key::F3 => {
                if modifier.contains(ModifierType::SHIFT_MASK) {
                    self.prev_match();
                } else {
                    self.next_match();
                }
                glib::Propagation::Stop
            }
            Key::Escape if self.search_bar.is_search_mode() => {
                self.search_bar.set_search_mode(false);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    }

    pub(crate) fn open_search(&self) {
        self.search_bar.set_search_mode(true);
        self.search_entry.grab_focus();
        self.search_entry.select_region(0, -1);
        let query = self.search_entry.text().to_string();
        if !query.is_empty() {
            self.run_search(query);
        }
    }

    fn clear_search(&self) {
        if let Some(source) = self.search_debounce.take() {
            source.remove();
        }
        let pages: Vec<i32> = {
            let search = self.document().search();
            let mut search = search.borrow_mut();
            let pages = search.results.keys().copied().collect();
            search.clear();
            pages
        };
        for page in pages {
            self.pane().redraw_page(page);
        }
        self.pane().viewport().set_current_search_result(None);
        self.update_search_status();
        self.pane().focus_reader();
    }

    fn schedule_search(&self, query: String) {
        if let Some(source) = self.search_debounce.take() {
            source.remove();
        }
        if query.is_empty() {
            self.run_search(query);
            return;
        }
        let source = glib::timeout_add_local_once(
            std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS),
            clone!(
                #[weak(rename_to = imp)]
                self,
                move || {
                    imp.search_debounce.replace(None);
                    imp.run_search(query);
                }
            ),
        );
        self.search_debounce.replace(Some(source));
    }

    fn run_search(&self, query: String) {
        let old_pages: Vec<i32> = self
            .document()
            .search()
            .borrow()
            .results
            .keys()
            .copied()
            .collect();
        let (epoch, shared_epoch) = {
            let search = self.document().search();
            let mut search = search.borrow_mut();
            search.query = query.clone();
            search.begin_sweep()
        };
        self.pane().viewport().set_current_search_result(None);
        for page in old_pages {
            self.pane().redraw_page(page);
        }
        self.update_search_status();

        let n_pages = self.document().n_pages();
        if n_pages == 0 || query.is_empty() {
            return;
        }
        let mut rx = crate::search::spawn_search(
            self.document().uri(),
            query,
            n_pages,
            self.pane().selection().selected() as i32,
            epoch,
            shared_epoch,
        );
        glib::spawn_future_local(clone!(
            #[weak(rename_to = imp)]
            self,
            async move {
                while let Some(update) = rx.next().await {
                    let first = {
                        let search = imp.document().search();
                        let mut search = search.borrow_mut();
                        if update.epoch != search.epoch() {
                            continue;
                        }
                        let first = imp.pane().viewport().current_search_result().is_none();
                        search.results.insert(update.page, update.matches);
                        if first {
                            imp.pane()
                                .viewport()
                                .set_current_search_result(Some((update.page, 0)));
                        }
                        first
                    };
                    if first {
                        imp.pane().reveal_current();
                    }
                    imp.pane().redraw_page(update.page);
                    imp.update_search_status();
                }
                let search = imp.document().search();
                let search = search.borrow();
                if search.epoch() == epoch && !search.query.is_empty() && search.total() == 0 {
                    imp.search_status.set_text("No results");
                }
            }
        ));
    }

    fn move_match(&self, forward: bool) {
        let (previous, selected) = {
            let search = self.document().search();
            let search = search.borrow();
            let previous = self.pane().viewport().current_search_result();
            let Some(selected) = search.step(previous, forward) else {
                return;
            };
            (previous, selected)
        };
        self.pane()
            .viewport()
            .set_current_search_result(Some(selected));
        if let Some((page, _)) = previous {
            self.pane().redraw_page(page);
        }
        self.pane().reveal_current();
        self.pane().redraw_page(selected.0);
        self.update_search_status();
    }

    fn next_match(&self) {
        self.move_match(true);
    }

    fn prev_match(&self) {
        self.move_match(false);
    }

    fn update_search_status(&self) {
        let search = self.document().search();
        let search = search.borrow();
        let text = if search.query.is_empty() {
            String::new()
        } else if let Some(ordinal) = search.ordinal(self.pane().viewport().current_search_result())
        {
            format!("{ordinal} / {}", search.total())
        } else {
            "Searching…".to_string()
        };
        self.search_status.set_text(&text);
    }

    pub(crate) fn handle_reader_key(
        &self,
        keyval: Key,
        modifier: ModifierType,
        context: ReaderKeyContext,
    ) -> glib::Propagation {
        let control = modifier.contains(ModifierType::CONTROL_MASK);
        match keyval {
            Key::o => run_reader_action(context, true, || {
                self.obj().emit_by_name::<()>("open-requested", &[])
            }),
            Key::t if !control => run_reader_action(context, true, || {
                if !self.toc_pages.borrow().is_empty() {
                    self.toc_revealer
                        .set_reveal_child(!self.toc_revealer.reveals_child());
                }
            }),
            Key::f => run_reader_action(context, true, || self.open_search()),
            Key::n if self.document().search().borrow().total() > 0 => {
                run_reader_action(context, true, || self.next_match())
            }
            Key::N if self.document().search().borrow().total() > 0 => {
                run_reader_action(context, true, || self.prev_match())
            }
            _ => self.pane().handle_reader_key(keyval, modifier, context),
        }
    }

    #[template_callback]
    fn handle_key_press(
        &self,
        keyval: Key,
        _keycode: u32,
        modifier: ModifierType,
    ) -> glib::Propagation {
        self.handle_reader_key(keyval, modifier, ReaderKeyContext::Document)
    }

    #[template_callback]
    fn toc_row_activated(&self, row: &gtk::ListBoxRow) {
        let index = row.index();
        let page = if index >= 0 {
            self.toc_pages
                .borrow()
                .get(index as usize)
                .copied()
                .flatten()
        } else {
            None
        };
        if let Some(page) = page {
            self.pane().goto_page(page as u32);
        }
        self.toc_revealer.set_reveal_child(false);
    }

    #[template_callback]
    fn open_document(&self) {
        self.obj().emit_by_name::<()>("open-requested", &[]);
    }

    #[template_callback]
    fn search_changed(&self, entry: &SearchEntry) {
        self.schedule_search(entry.text().to_string());
    }

    #[template_callback]
    fn search_activate(&self) {
        self.next_match();
    }

    #[template_callback]
    fn search_stop(&self) {
        self.search_bar.set_search_mode(false);
    }

    #[template_callback]
    fn search_next(&self) {
        self.next_match();
    }

    #[template_callback]
    fn search_prev(&self) {
        self.prev_match();
    }
}

fn run_reader_action(
    context: ReaderKeyContext,
    allowed_in_numeric_entry: bool,
    action: impl FnOnce(),
) -> glib::Propagation {
    if context == ReaderKeyContext::NumericEntry && !allowed_in_numeric_entry {
        return glib::Propagation::Proceed;
    }
    action();
    glib::Propagation::Stop
}

impl ObjectImpl for DocumentView {
    fn constructed(&self) {
        self.parent_constructed();

        let document = crate::document::Document::new();
        let pane = DocumentPane::new(&document);
        pane.set_hexpand(true);
        pane.set_vexpand(true);
        self.pane_host.append(&pane);
        self.document
            .set(document)
            .expect("one document per document view");
        self.pane.set(pane).expect("one pane per document view");
        self.setup_document();
        self.setup_search();
        self.setup_toc();
    }

    fn dispose(&self) {
        if let Some(source) = self.search_debounce.take() {
            source.remove();
        }
        self.document().search().borrow_mut().clear();
        self.obj().release_renders();
        if let Some(content) = self.content() {
            content.unparent();
        }
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: OnceLock<Vec<glib::ParamSpec>> = OnceLock::new();
        PROPERTIES.get_or_init(|| {
            vec![
                glib::ParamSpecObject::builder::<crate::document::Document>("document")
                    .read_only()
                    .build(),
                glib::ParamSpecObject::builder::<crate::viewport::Viewport>("viewport")
                    .read_only()
                    .build(),
                glib::ParamSpecObject::builder::<gtk::SingleSelection>("selection")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("toc-visible").build(),
                glib::ParamSpecBoolean::builder("has-toc")
                    .read_only()
                    .build(),
            ]
        })
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "document" => self.document.get().to_value(),
            "viewport" => self.pane.get().map(|pane| pane.viewport()).to_value(),
            "selection" => self.pane.get().map(DocumentPane::selection).to_value(),
            "toc-visible" => self
                .toc_revealer
                .try_get()
                .is_some_and(|revealer: gtk::Revealer| revealer.reveals_child())
                .to_value(),
            "has-toc" => (!self.toc_pages.borrow().is_empty()).to_value(),
            name => unimplemented!("unknown property {name}"),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "toc-visible" => self.toc_revealer.set_reveal_child(value.get().unwrap()),
            name => unimplemented!("unknown property {name}"),
        }
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| vec![Signal::builder("open-requested").build()])
    }
}

impl WidgetImpl for DocumentView {
    fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
        self.content()
            .map_or((0, 0, -1, -1), |child| child.measure(orientation, for_size))
    }

    fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
        if let Some(content) = self.content() {
            content.allocate(width, height, baseline, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{loaded_window, wait_until, window};
    use gtk::prelude::*;
    use gtk::subclass::prelude::ObjectSubclassIsExt;

    #[gtk::test]
    fn workspace_and_pane_share_the_document() {
        let window = window();

        assert_eq!(window.document(), window.pane().document());
        assert_eq!(window.viewport(), window.pane().viewport());
        assert_eq!(window.selection(), window.pane().selection());
    }

    #[gtk::test]
    fn ctrl_t_leaves_the_contents_panel_to_the_tab_key() {
        let window = loaded_window();
        let imp = window.imp();
        wait_until(|| !imp.toc_pages.borrow().is_empty());

        window.handle_reader_key(
            gtk::gdk::Key::t,
            gtk::gdk::ModifierType::CONTROL_MASK,
            super::ReaderKeyContext::Document,
        );
        assert!(!imp.toc_revealer.reveals_child(), "Ctrl+T opens a tab");

        window.handle_reader_key(
            gtk::gdk::Key::t,
            gtk::gdk::ModifierType::empty(),
            super::ReaderKeyContext::Document,
        );
        assert!(imp.toc_revealer.reveals_child(), "plain t still toggles");

        window.close();
    }

    #[gtk::test]
    fn empty_view_follows_the_page_count() {
        let window = window();
        let imp = window.imp();

        assert!(imp.empty_view.property::<bool>("visible"));
        imp.document().set_n_pages(1);
        assert!(!imp.empty_view.property::<bool>("visible"));
        imp.document().set_n_pages(0);
        assert!(imp.empty_view.property::<bool>("visible"));
    }
}
