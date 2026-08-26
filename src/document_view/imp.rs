use std::cell::{Cell, OnceCell, RefCell};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::StreamExt;
use glib::clone;
use glib::subclass::InitializingObject;
use gtk::gdk::{Key, ModifierType};
use gtk::glib::subclass::Signal;
use gtk::subclass::prelude::*;
use gtk::{glib, prelude::*, CompositeTemplate, SearchEntry};

use super::ReaderKeyContext;
use crate::document_pane::DocumentPane;
use crate::links::{DocumentLocation, LinkAction, LinkRequest};

const SEARCH_DEBOUNCE_MS: u64 = 100;
const SPLIT_GEOMETRY_TIMEOUT: Duration = Duration::from_secs(2);

struct SplitTargetSize {
    viewport: f64,
    pane: f64,
}

struct SourcePosition {
    page: i32,
    vertical: f64,
}

#[derive(Clone, Copy)]
enum SplitTarget {
    Page(i32),
    Location(DocumentLocation),
}

impl SplitTarget {
    fn page(self) -> i32 {
        match self {
            Self::Page(page) => page,
            Self::Location(location) => location.page,
        }
    }

    fn navigate(self, pane: &DocumentPane) {
        match self {
            Self::Page(page) => pane.goto_page(page.saturating_add(1) as u32),
            Self::Location(location) => pane.navigate_to_location(location),
        }
    }
}

#[derive(CompositeTemplate, Default)]
#[template(resource = "/com/andr2i/scrolex/document_view.ui")]
pub struct DocumentView {
    document: OnceCell<crate::document::Document>,
    primary: RefCell<Option<DocumentPane>>,
    secondary: RefCell<Option<DocumentPane>>,
    active_pane: RefCell<Option<DocumentPane>>,
    paned: RefCell<Option<gtk::Paned>>,
    split_generation: Cell<u64>,
    split_geometry_pending: Cell<bool>,
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
    pub(crate) fn active_pane(&self) -> DocumentPane {
        self.active_pane
            .borrow()
            .as_ref()
            .expect("a document view has an active pane")
            .clone()
    }

    pub(crate) fn primary_pane(&self) -> DocumentPane {
        self.primary
            .borrow()
            .as_ref()
            .expect("a document view has a primary pane")
            .clone()
    }

    fn split_container(&self) -> gtk::Paned {
        self.paned
            .borrow()
            .as_ref()
            .expect("a document view has a split container")
            .clone()
    }

    pub(crate) fn document(&self) -> &crate::document::Document {
        self.document.get().expect("a document view has a document")
    }

    fn content(&self) -> Option<gtk::Widget> {
        self.obj().first_child()
    }

