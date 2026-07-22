// Global (cross-document) user settings, persisted as a small INI under the user's config dir.

use std::path::PathBuf;
use std::{env, fs, thread};

// Render threads = resident MuPDF Documents (one per thread), each accruing an unreclaimable
// per-page cache, so this dial trades memory for parallelism. Rendering scales near-linearly to ~4
// threads before going memory-bandwidth bound; beyond that, more threads mainly buy prefetch depth.
pub const DEFAULT_RENDER_THREADS: usize = 4;

fn config_file_path() -> Option<PathBuf> {
    let mut path = env::var("XDG_CONFIG_HOME")
        .or_else(|_| env::var("HOME").map(|home| format!("{home}/.config")))
        .map(PathBuf::from)
        .ok()?;
    path.push("scrolex");
    path.push("config.ini");
    Some(path)
}

// Upper bound on render threads: reserve one core for the UI thread, since uninterruptible MuPDF
// renders on every core make the UI janky.
pub fn max_render_threads() -> usize {
    thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1))
        .unwrap_or(DEFAULT_RENDER_THREADS)
        .max(1)
}

pub fn load_render_threads() -> usize {
    config_file_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| {
            s.lines().find_map(|line| match line.split_once('=') {
                Some(("render_threads", v)) => v.trim().parse::<usize>().ok(),
                _ => None,
            })
        })
        .unwrap_or(DEFAULT_RENDER_THREADS)
        .clamp(1, max_render_threads())
}

pub fn save_render_threads(n: usize) {
    let Some(path) = config_file_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("Error saving config: {e}");
            return;
        }
    }
    if let Err(e) = fs::write(&path, format!("render_threads={n}\n")) {
        eprintln!("Error saving config: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_clamps_to_max() {
        let dir = env::temp_dir().join(format!("scrolex-cfg-test-{}", std::process::id()));
        env::set_var("XDG_CONFIG_HOME", &dir);

        save_render_threads(1);
        assert_eq!(load_render_threads(), 1);

        // an over-large value is clamped down to the machine's cap
        save_render_threads(9999);
        assert_eq!(load_render_threads(), max_render_threads());

        fs::remove_dir_all(&dir).ok();
    }
}
