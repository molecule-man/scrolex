// Synthetic render backend for reproducing a user's performance profile locally.
//
// Configured via env vars, read once at startup:
//   SCROLEX_EMULATE=1              activate (loads a synthetic document, ignoring any file argument)
//   SCROLEX_EMULATE_PAGES=356      page count
//   SCROLEX_EMULATE_PAGE_PT=597x843  page size in points; drives both full-res and preview memory
//   SCROLEX_EMULATE_FULL_MS=150    full-res render time
//   SCROLEX_EMULATE_PREVIEW_MS=60  preview render time

use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use gtk::cairo::{Context, FontSlant, FontWeight, Format, ImageSurface};

pub const URI: &str = "emulate:///doc";

pub struct Config {
    pub pages: i32,
    pub page_pt: (f64, f64),
    pub full_ms: u64,
    pub preview_ms: u64,
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

pub fn config() -> Option<&'static Config> {
    CONFIG.get_or_init(parse).as_ref()
}

fn parse() -> Option<Config> {
    match env::var("SCROLEX_EMULATE").ok().as_deref() {
        Some("1" | "true") => {}
        _ => return None,
    }

    let page_pt = env::var("SCROLEX_EMULATE_PAGE_PT")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((597.0, 843.0));

    Some(Config {
        pages: env_parse("SCROLEX_EMULATE_PAGES", 356),
        page_pt,
        full_ms: env_parse("SCROLEX_EMULATE_FULL_MS", 150),
        preview_ms: env_parse("SCROLEX_EMULATE_PREVIEW_MS", 60),
    })
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

pub fn pixels(
    cfg: &Config,
    page_num: i32,
    scale: f64,
    dsf: f64,
    is_preview: bool,
) -> (Vec<u8>, i32, i32, i32) {
    let ms = if is_preview {
        cfg.preview_ms
    } else {
        cfg.full_ms
    };
    std::thread::sleep(Duration::from_millis(ms));
    let (w, h) = cfg.page_pt;
    let width = ((w * scale * dsf) as i32).max(1);
    let height = ((h * scale * dsf) as i32).max(1);
    dummy_pixels(page_num, width, height)
}

pub fn full_surface(cfg: &Config, page_num: i32, scale: f64, dsf: f64) -> ImageSurface {
    let (data, width, height, stride) = pixels(cfg, page_num, scale, dsf, false);
    let surface = ImageSurface::create_for_data(data, Format::Rgb24, width, height, stride)
        .expect("emulate surface");
    surface.set_device_scale(dsf, dsf);
    surface
}

fn dummy_pixels(page_num: i32, width: i32, height: i32) -> (Vec<u8>, i32, i32, i32) {
    let mut surface = ImageSurface::create(Format::Rgb24, width, height).expect("emulate surface");
    {
        let cr = Context::new(&surface).expect("emulate context");
        cr.set_source_rgb(0.93, 0.93, 0.93);
        cr.paint().expect("paint");

        let label = format!("{}", page_num + 1);
        let font_size = (width.min(height) as f64 * 0.3).max(8.0);
        cr.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Bold);
        cr.set_font_size(font_size);
        cr.set_source_rgb(0.5, 0.5, 0.5);
        if let Ok(ext) = cr.text_extents(&label) {
            let x = (width as f64 - ext.width()) / 2.0 - ext.x_bearing();
            let y = (height as f64 - ext.height()) / 2.0 - ext.y_bearing();
            cr.move_to(x, y);
            let _ = cr.show_text(&label);
        }
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface.data().expect("emulate data").to_vec();
    (data, width, height, stride)
}
