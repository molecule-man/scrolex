// Global (cross-document) user settings, persisted as a small INI under the user's config dir.

use std::path::PathBuf;
use std::{env, fs, io, thread};

// Render threads = resident MuPDF Documents (one per thread), each accruing an unreclaimable
// per-page cache, so this dial trades memory for parallelism. Rendering scales near-linearly to ~4
// threads before going memory-bandwidth bound; beyond that, more threads mainly buy prefetch depth.
pub const DEFAULT_RENDER_THREADS: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub render_threads: usize,
    pub animate_scroll: bool,
    pub geometry: Option<Geometry>,
}

// Last-used main-window size and maximized state, restored on the next launch.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            render_threads: DEFAULT_RENDER_THREADS,
            animate_scroll: true,
            geometry: None,
        }
    }
}

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

pub fn load_config() -> Config {
    let contents = config_file_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();

    let mut render_threads = DEFAULT_RENDER_THREADS;
    let mut animate_scroll = true;
    let mut width = None;
    let mut height = None;
    let mut maximized = false;

    for line in contents.lines() {
        match line.split_once('=') {
            Some(("render_threads", v)) => {
                if let Ok(n) = v.trim().parse() {
                    render_threads = n;
                }
            }
            Some(("animate_scroll", v)) => animate_scroll = v.trim().parse().unwrap_or(true),
            Some(("width", v)) => width = v.trim().parse::<i32>().ok().filter(|&w| w > 0),
            Some(("height", v)) => height = v.trim().parse::<i32>().ok().filter(|&h| h > 0),
            Some(("maximized", v)) => maximized = v.trim().parse().unwrap_or(false),
            _ => {}
        }
    }

    let geometry = match (width, height) {
        (Some(width), Some(height)) => Some(Geometry {
            width,
            height,
            maximized,
        }),
        _ => None,
    };

    Config {
        render_threads: render_threads.clamp(1, max_render_threads()),
        animate_scroll,
        geometry,
    }
}

pub fn save_config(config: &Config) -> io::Result<()> {
    let path = config_file_path().ok_or_else(|| io::Error::other("no config dir"))?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }

    let mut out = format!("render_threads={}\n", config.render_threads);
    out.push_str(&format!("animate_scroll={}\n", config.animate_scroll));
    if let Some(g) = config.geometry {
        out.push_str(&format!("width={}\n", g.width));
        out.push_str(&format!("height={}\n", g.height));
        out.push_str(&format!("maximized={}\n", g.maximized));
    }

    fs::write(&path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_clamps_to_max() {
        let dir = env::temp_dir().join(format!("scrolex-cfg-test-{}", std::process::id()));
        env::set_var("XDG_CONFIG_HOME", &dir);

        save_config(&Config {
            render_threads: 1,
            animate_scroll: false,
            geometry: Some(Geometry {
                width: 1000,
                height: 700,
                maximized: true,
            }),
        })
        .unwrap();
        let loaded = load_config();
        assert_eq!(loaded.render_threads, 1);
        assert!(!loaded.animate_scroll);
        let g = loaded.geometry.expect("geometry persisted");
        assert_eq!((g.width, g.height, g.maximized), (1000, 700, true));

        // an over-large value is clamped down to the machine's cap, and omitting geometry clears it
        save_config(&Config {
            render_threads: 9999,
            animate_scroll: true,
            geometry: None,
        })
        .unwrap();
        let loaded = load_config();
        assert_eq!(loaded.render_threads, max_render_threads());
        assert!(loaded.animate_scroll);
        assert!(loaded.geometry.is_none());

        fs::remove_dir_all(&dir).ok();
    }
}
