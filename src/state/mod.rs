mod imp;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::*;
use poppler::Document;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::{env, fs};

use crate::page;

// Memory budget for the low-resolution preview cache. Spread over the resident preview window this
// also bounds the adaptive preview scale, so it holds a wide scroll window without thrashing.
pub(crate) const PREVIEW_CACHE_BUDGET: usize = 20 * 1024 * 1024;

glib::wrapper! {
    pub struct State(ObjectSubclass<imp::State>);
}

impl State {
    pub(crate) fn new() -> Self {
        // the preview-cache budget and other builder-instance setup live in State's `constructed`,
        // which runs here too
        glib::Object::builder()
            .property("zoom", 1.0)
            .property("crop", false)
            .property("animate_scroll", true)
            .property("page", 0_u32)
            .build()
    }

    pub(crate) fn jump_list_add(&self, page: u32) {
        self.set_prev_page(page);
        self.imp().jump_stack.borrow_mut().push(page);
    }

    pub(crate) fn jump_list_pop(&self) -> Option<u32> {
        let page = self.imp().jump_stack.borrow_mut().pop();
        self.set_prev_page(self.imp().jump_stack.borrow().peek().unwrap_or_default());
        page
    }

    pub fn load(&self, f: &gtk::gio::File) -> io::Result<()> {
        if self.doc().is_some() {
            self.save()?;
        }

        let doc =
            Document::from_gfile(f, None, gtk::gio::Cancellable::NONE).map_err(io::Error::other)?;
        self.imp().bbox_cache.borrow_mut().clear();
        self.imp().links.borrow_mut().clear();
        self.imp().search.borrow_mut().clear();
        self.imp().render_cache.borrow_mut().clear();
        self.imp().render_inflight.borrow_mut().clear();
        self.imp().render_waiters.borrow_mut().clear();
        self.imp().preview_cache.borrow_mut().clear();
        self.imp().preview_inflight.borrow_mut().clear();
        self.imp().preview_enabled.set(true);
        self.imp().preview_slow_streak.set(0);
        self.imp()
            .preview_scale
            .set(crate::page::PREVIEW_INITIAL_SCALE);

        self.emit_by_name::<()>("before-load", &[]);

        let uri = f.uri();
        let state_path = get_state_file_path(&uri).unwrap();

        self.imp().jump_stack.borrow_mut().reset();
        self.set_prev_page(0);
        self.set_uri(uri);
        crate::image_page::prewarm(&self.uri());
        self.set_doc(doc);
        self.set_zoom(1.0);
        self.set_crop(false);
        self.set_page(0);
        self.set_multithread_rendering(false);

        if state_path.exists() {
            for line in fs::read_to_string(&state_path).unwrap().lines() {
                match line.split_once('=') {
                    Some(("zoom", value)) => {
                        let zoom = value.parse().unwrap_or(1.0);
                        if zoom > 0.0 {
                            self.set_zoom(zoom);
                        }
                    }
                    Some(("page", value)) => {
                        let page = value.parse().unwrap_or(0);
                        self.set_page(page);
                    }
                    Some(("crop", value)) => {
                        let crop = value.parse().unwrap_or(false);
                        self.set_crop(crop);
                    }
                    _ => {}
                }
            }
        }

        self.log_document_info(f);

        self.emit_by_name::<()>("loaded", &[]);

        Ok(())
    }

    fn log_document_info(&self, f: &gtk::gio::File) {
        let Some(doc) = self.doc() else {
            return;
        };

        let size_bytes = f
            .query_info(
                "standard::size",
                gtk::gio::FileQueryInfoFlags::NONE,
                gtk::gio::Cancellable::NONE,
            )
            .map(|info| info.size())
            .unwrap_or(-1);

        let n_pages = doc.n_pages();
        let first_page_size = doc.page(0).map(|p| p.size());

        log::info!(
            "Loaded document: {n_pages} pages, {size_bytes} bytes, first page {first_page_size:?} pt, \
             start page {}, zoom {}, crop {}",
            self.page(),
            self.zoom(),
            self.crop(),
        );
    }

