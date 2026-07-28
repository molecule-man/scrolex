// Global (cross-document) user settings, persisted as a small INI under the user's config dir.

use std::path::PathBuf;
use std::{env, fs, io, thread};

// Render threads = resident MuPDF Documents (one per thread), each accruing an unreclaimable
// per-page cache, so this dial trades memory for parallelism. Rendering scales near-linearly to ~4
// threads before going memory-bandwidth bound; beyond that, more threads mainly buy prefetch depth.
pub const DEFAULT_RENDER_THREADS: usize = 4;

pub const DEFAULT_PREVIEW_CACHE_PAGES: usize = 65;

// Fallback when system memory can't be read; see default_render_cache_mb.
pub const DEFAULT_RENDER_CACHE_MB: usize = 64;
pub const MIN_RENDER_CACHE_MB: usize = 32;
pub const MAX_RENDER_CACHE_MB: usize = 512;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub render_threads: usize,
    pub preview_cache_pages: usize,
    pub render_cache_mb: usize,
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
            preview_cache_pages: DEFAULT_PREVIEW_CACHE_PAGES,
            render_cache_mb: DEFAULT_RENDER_CACHE_MB,
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

// Budget for a machine with `total_mb` of RAM: a sixteenth of it, since resident memory runs a few
// times the budget at high zoom.
fn cache_budget_for_memory(total_mb: usize) -> usize {
    (total_mb / 16).clamp(MIN_RENDER_CACHE_MB, MAX_RENDER_CACHE_MB)
}

fn default_render_cache_mb() -> usize {
    total_memory_mb().map_or(DEFAULT_RENDER_CACHE_MB, cache_budget_for_memory)
}

// Total RAM in MB, read from /proc (Linux).
fn total_memory_mb() -> Option<usize> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let field = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kb: usize = field.split_whitespace().next()?.parse().ok()?;
    Some(kb / 1024)
}

pub fn load_config() -> Config {
    let contents = config_file_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();

    let mut render_threads = DEFAULT_RENDER_THREADS;
    let mut preview_cache_pages = DEFAULT_PREVIEW_CACHE_PAGES;
    let mut render_cache_mb = default_render_cache_mb();
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
            Some(("preview_cache_pages", v)) => {
                if let Ok(n) = v.trim().parse::<usize>() {
                    preview_cache_pages = n.max(1);
                }
            }
            Some(("render_cache_mb", v)) => {
                if let Ok(n) = v.trim().parse::<usize>() {
                    render_cache_mb = n;
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
        preview_cache_pages,
        render_cache_mb: render_cache_mb.clamp(MIN_RENDER_CACHE_MB, MAX_RENDER_CACHE_MB),
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
    out.push_str(&format!(
        "preview_cache_pages={}\n",
        config.preview_cache_pages
    ));
    out.push_str(&format!("render_cache_mb={}\n", config.render_cache_mb));
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
    fn cache_budget_scales_with_memory_within_the_setting_range() {
        assert_eq!(cache_budget_for_memory(256), MIN_RENDER_CACHE_MB); // floor
        assert_eq!(cache_budget_for_memory(1024), 64);
        assert_eq!(cache_budget_for_memory(4096), 256);
        assert_eq!(cache_budget_for_memory(65536), MAX_RENDER_CACHE_MB); // capped
    }

    #[test]
    fn round_trips_and_clamps_to_max() {
        let dir = env::temp_dir().join(format!("scrolex-cfg-test-{}", std::process::id()));
        env::set_var("XDG_CONFIG_HOME", &dir);

        save_config(&Config {
            render_threads: 1,
            preview_cache_pages: 120,
            render_cache_mb: 256,
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
        assert_eq!(loaded.preview_cache_pages, 120);
        assert_eq!(loaded.render_cache_mb, 256);
        assert!(!loaded.animate_scroll);
        let g = loaded.geometry.expect("geometry persisted");
        assert_eq!((g.width, g.height, g.maximized), (1000, 700, true));

        // an over-large value is clamped down to the machine's cap, and omitting geometry clears it
        save_config(&Config {
            render_threads: 9999,
            preview_cache_pages: DEFAULT_PREVIEW_CACHE_PAGES,
            render_cache_mb: DEFAULT_RENDER_CACHE_MB,
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
