mod imp;
use gtk::gio::prelude::*;
use gtk::glib;
use gtk::prelude::ObjectExt;
use gtk::subclass::prelude::*;
use poppler::Document;

use std::io::{self, Write};
use std::path::PathBuf;
use std::{env, fs};

glib::wrapper! {
    pub struct State(ObjectSubclass<imp::State>);
}

impl State {
    pub(crate) fn new() -> Self {
        glib::Object::builder()
            .property("zoom", 1.0)
            .property("crop", false)
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

    pub(crate) fn load(&self, f: &gtk::gio::File) -> io::Result<()> {
        if self.doc().is_some() {
            self.save()?;
        }

        let doc = Document::from_gfile(f, None, gtk::gio::Cancellable::NONE)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

        self.emit_by_name::<()>("before-load", &[]);

        let uri = f.uri();
        let state_path = get_state_file_path(&uri).unwrap();

        self.imp().jump_stack.borrow_mut().reset();
        self.set_prev_page(0);
        self.set_uri(uri);
        self.set_doc(doc);
        self.set_zoom(1.0);
        self.set_crop(false);
        self.set_page(0);

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

        self.emit_by_name::<()>("loaded", &[]);

        Ok(())
    }

    pub(crate) fn save(&self) -> io::Result<()> {
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
