// Global (cross-document) user settings, persisted as a small INI under the user's config dir.

use std::path::PathBuf;
use std::{fs, io, thread};

// Render threads rasterize shared display lists, so a thread costs its in-flight pixmaps, not a
// resident MuPDF Document. Rendering scales near-linearly to ~4 threads, then goes
// memory-bandwidth bound. Beyond that, more threads only buy prefetch depth.
pub const DEFAULT_RENDER_THREADS: usize = 4;

pub const DEFAULT_PREVIEW_CACHE_PAGES: usize = 65;

// Fallback when system memory can't be read; see default_render_cache_mb.
pub const DEFAULT_RENDER_CACHE_MB: usize = 64;
pub const MIN_RENDER_CACHE_MB: usize = 32;
pub const MAX_RENDER_CACHE_MB: usize = 512;

pub const DARK_MODE_NOTICE_REVISION: u32 = 1;
const DARK_MODE_NOTICE_ID: u64 = 0x28a9_9587_3f4d_6d3a;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub render_threads: usize,
    pub preview_cache_pages: usize,
    pub render_cache_mb: usize,
    pub animate_scroll: bool,
    pub dark_mode: bool,
    pub always_open_in_tabs: bool,
    pub notice_revision: u32,
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
            render_cache_mb: default_render_cache_mb(),
            animate_scroll: true,
            dark_mode: false,
            always_open_in_tabs: false,
            notice_revision: 0,
            geometry: None,
        }
    }
}

fn config_file_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = test_config_path() {
        return Some(path);
    }

    let mut path = gtk::glib::user_config_dir();
    path.push("scrolex");
    path.push("config.ini");
    Some(path)
}

// Tests build windows that read these settings. Redirect the file per test: one test's setting must
// not reach another test's window, nor the reader's own config.
#[cfg(test)]
struct ScratchConfig {
    dir: tempfile::TempDir,
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: std::cell::RefCell<Option<ScratchConfig>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn test_config_path() -> Option<PathBuf> {
    TEST_CONFIG.with(|config| {
        config
            .borrow()
            .as_ref()
            .map(|config| config.dir.path().join("config.ini"))
    })
}

// Point this thread's settings at an empty file.
#[cfg(test)]
pub(crate) fn use_scratch_config() {
    let dir = tempfile::Builder::new()
        .prefix("scrolex-test-config-")
        .tempdir()
        .expect("scratch config dir");
    TEST_CONFIG.with(|slot| *slot.borrow_mut() = Some(ScratchConfig { dir }));
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
    let mut dark_mode = false;
    let mut always_open_in_tabs = false;
    let mut notice_revision = None;
    let mut dismissed_notice = None;
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
            Some(("dark_mode", v)) => dark_mode = v.trim().parse().unwrap_or(false),
            Some(("open_in_tabs", v)) => always_open_in_tabs = v.trim().parse().unwrap_or(false),
            Some(("dismissed_notice", v)) => {
                dismissed_notice = u64::from_str_radix(v.trim(), 16).ok();
            }
            Some(("notice_revision", v)) => notice_revision = v.trim().parse::<u32>().ok(),
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
    let notice_revision = notice_revision.unwrap_or(match dismissed_notice {
        Some(DARK_MODE_NOTICE_ID) => DARK_MODE_NOTICE_REVISION,
        _ => 0,
    });

    Config {
        render_threads: render_threads.clamp(1, max_render_threads()),
        preview_cache_pages,
        render_cache_mb: render_cache_mb.clamp(MIN_RENDER_CACHE_MB, MAX_RENDER_CACHE_MB),
        animate_scroll,
        dark_mode,
        always_open_in_tabs,
        notice_revision,
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
    out.push_str(&format!("dark_mode={}\n", config.dark_mode));
    out.push_str(&format!("open_in_tabs={}\n", config.always_open_in_tabs));
    out.push_str(&format!("notice_revision={}\n", config.notice_revision));
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
    fn replacing_scratch_config_removes_the_previous_directory() {
        use_scratch_config();
        let path = config_file_path().unwrap();
        save_config(&Config::default()).unwrap();
        let dir = path.parent().unwrap().to_path_buf();

        use_scratch_config();

        assert!(!dir.exists());
    }

    #[test]
    fn cache_budget_scales_with_memory_within_the_setting_range() {
        assert_eq!(cache_budget_for_memory(256), MIN_RENDER_CACHE_MB); // floor
        assert_eq!(cache_budget_for_memory(1024), 64);
        assert_eq!(cache_budget_for_memory(4096), 256);
        assert_eq!(cache_budget_for_memory(65536), MAX_RENDER_CACHE_MB); // capped
    }

    #[test]
    fn round_trips_and_clamps_to_max() {
        use_scratch_config();

        save_config(&Config {
            render_threads: 1,
            preview_cache_pages: 120,
            render_cache_mb: 256,
            animate_scroll: false,
            dark_mode: true,
            always_open_in_tabs: true,
            notice_revision: 2,
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
        assert!(loaded.dark_mode);
        assert!(loaded.always_open_in_tabs);
        assert_eq!(loaded.notice_revision, 2);
        let g = loaded.geometry.expect("geometry persisted");
        assert_eq!((g.width, g.height, g.maximized), (1000, 700, true));

        // an over-large value is clamped down to the machine's cap, and omitting geometry clears it
        save_config(&Config {
            render_threads: 9999,
            preview_cache_pages: DEFAULT_PREVIEW_CACHE_PAGES,
            render_cache_mb: DEFAULT_RENDER_CACHE_MB,
            animate_scroll: true,
            dark_mode: false,
            always_open_in_tabs: false,
            notice_revision: 0,
            geometry: None,
        })
        .unwrap();
        let loaded = load_config();
        assert_eq!(loaded.render_threads, max_render_threads());
        assert!(loaded.animate_scroll);
        assert!(!loaded.dark_mode);
        assert!(!loaded.always_open_in_tabs);
        assert_eq!(loaded.notice_revision, 0);
        assert!(loaded.geometry.is_none());
    }

    #[test]
    fn dark_mode_notice_hash_migrates_to_revision_one() {
        use_scratch_config();
        let path = config_file_path().unwrap();
        fs::write(
            &path,
            "dark_mode=true\nopen_in_tabs=true\nwidth=1111\nheight=777\nmaximized=true\ndismissed_notice=28a995873f4d6d3a\n",
        )
        .unwrap();

        let config = load_config();

        assert_eq!(config.notice_revision, DARK_MODE_NOTICE_REVISION);
        assert!(config.dark_mode);
        assert!(config.always_open_in_tabs);
        assert_eq!(config.geometry.unwrap().width, 1111);
        assert_eq!(config.geometry.unwrap().height, 777);
        assert!(config.geometry.unwrap().maximized);
        save_config(&config).unwrap();
        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("notice_revision=1\n"));
        assert!(!saved.contains("dismissed_notice"));
        assert!(saved.contains("dark_mode=true\n"));
        assert!(saved.contains("open_in_tabs=true\n"));
        assert!(saved.contains("width=1111\n"));
        assert!(saved.contains("height=777\n"));
        assert!(saved.contains("maximized=true\n"));
    }

    #[test]
    fn notice_revision_takes_priority_over_the_notice_hash() {
        use_scratch_config();
        let path = config_file_path().unwrap();
        fs::write(
            path,
            "dismissed_notice=28a995873f4d6d3a\nnotice_revision=2\n",
        )
        .unwrap();

        assert_eq!(load_config().notice_revision, 2);
    }
}