    pub fn save(&self) -> io::Result<()> {
        let state_path = get_state_file_path(&self.uri()).unwrap();
        let state_dir = state_path.parent().unwrap();

        if !state_dir.exists() {
            fs::create_dir_all(state_dir)?;
        }

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&state_path)?;

        writeln!(file, "zoom={}", self.zoom())?;
        writeln!(file, "page={}", self.page())?;
        writeln!(file, "crop={}", self.crop())?;

        file.flush()
    }

    pub(crate) fn bbox_cache(&self) -> Rc<RefCell<HashMap<i32, page::Rectangle>>> {
        self.imp().bbox_cache.clone()
    }

    pub(crate) fn search(&self) -> Rc<RefCell<crate::search::Search>> {
        self.imp().search.clone()
    }

    pub(crate) fn render_cache(&self) -> Rc<RefCell<crate::render_cache::RenderCache>> {
        self.imp().render_cache.clone()
    }

    pub(crate) fn render_inflight(&self) -> Rc<RefCell<HashSet<i32>>> {
        self.imp().render_inflight.clone()
    }

    pub(crate) fn render_waiters(&self) -> Rc<RefCell<HashMap<i32, glib::WeakRef<page::Page>>>> {
        self.imp().render_waiters.clone()
    }

    pub(crate) fn preview_cache(&self) -> Rc<RefCell<crate::render_cache::RenderCache>> {
        self.imp().preview_cache.clone()
    }

    pub(crate) fn preview_inflight(&self) -> Rc<RefCell<HashSet<i32>>> {
        self.imp().preview_inflight.clone()
    }

    pub(crate) fn preview_enabled(&self) -> bool {
        self.imp().preview_enabled.get()
    }

    pub(crate) fn set_preview_enabled(&self, enabled: bool) {
        self.imp().preview_enabled.set(enabled);
    }

    pub(crate) fn preview_slow_streak(&self) -> u32 {
        self.imp().preview_slow_streak.get()
    }

    pub(crate) fn set_preview_slow_streak(&self, streak: u32) {
        self.imp().preview_slow_streak.set(streak);
    }

    pub(crate) fn preview_scale(&self) -> f64 {
        self.imp().preview_scale.get()
    }

    pub(crate) fn set_preview_scale(&self, scale: f64) {
        self.imp().preview_scale.set(scale);
    }

    pub(crate) fn scrolling(&self) -> bool {
        self.imp().scrolling.get()
    }

    pub(crate) fn set_scrolling(&self, scrolling: bool) {
        self.imp().scrolling.set(scrolling);
    }

    pub(crate) fn scroll_forward(&self) -> bool {
        self.imp().scroll_forward.get()
    }

    pub(crate) fn set_scroll_forward(&self, forward: bool) {
        self.imp().scroll_forward.set(forward);
    }

    pub(crate) fn render_threads(&self) -> usize {
        self.imp().render_threads.get()
    }

    pub(crate) fn set_render_threads(&self, n: usize) {
        self.imp().render_threads.set(n);
    }

    pub(crate) fn visible_page_count(&self) -> i32 {
        self.imp().visible_page_count.get()
    }

    pub(crate) fn set_visible_page_count(&self, count: i32) {
        self.imp().visible_page_count.set(count);
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

fn get_state_file_path(uri: &str) -> Result<PathBuf, env::VarError> {
    let mut state_path = env::var("XDG_STATE_HOME")
        .or_else(|_| env::var("HOME").map(|home| format!("{home}/.local/state")))
        .map(PathBuf::from)?;

    state_path.push("pdf-viewer");
    state_path.push(uri);
    state_path.set_extension("ini");

    Ok(state_path)
}
