use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

#[derive(Debug)]
pub(crate) struct DocumentState {
    pub(crate) zoom: f64,
    pub(crate) crop_left: i32,
    pub(crate) crop_right: i32,
    pub(crate) page: u32,
}

pub(crate) fn load(path: &Path) -> DocumentState {
    let state_path = get_state_file_path(path).unwrap();

    let mut state = DocumentState {
        zoom: 1.0,
        crop_left: 0,
        crop_right: 0,
        page: 0,
    };

    if state_path.exists() {
        for line in fs::read_to_string(&state_path).unwrap().lines() {
            match line.split_once('=') {
                Some(("zoom", value)) => {
                    let zoom = value.parse().unwrap_or(1.0);
                    if zoom > 0.0 {
                        state.zoom = zoom;
                    }
                }
                Some(("crop_left", value)) => {
                    let crop_left = value.parse().unwrap_or(0);
                    state.crop_left = crop_left;
                }
                Some(("crop_right", value)) => {
                    let crop_right = value.parse().unwrap_or(0);
                    state.crop_right = crop_right;
                }
                Some(("page", value)) => {
                    let page = value.parse().unwrap_or(0);
                    state.page = page;
                }
                _ => {}
            }
        }
    }

    state
}

pub(crate) fn save(path: &Path, state: &DocumentState) -> io::Result<()> {
    let state_path = get_state_file_path(path).unwrap();
    let state_dir = state_path.parent().unwrap();

    if !state_dir.exists() {
        fs::create_dir_all(state_dir)?;
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&state_path)?;

    writeln!(file, "zoom={}", state.zoom)?;
    writeln!(file, "crop_left={}", state.crop_left)?;
    writeln!(file, "crop_right={}", state.crop_right)?;
    writeln!(file, "page={}", state.page)?;

    file.flush()
}

fn get_state_file_path(path: &Path) -> Result<PathBuf, env::VarError> {
    let mut state_path = env::var("XDG_STATE_HOME")
        .or_else(|_| env::var("HOME").map(|home| format!("{}/.local/state", home)))
        .map(PathBuf::from)?;

    state_path.push("pdf-viewer");

    for component in path.components().skip(1) {
        state_path.push(component);
    }
    state_path.set_extension("ini");

    Ok(state_path)
}
