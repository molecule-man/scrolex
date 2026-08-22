// Document data and the viewport state that reads it.
mod document;
mod document_imp;
mod viewport;
mod viewport_imp;

use gtk::glib;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

pub(crate) use document::Document;
pub(crate) use viewport::Viewport;

// Per-preview size the adaptive preview scaler steers toward. The preview cache's byte budget is
// this times the configured number of resident previews (config::preview_cache_pages), so the cache
// holds about that many previews regardless of the adaptive scale.
pub(crate) const PREVIEW_TARGET_BYTES: usize = 20 * 1024 * 1024 / 65;

// Zoom bounds. The same for every document: huge pages are the ones that need deep zoom most.
// Render buffers are bounded by scale instead (see page::render_scale).
const MAX_ZOOM: f64 = 10.0;
const MIN_ZOOM: f64 = 0.05;

// The zoom a typed percent asks for. None below MIN_ZOOM: too small is a typo, so keep the current
// zoom instead of clamping up to it.
pub(crate) fn zoom_from_percent(percent: f64) -> Option<f64> {
    let zoom = percent / 100.0;

    (zoom >= MIN_ZOOM).then(|| zoom.min(MAX_ZOOM))
}

pub(crate) fn zoom_is_supported(zoom: f64) -> bool {
    (MIN_ZOOM..=MAX_ZOOM).contains(&zoom)
}

// Zoom as a percent for the entry, at most two decimals so that it fully fits into entry input
pub(crate) fn zoom_percent_text(zoom: f64) -> String {
    format!("{}", (zoom * 10_000.0).round() / 100.0)
}

// Preview cache byte budget for a given number of resident previews.
pub(crate) fn preview_cache_budget(pages: usize) -> usize {
    pages * PREVIEW_TARGET_BYTES
}

// Where the reader left a document. Saved per uri, restored on open.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Position {
    pub zoom: f64,
    pub page: u32,
    pub crop: bool,
}

impl Default for Position {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            page: 0,
            crop: false,
        }
    }
}

impl Position {
    // Defaults for a document with no saved position.
    pub(crate) fn read(uri: &str) -> Self {
        let path = get_state_file_path(uri);
        let Ok(saved) = fs::read_to_string(&path) else {
            return Self::default();
        };

        let mut position = Self::default();
        for line in saved.lines() {
            match line.split_once('=') {
                Some(("zoom", value)) => {
                    let zoom = value.parse().unwrap_or(1.0);
                    if zoom > 0.0 {
                        position.zoom = zoom;
                    }
                }
                Some(("page", value)) => position.page = value.parse().unwrap_or(0),
                Some(("crop", value)) => position.crop = value.parse().unwrap_or(false),
                _ => {}
            }
        }
        position
    }

    pub(crate) fn write(&self, uri: &str) -> io::Result<()> {
        let path = get_state_file_path(uri);
        let dir = path.parent().unwrap();

        if !dir.exists() {
            fs::create_dir_all(dir)?;
        }

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        writeln!(file, "zoom={}", self.zoom)?;
        writeln!(file, "page={}", self.page)?;
        writeln!(file, "crop={}", self.crop)?;

        file.flush()
    }
}

// Tests open documents, and an open writes the reading position. Redirect the directory per test:
// a zoom left by one test must not come back in another, nor in the reader's own files.
#[cfg(test)]
struct ScratchState {
    dir: tempfile::TempDir,
}

#[cfg(test)]
thread_local! {
    static TEST_STATE: std::cell::RefCell<Option<ScratchState>> =
        const { std::cell::RefCell::new(None) };
}

// Point this thread's per-document state at an empty directory.
#[cfg(test)]
pub(crate) fn use_scratch_state_dir() {
    let dir = tempfile::Builder::new()
        .prefix("scrolex-test-state-")
        .tempdir()
        .expect("scratch state dir");
    TEST_STATE.with(|slot| *slot.borrow_mut() = Some(ScratchState { dir }));
}

// A uri maps to nested directories under the state dir. Windows forbids : ? " < > | * and \\ in a
// name, and every uri starts with a scheme colon, so those are replaced there. Paths on other
// platforms keep their existing layout.
#[cfg(not(windows))]
fn uri_components(uri: &str) -> PathBuf {
    PathBuf::from(uri)
}

#[cfg(windows)]
fn uri_components(uri: &str) -> PathBuf {
    uri.split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.replace([':', '?', '"', '<', '>', '|', '*', '\\'], "_"))
        .collect()
}

fn get_state_file_path(uri: &str) -> PathBuf {
    #[cfg(test)]
    if let Some(mut state_path) = TEST_STATE.with(|state| {
        state
            .borrow()
            .as_ref()
            .map(|state| state.dir.path().to_path_buf())
    }) {
        state_path.push(uri_components(uri));
        state_path.set_extension("ini");
        return state_path;
    }

    let mut state_path = glib::user_state_dir();
    state_path.push("pdf-viewer");
    state_path.push(uri_components(uri));
    state_path.set_extension("ini");

    state_path
}

#[cfg(test)]
mod tests {
    use super::*;

    // Windows rejects a colon in a filename, and every uri carries a scheme colon.
    #[test]
    fn a_state_path_for_a_file_uri_can_be_written() {
        use_scratch_state_dir();
        let path = get_state_file_path("file:///D:/a/scrolex/tests/fixtures/no_outline.pdf");

        fs::create_dir_all(path.parent().expect("a parent")).expect("create the state directory");
        fs::write(&path, "zoom=1\n").expect("write the state file");

        assert!(path.exists());
    }

    #[test]
    fn replacing_scratch_state_removes_the_previous_directory() {
        use_scratch_state_dir();
        Position::default()
            .write("scratch.pdf")
            .expect("write the position");
        let dir = get_state_file_path("scratch.pdf")
            .parent()
            .unwrap()
            .to_path_buf();

        use_scratch_state_dir();

        assert!(!dir.exists());
    }

    #[test]
    fn a_saved_position_comes_back() {
        use_scratch_state_dir();
        let position = Position {
            zoom: 2.5,
            page: 7,
            crop: true,
        };
        position.write("position.pdf").expect("write the position");

        assert_eq!(Position::read("position.pdf"), position);
    }

    #[test]
    fn an_unknown_document_reads_the_defaults() {
        use_scratch_state_dir();

        assert_eq!(Position::read("never-opened.pdf"), Position::default());
    }
}