    pub(crate) fn load(&self, file: &gtk::gio::File) {
        self.split_generation
            .set(self.split_generation.get().wrapping_add(1));
        self.split_geometry_pending.set(false);
        let secondary = self.secondary.borrow().clone();
        if let Some(secondary) = secondary {
            self.close_pane(&secondary);
        }
        if self.document().n_pages() > 0 {
            if let Err(err) = self.primary_pane().viewport().save_position() {
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
                move |_: crate::document::Document| imp.primary_pane().clear_document()
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
        self.primary_pane().prepare_load();
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
        self.primary_pane().finish_document_load();
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
                    imp.active_pane().focus_reader();
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

        let middle_click = gtk::GestureClick::builder()
            .button(gtk::gdk::BUTTON_MIDDLE)
            .build();
        middle_click.connect_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            move |gesture, _, _, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                imp.toc_row_split(y);
            }
        ));
        self.toc_list.add_controller(middle_click);
    }

    pub(crate) fn panes(&self) -> Vec<DocumentPane> {
        let mut panes = vec![self.primary_pane()];
        if let Some(pane) = self.secondary.borrow().clone() {
            panes.push(pane);
        }
        panes
    }

    fn focused_pane(&self) -> Option<DocumentPane> {
        let window = self.obj().root().and_downcast::<gtk::Window>()?;
        let focus = gtk::prelude::GtkWindowExt::focus(&window)?;
        self.panes()
            .into_iter()
            .find(|pane| focus.is_ancestor(pane) || focus == *pane.upcast_ref::<gtk::Widget>())
    }

    fn switch_pane(&self) -> glib::Propagation {
        if self.split_geometry_pending.get() {
            return glib::Propagation::Proceed;
        }
        let Some(secondary) = self.secondary.borrow().clone() else {
            return glib::Propagation::Proceed;
        };
        let Some(current) = self.focused_pane() else {
            return glib::Propagation::Proceed;
        };
        let target = if current == secondary {
            self.primary_pane()
        } else {
            secondary
        };
        self.activate_pane(&target);
        target.focus_reader();
        glib::Propagation::Stop
    }

    fn setup_pane(&self, pane: &DocumentPane) {
        pane.connect_closure(
            "link-activated",
            false,
            glib::closure_local!(
                #[weak(rename_to = imp)]
                self,
                move |pane: DocumentPane, request: LinkRequest| {
                    imp.handle_link(&pane, request);
                }
            ),
        );

        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter(clone!(
            #[weak(rename_to = imp)]
            self,
            #[weak]
            pane,
            move |_| imp.activate_pane(&pane)
        ));
        pane.page_area().add_controller(focus);

        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        click.connect_pressed(clone!(
            #[weak(rename_to = imp)]
            self,
            #[weak]
            pane,
            move |gesture, _, _, _| {
                imp.activate_pane(&pane);
                if imp.toc_revealer.reveals_child() {
                    imp.toc_revealer.set_reveal_child(false);
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
        ));
        pane.page_area().add_controller(click);

        pane.close_button().connect_clicked(clone!(
            #[weak(rename_to = imp)]
            self,
            #[weak]
            pane,
            move |_| imp.close_pane(&pane)
        ));
    }

    fn activate_pane(&self, pane: &DocumentPane) {
        if self.active_pane.borrow().as_ref() == Some(pane) {
            return;
        }
        for candidate in self.panes() {
            if candidate == *pane {
                candidate.add_css_class("active-pane");
                candidate.remove_css_class("inactive-pane");
            } else {
                candidate.remove_css_class("active-pane");
                candidate.add_css_class("inactive-pane");
            }
        }
        self.active_pane.replace(Some(pane.clone()));
        self.obj().notify("viewport");
        self.obj().notify("selection");
        self.update_search_status();
    }

    fn handle_link(&self, source: &DocumentPane, request: LinkRequest) {
        self.activate_pane(source);
        match request.action {
            LinkAction::Open => source.follow_link(request.source_page, request.location),
            LinkAction::OpenBeside => {
                self.open_beside(source, request.source_page, request.location)
            }
            LinkAction::OpenInNewTab => self
                .obj()
                .emit_by_name::<()>("new-tab-location-requested", &[&request]),
        }
    }

    pub(crate) fn split_here(&self) {
        if self.secondary.borrow().is_some()
            || self.split_geometry_pending.get()
            || self.document().n_pages() == 0
        {
            return;
        }
        let source = self.active_pane();
        let page = source.viewport().page() as i32;
        self.open_split(&source, page, SplitTarget::Page(page), true);
    }

    fn ensure_secondary(&self) -> DocumentPane {
        if let Some(pane) = self.secondary.borrow().clone() {
            return pane;
        }
        let primary = self.primary_pane();
        let pane = DocumentPane::new(self.document());
        pane.set_hexpand(true);
        pane.set_vexpand(true);
        pane.add_css_class("inactive-pane");
        pane.viewport()
            .set_animate_scroll(primary.viewport().animate_scroll());
        self.setup_pane(&pane);
        pane.finish_document_load();
        pane.set_sensitive(!self.split_geometry_pending.get());
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_hexpand(true);
        paned.set_vexpand(true);
        paned.set_wide_handle(true);
        paned.add_css_class("document-split");
        self.paned.replace(Some(paned.clone()));
        self.pane_host.remove(&primary);
        paned.set_start_child(Some(&primary));
        self.pane_host.append(&paned);
        paned.set_end_child(Some(&pane));
        self.secondary.replace(Some(pane.clone()));
        self.obj().notify("split-open");
        primary.set_close_visible(true);
        pane.set_close_visible(true);
        pane
    }

    fn open_beside(&self, source: &DocumentPane, source_page: i32, location: DocumentLocation) {
        if self.secondary.borrow().is_some() && !self.split_geometry_pending.get() {
            let target = if *source == self.primary_pane() {
                self.secondary.borrow().as_ref().unwrap().clone()
            } else {
                self.primary_pane()
            };
            target.viewport().set_crop(source.viewport().crop());
            target.navigate_to_location(location);
            self.activate_pane(source);
            return;
        }
        self.open_split(source, source_page, SplitTarget::Location(location), false);
    }

    fn open_split(
        &self,
        source: &DocumentPane,
        source_page: i32,
        target: SplitTarget,
        focus_target: bool,
    ) {
        let generation = self.split_generation.get().wrapping_add(1);
        self.split_generation.set(generation);
        self.split_geometry_pending.set(true);
        self.resolve_beside_crop(source, source_page, target, focus_target, generation);
    }

    fn resolve_beside_crop(
        &self,
        source: &DocumentPane,
        source_page: i32,
        target: SplitTarget,
        focus_target: bool,
        generation: u64,
    ) {
        if !source.viewport().crop() {
            self.apply_beside(source, source_page, target, focus_target, generation);
            return;
        }

        let cache = self.document().bbox_cache();
        let uri = self.document().uri();
        let mut missing = Vec::new();
        for page in [source_page, target.page()] {
            if missing.iter().any(|(index, _, _)| *index == page)
                || cache.borrow().contains_key(&page)
            {
                continue;
            }
            if let Some(size) = self.document().page_size(page) {
                missing.push((page, size.width, size.height));
            }
        }
        if missing.is_empty() {
            self.apply_beside(source, source_page, target, focus_target, generation);
            return;
        }

        self.loading_spinner.start();
        self.loading_overlay.set_visible(true);
        let (sender, receiver) = oneshot::channel();
        std::thread::spawn(move || {
            let boxes: Vec<_> = missing
                .into_iter()
                .map(|(page, width, height)| {
                    (page, crate::page::crop_box(&uri, page, width, height))
                })
                .collect();
            let _ = sender.send(boxes);
        });

        let source = source.clone();
        glib::spawn_future_local(clone!(
            #[weak(rename_to = imp)]
            self,
            async move {
                let Ok(boxes) = receiver.await else {
                    if imp.split_generation.get() == generation {
                        imp.split_geometry_pending.set(false);
                        imp.hide_loading();
                    }
                    return;
                };
                if imp.split_generation.get() != generation {
                    return;
                }
                imp.document().bbox_cache().borrow_mut().extend(boxes);
                imp.apply_beside(&source, source_page, target, focus_target, generation);
            }
        ));
    }

    fn apply_beside(
        &self,
        source: &DocumentPane,
        source_page: i32,
        split_target: SplitTarget,
        focus_target: bool,
        generation: u64,
    ) {
        if self.split_generation.get() != generation {
            return;
        }
        let source_crop = source.viewport().crop();
        let secondary = self.ensure_secondary();
        let target = if *source == self.primary_pane() {
            secondary
        } else {
            self.primary_pane()
        };
        target.viewport().set_crop(source_crop);
        let paned = self.split_container();
        let total = f64::from(paned.width());
        if total <= 0.0 {
            let source = source.clone();
            paned.add_tick_callback(clone!(
                #[weak(rename_to = imp)]
                self,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move |paned, _| {
                    if paned.width() <= 0 {
                        return glib::ControlFlow::Continue;
                    }
                    imp.apply_beside(&source, source_page, split_target, focus_target, generation);
                    glib::ControlFlow::Break
                }
            ));
            return;
        }
        let source_width = source.paper_width(source_page).unwrap_or(1.0);
        let target_page = split_target.page();
        let target_width = target.paper_width(target_page).unwrap_or(1.0);
        let source_chrome = source.horizontal_chrome(source_page).unwrap_or_default();
        let target_chrome = target
            .horizontal_chrome(target_page)
            .unwrap_or(source_chrome);
        let divider = (total
            - f64::from(self.primary_pane().width())
            - self
                .secondary
                .borrow()
                .as_ref()
                .map_or(0.0, |pane| f64::from(pane.width())))
        .max(0.0);
        let gaps = source_chrome.total() + target_chrome.total() + divider;
        let zoom = split_zoom(
            total,
            source_width,
            target_width,
            gaps,
            source.viewport().zoom(),
        );
        let source_vertical = source.vertical_position();
        source.apply_split_zoom(zoom);
        target.apply_split_zoom(zoom);
        split_target.navigate(&target);

        let target_viewport = target_width * zoom + target_chrome.row;
        let target_display = target_viewport + target_chrome.pane;
        let position = if target == self.primary_pane() {
            target_display
        } else {
            total - divider - target_display
        };
        self.split_container().set_position(position.round() as i32);
        if focus_target {
            self.activate_pane(&target);
        } else {
            self.activate_pane(source);
        }
        self.correct_split_once(
            source,
            target,
            SourcePosition {
                page: source_page,
                vertical: source_vertical,
            },
            SplitTargetSize {
                viewport: target_viewport,
                pane: target_display,
            },
            focus_target,
            generation,
        );
    }

    fn correct_split_once(
        &self,
        source: &DocumentPane,
        target: DocumentPane,
        source_position: SourcePosition,
        target_size: SplitTargetSize,
        focus_target: bool,
        generation: u64,
    ) {
        let paned = self.split_container();
        let source = source.clone();
        let primary = self.primary_pane();
        let deadline = Instant::now() + SPLIT_GEOMETRY_TIMEOUT;
        paned.add_tick_callback(clone!(
            #[weak(rename_to = imp)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move |paned, _| {
                if imp.split_generation.get() != generation {
                    return glib::ControlFlow::Break;
                }
                if Instant::now() >= deadline {
                    source.restore_vertical_position(source_position.vertical);
                    source.reveal_page_horizontally(source_position.page);
                    target.set_sensitive(true);
                    if focus_target {
                        target.focus_reader();
                    }
                    imp.hide_loading();
                    imp.split_geometry_pending.set(false);
                    return glib::ControlFlow::Break;
                }
                if (f64::from(target.width()) - target_size.pane).abs() > 1.0 {
                    return glib::ControlFlow::Continue;
                }
                let allocations_ready = imp.panes().into_iter().all(|pane| {
                    let viewport = pane.viewport_width();
                    viewport > 0.0 && viewport <= f64::from(pane.width()) + 1.0
                });
                if !allocations_ready {
                    return glib::ControlFlow::Continue;
                }
                let error = target.viewport_width() - target_size.viewport;
                if error.abs() > 1.0 {
                    let direction = if target == primary { -1.0 } else { 1.0 };
                    paned.set_position(
                        (f64::from(paned.position()) + error * direction).round() as i32
                    );
                }
                source.restore_vertical_position(source_position.vertical);
                source.reveal_page_horizontally(source_position.page);
                target.set_sensitive(true);
                if focus_target {
                    target.focus_reader();
                }
                imp.hide_loading();
                imp.split_geometry_pending.set(false);
                glib::ControlFlow::Break
            }
        ));
    }

    pub(crate) fn close_active_pane(&self) -> bool {
        if self.secondary.borrow().is_none() {
            return false;
        }
        self.close_pane(&self.active_pane());
        true
    }

    fn close_pane(&self, pane: &DocumentPane) {
        if self.secondary.borrow().is_none() {
            return;
        }
        if self.split_geometry_pending.get() {
            self.hide_loading();
        }
        self.split_generation
            .set(self.split_generation.get().wrapping_add(1));
        self.split_geometry_pending.set(false);
        let active_closed = self.active_pane.borrow().as_ref() == Some(pane);
        pane.release_renders();
        let paned = self.split_container();
        let remaining;
        if *pane == self.primary_pane() {
            remaining = self.secondary.take().expect("a remaining pane");
            self.primary.replace(Some(remaining.clone()));
        } else {
            remaining = self.primary_pane();
            self.secondary.take();
        }
        self.obj().notify("split-open");
        paned.set_start_child(gtk::Widget::NONE);
        paned.set_end_child(gtk::Widget::NONE);
        self.pane_host.remove(&paned);
        self.paned.take();
        self.pane_host.append(&remaining);
        remaining.set_close_visible(false);
        remaining.set_sensitive(true);
        self.activate_pane(&self.primary_pane());
        if active_closed {
            self.primary_pane().focus_reader();
        }
    }

    pub(crate) fn redraw_pages(&self) {
        for pane in self.panes() {
            pane.redraw_pages();
        }
    }

    fn redraw_page(&self, page: i32) {
        for pane in self.panes() {
            pane.redraw_page(page);
        }
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
            self.redraw_page(page);
        }
        for pane in self.panes() {
            pane.viewport().set_current_search_result(None);
        }
        self.update_search_status();
        self.active_pane().focus_reader();
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
        for pane in self.panes() {
            pane.viewport().set_current_search_result(None);
        }
        for page in old_pages {
            self.redraw_page(page);
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
            self.active_pane().selection().selected() as i32,
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
                        let first = imp
                            .active_pane()
                            .viewport()
                            .current_search_result()
                            .is_none();
                        search.results.insert(update.page, update.matches);
                        if first {
                            imp.active_pane()
                                .viewport()
                                .set_current_search_result(Some((update.page, 0)));
                        }
                        first
                    };
                    if first {
                        imp.active_pane().reveal_current();
                    }
                    imp.redraw_page(update.page);
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
            let previous = self.active_pane().viewport().current_search_result();
            let Some(selected) = search.step(previous, forward) else {
                return;
            };
            (previous, selected)
        };
        self.active_pane()
            .viewport()
            .set_current_search_result(Some(selected));
        if let Some((page, _)) = previous {
            self.redraw_page(page);
        }
        self.active_pane().reveal_current();
        self.redraw_page(selected.0);
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
        } else if let Some(ordinal) =
            search.ordinal(self.active_pane().viewport().current_search_result())
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
            Key::s => run_reader_action(context, true, || self.split_here()),
            Key::n if self.document().search().borrow().total() > 0 => {
                run_reader_action(context, true, || self.next_match())
            }
            Key::N if self.document().search().borrow().total() > 0 => {
                run_reader_action(context, true, || self.prev_match())
            }
            _ => self
                .active_pane()
                .handle_reader_key(keyval, modifier, context),
        }
    }

    #[template_callback]
    fn handle_key_press(
        &self,
        keyval: Key,
        _keycode: u32,
        modifier: ModifierType,
    ) -> glib::Propagation {
        if !modifier.contains(ModifierType::CONTROL_MASK)
            && matches!(keyval, Key::Tab | Key::ISO_Left_Tab)
        {
            let taken = self.switch_pane();
            if matches!(taken, glib::Propagation::Stop) {
                return taken;
            }
        }
        self.handle_reader_key(keyval, modifier, ReaderKeyContext::Document)
    }

    #[template_callback]
    fn toc_row_activated(&self, row: &gtk::ListBoxRow) {
        if let Some(page) = self.toc_page(row) {
            self.active_pane().goto_page(page as u32);
        }
        self.toc_revealer.set_reveal_child(false);
    }

    fn toc_page(&self, row: &gtk::ListBoxRow) -> Option<i32> {
        let index = usize::try_from(row.index()).ok()?;
        self.toc_pages.borrow().get(index).copied().flatten()
    }

    fn toc_row_split(&self, y: f64) {
        let Some(page) = self
            .toc_list
            .row_at_y(y as i32)
            .and_then(|row| self.toc_page(&row))
        else {
            return;
        };
        let source = self.active_pane();
        let source_page = source.viewport().page() as i32;
        let location = DocumentLocation {
            page: page - 1,
            x: None,
            y: None,
        };
        self.open_beside(&source, source_page, location);
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

fn split_zoom(
    viewport: f64,
    source_points: f64,
    target_points: f64,
    gaps: f64,
    source_zoom: f64,
) -> f64 {
    crate::document_pane::fit_width_zoom(viewport, source_points + target_points, gaps)
        .map_or(source_zoom, |fit| source_zoom.min(fit))
}

impl ObjectImpl for DocumentView {
    fn constructed(&self) {
        self.parent_constructed();

        let document = crate::document::Document::new();
        let pane = DocumentPane::new(&document);
        pane.set_hexpand(true);
        pane.set_vexpand(true);
        pane.add_css_class("active-pane");
        self.pane_host.append(&pane);
        self.document
            .set(document)
            .expect("one document per document view");
        self.primary.replace(Some(pane.clone()));
        self.active_pane.replace(Some(pane.clone()));
        self.setup_pane(&pane);
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
                glib::ParamSpecBoolean::builder("split-open")
                    .read_only()
                    .build(),
            ]
        })
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "document" => self.document.get().to_value(),
            "viewport" => self
                .active_pane
                .borrow()
                .as_ref()
                .map(|pane| pane.viewport())
                .to_value(),
            "selection" => self
                .active_pane
                .borrow()
                .as_ref()
                .map(DocumentPane::selection)
                .to_value(),
            "toc-visible" => self
                .toc_revealer
                .try_get()
                .is_some_and(|revealer: gtk::Revealer| revealer.reveals_child())
                .to_value(),
            "has-toc" => (!self.toc_pages.borrow().is_empty()).to_value(),
            "split-open" => self.secondary.borrow().is_some().to_value(),
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
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("open-requested").build(),
                Signal::builder("new-tab-location-requested")
                    .param_types([LinkRequest::static_type()])
                    .build(),
            ]
        })
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
    use super::split_zoom;
    use crate::document_view::ReaderKeyContext;
    use crate::links::{DocumentLocation, LinkAction, LinkRequest};
    use crate::test_support::{loaded_window, wait_until, window};
    use gtk::prelude::*;
    use gtk::subclass::prelude::ObjectSubclassIsExt;

    #[test]
    fn split_zoom_fits_both_page_widths_and_gaps() {
        assert_eq!(split_zoom(1_000.0, 400.0, 400.0, 20.0, 2.0), 1.225);
        assert_eq!(split_zoom(1_000.0, 300.0, 500.0, 200.0, 2.0), 1.0);
        assert_eq!(split_zoom(1_000.0, 300.0, 500.0, 20.0, 0.75), 0.75);
    }

    #[gtk::test]
    fn normal_link_click_updates_back_and_forward_history() {
        let window = loaded_window();
        let imp = window.imp();
        let pane = imp.primary_pane();

        imp.handle_link(
            &pane,
            LinkRequest {
                source_page: 0,
                location: DocumentLocation {
                    page: 1,
                    x: Some(0.0),
                    y: Some(0.0),
                },
                action: LinkAction::Open,
            },
        );
        wait_until(|| pane.viewport().page() == 1);
        assert_eq!(pane.viewport().prev_page(), 1);

        pane.jump_back();
        wait_until(|| pane.viewport().page() == 0);
        assert_eq!(pane.viewport().next_page(), 2);

        pane.jump_forward();
        wait_until(|| pane.viewport().page() == 1);
        window.close();
    }

    #[gtk::test]
    fn beside_action_keeps_the_source_active_and_closes_cleanly() {
        let window = loaded_window();
        let imp = window.imp();
        let source = imp.primary_pane();
        let source_vertical = source.vertical_position();
        imp.open_beside(
            &source,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        wait_until(|| {
            imp.secondary
                .borrow()
                .as_ref()
                .is_some_and(|pane| pane.viewport().page() == 1 && pane.viewport_width() > 0.0)
        });
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        let paned = imp.split_container();
        assert!(paned.has_css_class("document-split"));
        // the gutter is the separator min-width in ui/style.css
        assert_eq!(paned.width() - source.width() - secondary.width(), 8);
        assert_eq!(imp.active_pane(), source);
        assert!(source.has_css_class("active-pane"));
        assert!(secondary.has_css_class("inactive-pane"));
        assert_eq!(source.viewport().zoom(), secondary.viewport().zoom());
        assert!((source.vertical_position() - source_vertical).abs() <= 1.0);

        imp.open_beside(
            &secondary,
            1,
            DocumentLocation {
                page: 2,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        wait_until(|| imp.primary_pane().viewport().page() == 2);
        assert_eq!(imp.active_pane(), secondary);
        assert!(source.has_css_class("inactive-pane"));
        assert!(secondary.has_css_class("active-pane"));

        imp.close_pane(&secondary);
        assert!(imp.secondary.borrow().is_none());
        assert_eq!(imp.active_pane(), imp.primary_pane());
        assert!(source.has_css_class("active-pane"));
        assert!(!source.has_css_class("inactive-pane"));
        window.close();
    }

    #[gtk::test]
    fn split_here_opens_the_current_page_and_activates_the_target() {
        let window = loaded_window();
        let imp = window.imp();
        let source = imp.primary_pane();
        source.goto_page(2);
        wait_until(|| source.viewport().page() == 1);

        imp.split_here();

        wait_until(|| {
            imp.secondary.borrow().as_ref().is_some_and(|pane| {
                pane.viewport().page() == 1 && !imp.split_geometry_pending.get()
            })
        });
        let target = imp.secondary.borrow().as_ref().unwrap().clone();
        assert_eq!(imp.active_pane(), target);
        assert!(target.has_css_class("active-pane"));
        assert!(source.has_css_class("inactive-pane"));
        assert!(window.property::<bool>("split-open"));

        imp.split_here();
        assert_eq!(imp.secondary.borrow().as_ref(), Some(&target));
        imp.close_pane(&target);
        assert!(!window.property::<bool>("split-open"));
        window.close();
    }

    #[gtk::test]
    fn tab_moves_the_focus_between_the_panes() {
        let window = loaded_window();
        let imp = window.imp();
        let source = imp.primary_pane();
        source.focus_reader();

        let press = |key| imp.handle_key_press(key, 0, gtk::gdk::ModifierType::empty());
        assert_eq!(press(gtk::gdk::Key::Tab), glib::Propagation::Proceed);

        imp.split_here();
        wait_until(|| imp.secondary.borrow().is_some() && !imp.split_geometry_pending.get());
        let target = imp.secondary.borrow().as_ref().unwrap().clone();
        assert_eq!(imp.active_pane(), target);

        assert_eq!(press(gtk::gdk::Key::Tab), glib::Propagation::Stop);
        assert_eq!(imp.active_pane(), source);
        assert!(source.has_css_class("active-pane"));

        assert_eq!(press(gtk::gdk::Key::ISO_Left_Tab), glib::Propagation::Stop);
        assert_eq!(imp.active_pane(), target);

        // The search bar keeps Tab, so the focus can reach its buttons.
        imp.open_search();
        assert!(imp.focused_pane().is_none());
        assert_eq!(press(gtk::gdk::Key::Tab), glib::Propagation::Proceed);
        assert_eq!(imp.active_pane(), target);
        imp.search_bar.set_search_mode(false);
        target.focus_reader();

        assert_eq!(
            imp.handle_key_press(gtk::gdk::Key::Tab, 0, gtk::gdk::ModifierType::CONTROL_MASK,),
            glib::Propagation::Proceed
        );
        assert_eq!(imp.active_pane(), target);
        window.close();
    }

    #[gtk::test]
    fn split_shortcut_is_consumed_when_a_split_exists() {
        let window = loaded_window();

        assert_eq!(
            window.handle_reader_key(
                gtk::gdk::Key::s,
                gtk::gdk::ModifierType::empty(),
                ReaderKeyContext::Document,
            ),
            glib::Propagation::Stop
        );
        wait_until(|| !window.imp().split_geometry_pending.get());
        let target = window.imp().secondary.borrow().as_ref().unwrap().clone();

        assert_eq!(
            window.handle_reader_key(
                gtk::gdk::Key::s,
                gtk::gdk::ModifierType::empty(),
                ReaderKeyContext::Document,
            ),
            glib::Propagation::Stop
        );
        assert_eq!(window.imp().secondary.borrow().as_ref(), Some(&target));
        window.close();
    }

    #[gtk::test]
    fn load_closes_an_open_split_without_a_borrow_panic() {
        let window = loaded_window();
        let imp = window.imp();
        let source = imp.primary_pane();
        imp.open_beside(
            &source,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        secondary.set_sensitive(false);
        imp.split_geometry_pending.set(true);
        let file = gtk::gio::File::for_uri(&window.document().uri());

        imp.load(&file);

        assert!(imp.secondary.borrow().is_none());
        assert!(imp.primary_pane().is_sensitive());
        window.close();
    }

    #[gtk::test]
    fn split_zoom_preserves_the_manual_zoom() {
        let window = loaded_window();
        let imp = window.imp();
        let source = imp.primary_pane();
        source.viewport().zoom_to(4.0);
        imp.open_beside(
            &source,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );

        assert_eq!(source.viewport().manual_zoom(), 4.0);
        window.close();
    }

    #[gtk::test]
    fn clear_search_resets_each_pane_result() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        primary.viewport().set_current_search_result(Some((0, 0)));
        secondary.viewport().set_current_search_result(Some((1, 0)));

        imp.clear_search();

        assert_eq!(primary.viewport().current_search_result(), None);
        assert_eq!(secondary.viewport().current_search_result(), None);
        window.close();
    }

    #[gtk::test]
    fn the_pane_close_button_shows_a_pointer() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();

        for pane in [&primary, &secondary] {
            let cursor = pane.close_button().cursor().expect("a cursor");
            assert_eq!(cursor.name().as_deref(), Some("pointer"));
        }
        window.close();
    }

    #[gtk::test]
    fn closing_the_primary_updates_the_pane_access() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        secondary.set_sensitive(false);
        imp.split_geometry_pending.set(true);

        imp.close_pane(&primary);

        assert_eq!(window.pane(), secondary);
        assert_ne!(window.pane(), primary);
        assert!(window.pane().is_sensitive());
        window.close();
    }

    #[gtk::test]
    fn closing_a_split_releases_the_pane() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        let weak = secondary.downgrade();

        imp.close_pane(&secondary);
        drop(secondary);

        assert!(weak.upgrade().is_none());
        window.close();
    }

    #[gtk::test]
    fn beside_target_uses_the_source_crop_mode() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        primary.viewport().set_crop(true);

        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        wait_until(|| secondary.viewport().page() == 1 && secondary.viewport_width() > 0.0);
        assert!(secondary.viewport().crop());

        primary.viewport().set_crop(false);
        imp.open_beside(
            &secondary,
            1,
            DocumentLocation {
                page: 2,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        wait_until(|| primary.viewport().page() == 2);
        assert!(primary.viewport().crop());

        imp.close_pane(&secondary);
        window.close();
    }

    #[gtk::test]
    fn beside_target_uses_disabled_source_crop_mode() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        primary.viewport().set_crop(true);
        primary.viewport().save_position().unwrap();

        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        wait_until(|| secondary.viewport().page() == 1 && !imp.split_geometry_pending.get());
        assert!(secondary.viewport().crop());
        imp.close_pane(&secondary);

        primary.viewport().set_crop(false);
        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 2,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        wait_until(|| secondary.viewport().page() == 2 && !imp.split_geometry_pending.get());
        assert!(!secondary.viewport().crop());
        imp.close_pane(&secondary);
        window.close();
    }

    #[gtk::test]
    fn closing_the_source_pane_before_the_split_settles_keeps_the_view_usable() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        primary.viewport().set_crop(true);
        window.document().bbox_cache().borrow_mut().clear();

        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 2,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        wait_until(|| imp.secondary.borrow().is_some());
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        assert!(
            imp.split_geometry_pending.get(),
            "the split must still be pending for this test to mean anything"
        );
        assert!(!secondary.is_sensitive());

        imp.close_pane(&primary);

        assert_eq!(window.pane(), secondary);
        assert!(window.pane().is_sensitive());
        assert!(!window.is_loading());
        window.close();
    }

    #[gtk::test]
    fn beside_target_width_uses_resolved_crop_box() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        primary.viewport().set_crop(true);
        window.document().bbox_cache().borrow_mut().clear();

        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 2,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        assert!(window.is_loading());
        assert!(imp.secondary.borrow().is_none());
        wait_until(|| imp.secondary.borrow().is_some());
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        wait_until(|| {
            secondary.viewport().page() == 2
                && !imp.split_geometry_pending.get()
                && secondary.horizontal_chrome(2).is_some()
        });
        assert!(!window.is_loading());
        assert!(secondary.is_sensitive());

        let paper_width = window
            .document()
            .bbox_cache()
            .borrow()
            .get(&2)
            .unwrap()
            .size()
            .0;
        let chrome = secondary.horizontal_chrome(2).unwrap();
        let expected = paper_width * secondary.viewport().zoom() + chrome.row;
        assert!((secondary.viewport_width() - expected).abs() <= 1.0);
        imp.close_pane(&secondary);
        window.close();
    }

    #[gtk::test]
    fn later_beside_actions_keep_the_user_split_geometry() {
        let window = loaded_window();
        let imp = window.imp();
        let primary = imp.primary_pane();
        imp.open_beside(
            &primary,
            0,
            DocumentLocation {
                page: 1,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        wait_until(|| {
            secondary.viewport_width() > 0.0
                && imp.split_container().position() > 0
                && !imp.split_geometry_pending.get()
        });
        let paned = imp.split_container();
        let position = paned.width() / 3;
        paned.set_position(position);
        primary.apply_split_zoom(0.75);
        secondary.apply_split_zoom(1.25);
        wait_until(|| paned.position() == position);

        imp.open_beside(
            &secondary,
            1,
            DocumentLocation {
                page: 2,
                x: Some(0.0),
                y: Some(0.0),
            },
        );
        wait_until(|| primary.viewport().page() == 2);
        assert_eq!(paned.position(), position);
        assert_eq!(primary.viewport().zoom(), 0.75);
        assert_eq!(secondary.viewport().zoom(), 1.25);
        imp.close_pane(&secondary);
        window.close();
    }

    #[gtk::test]
    fn workspace_and_pane_share_the_document() {
        let window = window();

        assert_eq!(window.document(), window.pane().document());
        assert_eq!(window.viewport(), window.pane().viewport().clone());
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
    fn middle_click_on_a_contents_row_opens_the_page_beside() {
        let window = loaded_window();
        let imp = window.imp();
        wait_until(|| !imp.toc_pages.borrow().is_empty());
        imp.toc_revealer.set_reveal_child(true);
        let row = imp.toc_list.row_at_index(1).unwrap();
        wait_until(|| row.height() > 0);

        let point = gtk::graphene::Point::new(0.0, 1.0);
        let y = row.compute_point(&*imp.toc_list, &point).unwrap().y();
        imp.toc_row_split(f64::from(y));

        wait_until(|| {
            imp.secondary
                .borrow()
                .as_ref()
                .is_some_and(|pane| pane.viewport().page() == 1)
        });
        assert!(!imp.toc_revealer.reveals_child(), "the panel closes");
        assert_eq!(imp.active_pane(), imp.primary_pane());

        let secondary = imp.secondary.borrow().as_ref().unwrap().clone();
        imp.close_pane(&secondary);
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
