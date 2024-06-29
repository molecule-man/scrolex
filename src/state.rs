use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

#[derive(Debug)]
pub(crate) struct DocumentState {
    pub(crate) zoom: f64,
    pub(crate) scroll_position: f64,
    pub(crate) start: usize,
}

pub(crate) fn load(path: &Path) -> DocumentState {
    let state_path = get_state_file_path(path).unwrap();

    let mut state = DocumentState {
        zoom: 1.0,
        scroll_position: 0.0,
        start: 0,
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
                Some(("scroll_position", value)) => {
                    let scroll_position = value.parse().unwrap_or(0.0);
                    state.scroll_position = scroll_position;
                }
                Some(("start", value)) => {
                    let start = value.parse().unwrap_or(0);
                    state.start = start;
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
    writeln!(file, "scroll_position={}", state.scroll_position)?;
    writeln!(file, "start={}", state.start)?;

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
