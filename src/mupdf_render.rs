// Rasterize PDF pages with MuPDF, which downscale-decodes embedded images (JPEG/JPEG2000) to the
// requested resolution - scanned pages render at fit-to-page cost, not poppler's full-res decode.

use std::cell::RefCell;
use std::collections::{hash_map::Entry, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use gtk::cairo::{Format, ImageSurface};
use gtk::gio::prelude::InputStreamExtManual;
use gtk::prelude::FileExt;
use mupdf::{Colorspace, Device, DisplayList, Document, IRect, Matrix, Pixmap, Rect};
use once_cell::sync::Lazy;

// Bump this version when content span results can change.
pub(crate) const CONTENT_SCAN_VERSION: u16 = 1;
pub(crate) const CONTENT_SCAN_SCALE: f64 = 0.2;

#[derive(Clone, Copy)]
struct DarkMode {
    paper: [u8; 3],
    ink: [u8; 3],
}

const DARK_MODE: DarkMode = DarkMode {
    paper: [0x1e, 0x1e, 0x1e],
    ink: [0xea, 0xea, 0xea],
};

static DARK_MODE_ENABLED: AtomicBool = AtomicBool::new(false);

static GREY_LUT: Lazy<[[u8; 3]; 256]> = Lazy::new(|| {
    std::array::from_fn(|value| recolor(value as u8, value as u8, value as u8, DARK_MODE))
});

// Identifies the bytes behind each cached document. A document load changes this value.
// Later cache access reopens files that changed on disk.
static GENERATION: AtomicU64 = AtomicU64::new(0);

// Non-local GFiles (smb://, sftp://, GVfs mounts) have no local path, and MuPDF opens by path only.
// Stage their bytes to a temp file once, keyed by uri; cleared on invalidate() so a changed remote
// file re-stages. Shutdown skips destructors (main.rs _exit), so the last session's staged file
// lingers in the temp dir - harmless, and left for the OS temp cleaner.
static STAGED: Lazy<Mutex<HashMap<String, PathBuf>>> = Lazy::new(|| Mutex::new(HashMap::new()));

thread_local! {
    // (uri, generation-at-open, Document). One Document per thread: it's bound to the thread's
    // fz_context, so it can't cross threads. Reopened when the uri or the generation changes.
    static DOC: RefCell<Option<(String, u64, Document)>> = const { RefCell::new(None) };
}

// Current document generation, bumped by invalidate(). Callers that cache derived data (e.g. the
// selection glyph list) can key on it so a reload rebuilds.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub fn set_dark_mode(enabled: bool) {
    DARK_MODE_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn dark_mode_enabled() -> bool {
    DARK_MODE_ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn page_background_rgb() -> [u8; 3] {
    dark_mode().map_or([0xff; 3], |mode| mode.paper)
}

pub(crate) fn loading_text_rgb() -> [u8; 3] {
    dark_mode().map_or([0x99; 3], |mode| {
        std::array::from_fn(|i| {
            (0.65 * f32::from(mode.ink[i]) + 0.35 * f32::from(mode.paper[i])).round() as u8
        })
    })
}

// Invalidate cached documents and derived data after a document load. Thread-local documents reopen
// on their next use. The owner clears its documents and display lists on its next request.
pub fn invalidate() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
    let mut staged = STAGED.lock().unwrap();
    for path in staged.values() {
        let _ = std::fs::remove_file(path);
    }
    staged.clear();
}

// Stream a non-local GFile into a secure temp copy (O_EXCL, mode 600): peak memory is one buffer,
// not the whole file. Deletes on drop unless keep()d.
fn fetch_to_temp(file: &gtk::gio::File) -> Option<tempfile::TempPath> {
    let suffix = file
        .basename()
        .and_then(|name| {
            name.extension()
                .map(|ext| format!(".{}", ext.to_string_lossy()))
        })
        .unwrap_or_default();
    let mut reader = file.read(gtk::gio::Cancellable::NONE).ok()?.into_read();
    let mut tmp = tempfile::Builder::new()
        .prefix("scrolex-staged-")
        .suffix(&suffix)
        .tempfile()
        .ok()?;
    std::io::copy(&mut reader, &mut tmp).ok()?;
    Some(tmp.into_temp_path())
}

// A document staged for load: `path` is validated then commit()ted, so validation and render see
// identical bytes (no TOCTOU gap). Dropped uncommitted, an owned temp copy is removed.
pub(crate) struct Candidate {
    uri: String,
    path: PathBuf,
    // Some for a temp copy we own: kept on commit, else deleted on drop.
    temp: Option<tempfile::TempPath>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageSize {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, PartialEq)]
pub(crate) struct DocumentInfo {
    pub page_sizes: Vec<Option<PageSize>>,
}

impl DocumentInfo {
    pub fn tallest_page_height(&self) -> Option<f64> {
        self.page_sizes
            .iter()
            .flatten()
            .map(|size| size.height)
            .max_by(f64::total_cmp)
    }
}

// Stage `uri`: own path if local, else a temp copy of the bytes.
pub(crate) fn stage_candidate(uri: &str) -> Option<Candidate> {
    // Emulate mode has no real file to stage; hand back a placeholder candidate.
    if crate::emulate::config().is_some() {
        return Some(Candidate {
            uri: uri.to_string(),
            path: PathBuf::new(),
            temp: None,
        });
    }
    let file = gtk::gio::File::for_uri(uri);
    if let Some(path) = file.path() {
        return Some(Candidate {
            uri: uri.to_string(),
            path,
            temp: None,
        });
    }
    let temp = fetch_to_temp(&file)?;
    let path = temp.to_path_buf();
    Some(Candidate {
        uri: uri.to_string(),
        path,
        temp: Some(temp),
    })
}

impl Candidate {
    // Read all paper sizes from one document open.
    pub(crate) fn probe(&self) -> Option<DocumentInfo> {
        if let Some(cfg) = crate::emulate::config() {
            return Some(DocumentInfo {
                page_sizes: vec![
                    Some(PageSize {
                        width: cfg.page_pt.0,
                        height: cfg.page_pt.1,
                    });
                    cfg.pages as usize
                ],
            });
        }
        let _ctx = Colorspace::device_bgr();
        let doc = open_document(&self.path)?;
        let n_pages = doc.page_count().ok()?;
        let page_sizes = (0..n_pages)
            .map(|index| {
                let bounds = doc.load_page(index).ok()?.bounds().ok()?;
                Some(PageSize {
                    width: f64::from(bounds.x1 - bounds.x0),
                    height: f64::from(bounds.y1 - bounds.y0),
                })
            })
            .collect();
        Some(DocumentInfo { page_sizes })
    }

    // Publish the validated temp so workers render these exact bytes. Call after invalidate().
    pub(crate) fn commit(mut self) {
        if let Some(temp) = self.temp.take() {
            if let Ok(path) = temp.keep() {
                if let Some(orphan) = STAGED.lock().unwrap().insert(self.uri.clone(), path) {
                    let _ = std::fs::remove_file(orphan);
                }
            }
        }
    }
}

// Local path for `uri`: own path if local, else the staged temp copy (miss → fetch as fallback).
// MuPDF takes a UTF-8 path on Windows, where Path does not convert on its own.
pub(crate) fn open_document(path: &std::path::Path) -> Option<Document> {
    Document::open(path.to_str()?).ok()
}

pub(crate) fn local_path(uri: &str) -> Option<PathBuf> {
    let file = gtk::gio::File::for_uri(uri);
    if let Some(path) = file.path() {
        return Some(path);
    }
    if let Some(path) = STAGED.lock().unwrap().get(uri).cloned() {
        return Some(path);
    }
    // Fetch off-lock: network I/O must not stall invalidate() on the main thread.
    let generation_at_fetch = generation();
    let temp = fetch_to_temp(&file)?;

    let mut staged = STAGED.lock().unwrap();
    // Reload during the fetch (generation bumped) → these bytes are stale; drop (temp deletes here).
    if generation() != generation_at_fetch {
        return None;
    }
    match staged.entry(uri.to_string()) {
        // lost a concurrent staging race for the same uri; our temp drops here as an orphan
        Entry::Occupied(e) => Some(e.get().clone()),
        Entry::Vacant(e) => Some(e.insert(temp.keep().ok()?).clone()),
    }
}

// Run `f` with this thread's MuPDF Document for `uri`, opening it (or reusing the cached one,
// reopening on a uri change). Touches the TLS fz_context before the DOC thread-local so its
// destructor registers first and runs last: our Document's Drop needs a live context, else it aborts
// ("thread local panicked on drop") when a pool worker exits.
pub fn with_doc<T>(uri: &str, f: impl FnOnce(&Document) -> Option<T>) -> Option<T> {
    let _ctx = Colorspace::device_bgr();
    let generation = GENERATION.load(Ordering::Relaxed);
    DOC.with(|cell| {
        let mut slot = cell.borrow_mut();
        let fresh = slot
            .as_ref()
            .is_some_and(|(u, g, _)| u == uri && *g == generation);
        if !fresh {
            let path = local_path(uri)?;
            let doc = open_document(&path)?;
            *slot = Some((uri.to_string(), generation, doc));
        }
        f(&slot.as_ref().unwrap().2)
    })
}

struct LruCache<K, V> {
    capacity: usize,
    entries: VecDeque<(K, V)>,
}

impl<K: PartialEq, V> LruCache<K, V> {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        let index = self
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == key)?;
        let entry = self.entries.remove(index)?;
        self.entries.push_back(entry);
        self.entries.back().map(|(_, value)| value)
    }

    fn insert(&mut self, key: K, value: V) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(entry_key, _)| entry_key == &key)
        {
            self.entries.remove(index);
        }
        self.entries.push_back((key, value));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

struct ListRequest {
    uri: String,
    page: i32,
    reply: mpsc::Sender<Option<Arc<DisplayList>>>,
}

// Display lists the owner keeps alive. A list holds the page's images and fonts, so this trades
// memory for parse time.
const LIST_CACHE_PAGES: usize = 8;

// Documents the owner keeps open. Reopening costs 30us on a small file and 2.8s on a large damaged
// one. Four covers both split-view panes plus the last two tabs.
const OPEN_DOCUMENTS: usize = 4;

static LIST_SOURCE: Lazy<Option<mpsc::Sender<ListRequest>>> = Lazy::new(spawn_list_source);

// One thread owns documents and records page display lists. MuPDF documents cannot cross threads.
// A document per worker duplicates its xref, object cache, and store entries.
// Workers can rasterize one recorded list at the same time.
struct DisplayListOwner {
    generation: u64,
    documents: LruCache<String, Document>,
    lists: LruCache<(String, i32), Arc<DisplayList>>,
}

impl DisplayListOwner {
    fn for_generation(generation: u64) -> Self {
        Self {
            generation,
            documents: LruCache::with_capacity(OPEN_DOCUMENTS),
            lists: LruCache::with_capacity(LIST_CACHE_PAGES),
        }
    }

    fn list(&mut self, generation: u64, uri: &str, page: i32) -> Option<Arc<DisplayList>> {
        if generation != self.generation {
            self.generation = generation;
            self.documents.clear();
            self.lists.clear();
        }

        let key = (uri.to_string(), page);
        if let Some(list) = self.lists.get(&key) {
            return Some(list.clone());
        }

        if self.documents.get(&key.0).is_none() {
            let document = open_document(&local_path(&key.0)?)?;
            self.documents.insert(key.0.clone(), document);
        }
        let document = self.documents.get(&key.0)?;
        let list = Arc::new(document.load_page(page).ok()?.to_display_list(true).ok()?);
        self.lists.insert(key, list.clone());
        Some(list)
    }
}

fn spawn_list_source() -> Option<mpsc::Sender<ListRequest>> {
    // Each caller waits for one reply, so the caller count bounds the queue.
    let (sender, receiver) = mpsc::channel::<ListRequest>();
    let thread = std::thread::Builder::new()
        .name("scrolex-display-lists".to_string())
        .spawn(move || {
            // Touch the context first, so it outlives every Document here; see with_doc.
            let _ctx = Colorspace::device_bgr();
            let mut owner = DisplayListOwner::for_generation(generation());

            while let Ok(req) = receiver.recv() {
                let current_generation = generation();
                let list = owner.list(current_generation, &req.uri, req.page);
                let _ = req.reply.send(list);
            }
        });
    match thread {
        Ok(_) => Some(sender),
        Err(err) => {
            log::error!("could not start the display list thread: {err}");
            None
        }
    }
}

// The page's display list from the owner thread. Blocks until it arrives.
fn display_list(uri: &str, page_num: i32) -> Option<Arc<DisplayList>> {
    let (reply, answer) = mpsc::channel();
    LIST_SOURCE
        .as_ref()?
        .send(ListRequest {
            uri: uri.to_string(),
            page: page_num,
            reply,
        })
        .ok()?;
    answer.recv().ok()?
}

// Rasterize `area` (pixel space) of `list` into a fresh pixmap.
fn raster(list: &DisplayList, ctm: &Matrix, area: IRect) -> Option<Pixmap> {
    // device_bgr + no alpha yields B,G,R samples, matching cairo Rgb24's byte order.
    let colorspace = Colorspace::device_bgr();
    let mut pixmap = Pixmap::new_with_rect(&colorspace, area, false).ok()?;
    pixmap.clear_with(255).ok()?;
    let device = Device::from_pixmap(&pixmap).ok()?;
    list.run(&device, ctm, Rect::from(area)).ok()?;
    drop(device);
    Some(pixmap)
}

// One raster buffer's raw pixels (cairo Rgb24/BGRx), transferable between render and UI threads.
pub struct PagePixels {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
}

// Page pixels at `scale`*`dsf`, or None if unrenderable. `page_pt` sizes the buffer to match the
// render cache's check - MuPDF's pixmap rounding differs ~1px, which would look endlessly stale.
// None → size from MuPDF bounds (bench only).
pub fn render_page_pixels(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    page_pt: Option<(f64, f64)>,
) -> Option<PagePixels> {
    let ctm = Matrix::new_scale((scale * dsf) as f32, (scale * dsf) as f32);
    let list = display_list(uri, page_num)?;
    let bounds = list.bounds();
    let pixmap = raster(&list, &ctm, bounds.transform(&ctm).round())?;
    let page_size = (
        f64::from(bounds.x1 - bounds.x0),
        f64::from(bounds.y1 - bounds.y0),
    );
    let (width_pt, height_pt) = page_pt.unwrap_or(page_size);
    let width = ((width_pt * scale * dsf) as i32).max(1);
    let height = ((height_pt * scale * dsf) as i32).max(1);
    let (data, stride) = pack_pixmap(&pixmap, width, height, dark_mode())?;
    Some(PagePixels {
        data,
        width,
        height,
        stride,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl PixelRect {
    pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }
}

// Rasterize page regions serially from one recorded display list. Pixel-space origins make every
// region land on the same page-anchored grid, so adjacent textures meet without resampling.
pub fn render_page_regions(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    regions: &[PixelRect],
) -> Option<Vec<PagePixels>> {
    let ctm = Matrix::new_scale((scale * dsf) as f32, (scale * dsf) as f32);
    let dark_mode = dark_mode();
    let list = display_list(uri, page_num)?;
    let mut rendered = Vec::with_capacity(regions.len());
    for region in regions {
        debug_assert!(region.x0 < region.x1 && region.y0 < region.y1);
        if region.x0 >= region.x1 || region.y0 >= region.y1 {
            return None;
        }
        let area = IRect::new(region.x0, region.y0, region.x1, region.y1);
        let pixmap = raster(&list, &ctm, area)?;
        let width = region.x1 - region.x0;
        let height = region.y1 - region.y0;
        let (data, stride) = pack_pixmap(&pixmap, width, height, dark_mode)?;
        rendered.push(PagePixels {
            data,
            width,
            height,
            stride,
        });
    }
    Some(rendered)
}

// `render_page_pixels` as an ImageSurface for benchmarks and tests.
pub fn render_page_surface(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    page_pt: Option<(f64, f64)>,
) -> Option<ImageSurface> {
    if let Some(cfg) = crate::emulate::config() {
        return Some(crate::emulate::full_surface(cfg, page_num, scale, dsf));
    }
    let px = render_page_pixels(uri, page_num, scale, dsf, page_pt)?;
    let surface =
        ImageSurface::create_for_data(px.data, Format::Rgb24, px.width, px.height, px.stride)
            .ok()?;
    surface.set_device_scale(dsf, dsf);
    Some(surface)
}

// Page size in points (width, height), or None.
pub fn page_size(uri: &str, page_num: i32) -> Option<(f64, f64)> {
    if let Some(cfg) = crate::emulate::config() {
        let _ = page_num;
        return Some(cfg.page_pt);
    }
    with_doc(uri, |doc| {
        let b = doc.load_page(page_num).ok()?.bounds().ok()?;
        Some(((b.x1 - b.x0) as f64, (b.y1 - b.y0) as f64))
    })
}

// Left and right ink edges in page points, None for a blank page. X only: crop trims side margins.
// Scans rendered pixels because mupdf-rs exposes no ink-bbox api.
pub fn content_x_span(uri: &str, page_num: i32) -> Option<(f64, f64)> {
    if let Some(config) = crate::emulate::config() {
        return Some((0.0, config.page_pt.0));
    }
    let (data, width, height, stride) = with_doc(uri, |doc| {
        let colorspace = Colorspace::device_bgr();
        let page = doc.load_page(page_num).ok()?;
        let ctm = Matrix::new_scale(CONTENT_SCAN_SCALE as f32, CONTENT_SCAN_SCALE as f32);
        let pixmap = page.to_pixmap(&ctm, &colorspace, false, true).ok()?;
        let width = i32::try_from(pixmap.width()).ok()?;
        let height = i32::try_from(pixmap.height()).ok()?;
        let (data, stride) = pack_pixmap(&pixmap, width, height, None)?;
        Some((data, width, height, stride))
    })?;
    let (min_x, max_x) = scan_x_span(&data, width, height, stride as usize)?;
    Some((
        min_x as f64 / CONTENT_SCAN_SCALE,
        (max_x + 1) as f64 / CONTENT_SCAN_SCALE,
    ))
}

// Leftmost and rightmost pixel column (inclusive) holding non-white content in a Rgb24 (BGRx)
// buffer, or None if every pixel is near-white.
fn scan_x_span(data: &[u8], w: i32, h: i32, stride: usize) -> Option<(i32, i32)> {
    let (mut min_x, mut max_x) = (w, -1);
    for y in 0..h {
        let row = &data[y as usize * stride..];
        let ink = |x: i32| {
            let p = &row[x as usize * 4..];
            !(p[0] >= 245 && p[1] >= 245 && p[2] >= 245)
        };
        for x in 0..min_x {
            if ink(x) {
                min_x = x;
                break;
            }
        }
        for x in ((max_x + 1).max(min_x)..w).rev() {
            if ink(x) {
                max_x = x;
                break;
            }
        }
        if min_x == 0 && max_x == w - 1 {
            break;
        }
    }
    (max_x >= min_x).then_some((min_x, max_x))
}

// Pack a MuPDF BGR pixmap into a Rgb24 (BGRx) buffer of exactly (target_w, target_h) plus its stride.
// The pixmap is within ~1px; copy the overlap and fill any padding with the page background.
fn pack_pixmap(
    pix: &mupdf::Pixmap,
    target_w: i32,
    target_h: i32,
    dark_mode: Option<DarkMode>,
) -> Option<(Vec<u8>, i32)> {
    let n = pix.n() as usize; // 3 for device_bgr without alpha
    let src = pix.samples();
    let src_stride = pix.stride() as usize;
    let dst_stride = Format::Rgb24.stride_for_width(target_w as u32).ok()? as usize;

    let mut data = vec![0xffu8; dst_stride * target_h as usize];
    let rows = (pix.height() as usize).min(target_h as usize);
    let cols = (pix.width() as usize).min(target_w as usize);
    for y in 0..rows {
        let srow = &src[y * src_stride..];
        let drow = &mut data[y * dst_stride..];
        for x in 0..cols {
            let s = &srow[x * n..];
            let rgb = match dark_mode {
                Some(_) if s[0] == s[1] && s[1] == s[2] => GREY_LUT[s[0] as usize],
                Some(mode) => recolor(s[2], s[1], s[0], mode),
                None => [s[2], s[1], s[0]],
            };
            drow[x * 4] = rgb[2];
            drow[x * 4 + 1] = rgb[1];
            drow[x * 4 + 2] = rgb[0];
        }
    }

    if let Some(mode) = dark_mode {
        for y in 0..target_h as usize {
            let first_padding_pixel = if y < rows { cols } else { 0 };
            let row = &mut data[y * dst_stride..][first_padding_pixel * 4..target_w as usize * 4];
            for pixel in row.as_chunks_mut::<4>().0 {
                pixel[..3].copy_from_slice(&[mode.paper[2], mode.paper[1], mode.paper[0]]);
            }
        }
    }

    Some((data, dst_stride as i32))
}

fn dark_mode() -> Option<DarkMode> {
    dark_mode_enabled().then_some(DARK_MODE)
}

fn recolor(r: u8, g: u8, b: u8, mode: DarkMode) -> [u8; 3] {
    const WEIGHTS: [f32; 3] = [0.30, 0.59, 0.11];

    let rgb = [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0];
    let paper = mode.paper.map(|value| value as f32 / 255.0);
    let ink = mode.ink.map(|value| value as f32 / 255.0);
    let lightness =
        |color: [f32; 3]| WEIGHTS[0] * color[0] + WEIGHTS[1] * color[1] + WEIGHTS[2] * color[2];
    let source_lightness = lightness(rgb);
    let hue = rgb.map(|channel| channel - source_lightness);
    let source_scale = colorumax(hue, source_lightness, 0.0, 1.0);
    let saturation = if source_scale.abs() > f32::EPSILON {
        1.0 / source_scale
    } else {
        0.0
    };
    let ink_lightness = lightness(ink);
    let paper_lightness = lightness(paper);
    let target_lightness = source_lightness * (paper_lightness - ink_lightness) + ink_lightness;
    let target_scale =
        saturation * colorumax(hue, target_lightness, ink_lightness, paper_lightness);

    std::array::from_fn(|i| {
        ((target_lightness + target_scale * hue[i]) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    })
}

fn colorumax(hue: [f32; 3], lightness: f32, low: f32, high: f32) -> f32 {
    if hue == [0.0; 3] {
        return 0.0;
    }
    let remapped_lightness = (lightness - low) / (high - low);
    let mut source_limit = f32::MAX;
    let mut target_limit = f32::MAX;
    for channel in hue {
        if channel > f32::EPSILON {
            source_limit = source_limit.min(((1.0 - lightness) / channel).abs());
            target_limit = target_limit.min(((1.0 - remapped_lightness) / channel).abs());
        } else if channel < -f32::EPSILON {
            source_limit = source_limit.min((lightness / channel).abs());
            target_limit = target_limit.min((remapped_lightness / channel).abs());
        }
    }
    source_limit.min((high - low).abs() * target_limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_access_delays_eviction() {
        let mut cache = LruCache::with_capacity(2);
        cache.insert(1, "one");
        cache.insert(2, "two");

        assert_eq!(cache.get(&1), Some(&"one"));
        cache.insert(3, "three");

        assert_eq!(cache.get(&1), Some(&"one"));
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), Some(&"three"));
    }

    #[test]
    fn dark_mode_maps_page_and_ink_to_configured_colors() {
        assert_eq!(recolor(0, 0, 0, DARK_MODE), DARK_MODE.ink);
        assert_eq!(recolor(255, 255, 255, DARK_MODE), DARK_MODE.paper);
        assert_eq!(GREY_LUT[0], DARK_MODE.ink);
        assert_eq!(GREY_LUT[255], DARK_MODE.paper);
    }

    #[test]
    fn dark_mode_grey_lut_matches_recolor() {
        for value in 0..=255_u8 {
            assert_eq!(
                GREY_LUT[value as usize],
                recolor(value, value, value, DARK_MODE)
            );
        }
    }

    // Cold (open+repair) vs warm (render) cost, plus a PPM dump to eyeball correctness. Needs a file:
    //   PDF_PATH=/abs/scan.pdf SCALE=0.25 cargo test --release \
    //     mupdf_render::tests::bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench() {
        let path = std::env::var("PDF_PATH").expect("PDF_PATH not set");
        let uri = crate::test_support::file_uri(&path);
        let scale: f64 = std::env::var("SCALE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.25);

        let t = std::time::Instant::now();
        let s = render_page_surface(&uri, 0, scale, 1.0, None).expect("mupdf render");
        let cold = t.elapsed();

        let t = std::time::Instant::now();
        let s2 = render_page_surface(&uri, 0, scale, 1.0, None).expect("mupdf render");
        let warm = t.elapsed();

        println!(
            "mupdf page 0 @ {scale}x: {}x{} | cold (open+repair+render) {cold:?} | warm (render) {warm:?}",
            s2.width(),
            s2.height()
        );

        let out = std::env::temp_dir().join("mupdf_poc.ppm");
        dump_ppm(&s, out.to_str().unwrap());
        println!("wrote {}", out.display());
    }

    // A 200x200 page with a filled rectangle at PDF (60,50) size 80x100 - content that does NOT fill
    // the page, so its bbox must be strictly inside the page (the crop-to-content case).
    const MARGIN_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 200 200] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 26 >>\nstream\n0 0 0 rg 60 50 80 100 re f\nendstream\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    // Written once. Parallel tests share the path, and a second write truncates the file while
    // another test opens it.
    fn margin_pdf_uri() -> String {
        static URI: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        URI.get_or_init(|| {
            let dir = std::env::temp_dir().join("scrolex_content_x_span_test");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("margins.pdf");
            std::fs::write(&path, MARGIN_PDF).unwrap();
            crate::test_support::file_uri(&path)
        })
        .clone()
    }

    #[test]
    fn page_regions_match_the_same_pixels_in_a_full_render() {
        let uri = margin_pdf_uri();
        let full = render_page_pixels(&uri, 0, 1.0, 1.0, Some((200.0, 200.0))).unwrap();
        let regions = [
            PixelRect::new(0, 0, 100, 100),
            PixelRect::new(100, 0, 200, 100),
            PixelRect::new(0, 100, 100, 200),
            PixelRect::new(100, 100, 200, 200),
        ];
        let tiles = render_page_regions(&uri, 0, 1.0, 1.0, &regions).unwrap();

        for (region, tile) in regions.into_iter().zip(tiles) {
            for y in 0..tile.height {
                for x in 0..tile.width {
                    let tile_offset = (y * tile.stride + x * 4) as usize;
                    let page_offset =
                        ((region.y0 + y) * full.stride + (region.x0 + x) * 4) as usize;
                    assert_eq!(
                        &tile.data[tile_offset..tile_offset + 3],
                        &full.data[page_offset..page_offset + 3],
                        "pixel differs at ({}, {})",
                        region.x0 + x,
                        region.y0 + y,
                    );
                }
            }
        }
    }

    // A shared display list must render identically from any number of threads.
    #[test]
    fn concurrent_renders_of_one_page_match() {
        let uri = margin_pdf_uri();
        let expected = render_page_pixels(&uri, 0, 1.0, 1.0, Some((200.0, 200.0))).unwrap();
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let uri = uri.clone();
                std::thread::spawn(move || {
                    render_page_pixels(&uri, 0, 1.0, 1.0, Some((200.0, 200.0))).unwrap()
                })
            })
            .collect();

        for worker in workers {
            let rendered = worker.join().unwrap();
            assert_eq!(
                (rendered.width, rendered.height, rendered.stride),
                (expected.width, expected.height, expected.stride)
            );
            assert_eq!(rendered.data, expected.data);
        }
    }

    #[test]
    fn owner_generation_discards_recorded_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.pdf");
        let uri = crate::test_support::file_uri(&path);
        std::fs::write(&path, MARGIN_PDF).unwrap();
        let mut owner = DisplayListOwner::for_generation(10);

        let first = owner.list(10, &uri, 0).unwrap();
        assert_eq!(first.bounds(), Rect::new(0.0, 0.0, 200.0, 200.0));

        let larger = String::from_utf8(MARGIN_PDF.to_vec())
            .unwrap()
            .replace("/MediaBox [0 0 200 200]", "/MediaBox [0 0 400 400]");
        std::fs::write(&path, larger).unwrap();

        let reloaded = owner.list(11, &uri, 0).unwrap();
        assert_eq!(reloaded.bounds(), Rect::new(0.0, 0.0, 400.0, 400.0));
        assert_eq!(first.bounds(), Rect::new(0.0, 0.0, 200.0, 200.0));
    }

    #[test]
    fn owner_serves_each_document_its_own_pages() {
        let (small, large) = (margin_pdf_uri(), mixed_size_pdf_uri());
        let mut owner = DisplayListOwner::for_generation(10);
        for _ in 0..3 {
            let list = owner.list(10, &small, 0).unwrap();
            assert_eq!(list.bounds(), Rect::new(0.0, 0.0, 200.0, 200.0));
            let list = owner.list(10, &large, 1).unwrap();
            assert_eq!(list.bounds(), Rect::new(0.0, 0.0, 2000.0, 3000.0));
        }
    }

    #[test]
    fn page_count_and_size_read_the_document() {
        let uri = margin_pdf_uri();
        assert_eq!(
            stage_candidate(&uri).unwrap().probe(),
            Some(DocumentInfo {
                page_sizes: vec![Some(PageSize {
                    width: 200.0,
                    height: 200.0,
                })],
            })
        );
        assert_eq!(page_size(&uri, 0), Some((200.0, 200.0)));
        // out-of-range / unopenable degrade rather than panic
        assert_eq!(page_size(&uri, 99), None);
        // a local uri always stages (the path exists as a value); an unopenable file fails to probe
        assert_eq!(
            stage_candidate("file:///no/such/file.pdf").unwrap().probe(),
            None
        );
    }

    // Three pages, the middle one much larger: a small cover followed by big content, the case where
    // the first page alone would understate the document's size.
    const MIXED_SIZE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n\
4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 2000 3000] >>\nendobj\n\
5 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 400] >>\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    // Written once, for the same reason as margin_pdf_uri.
    fn mixed_size_pdf_uri() -> String {
        static URI: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        URI.get_or_init(|| {
            let dir = std::env::temp_dir().join("scrolex_mixed_size_test");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("mixed.pdf");
            std::fs::write(&path, MIXED_SIZE_PDF).unwrap();
            crate::test_support::file_uri(&path)
        })
        .clone()
    }

    #[test]
    fn probe_reports_the_tallest_page() {
        let uri = mixed_size_pdf_uri();

        assert_eq!(
            stage_candidate(&uri).unwrap().probe(),
            Some(DocumentInfo {
                page_sizes: vec![
                    Some(PageSize {
                        width: 200.0,
                        height: 200.0,
                    }),
                    Some(PageSize {
                        width: 2000.0,
                        height: 3000.0,
                    }),
                    Some(PageSize {
                        width: 400.0,
                        height: 400.0,
                    }),
                ],
            })
        );
    }

    // The tall page sits after the first eight pages.
    const TALL_LAST_PAGE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R 7 0 R 8 0 R 9 0 R 10 0 R 11 0 R 12 0 R 13 0 R 14 0 R] /Count 12 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
5 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
6 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
7 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
8 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
9 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
10 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
11 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
12 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
13 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 200] >>\nendobj\n\
14 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 900] >>\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    #[test]
    fn probe_reads_every_page_not_a_sample() {
        let dir = std::env::temp_dir().join("scrolex_tall_last_page_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tall_last.pdf");
        std::fs::write(&path, TALL_LAST_PAGE_PDF).unwrap();
        let uri = crate::test_support::file_uri(&path);

        let info = stage_candidate(&uri).unwrap().probe().unwrap();
        assert_eq!(info.page_sizes.len(), 12);
        assert_eq!(info.tallest_page_height(), Some(900.0));
    }

    // A 300x200 page with /Rotate 90 (displayed 200x300) and the word "Hello" near the top-left of
    // the unrotated page, for checking rotation-frame consistency across providers.
    const ROTATED_TEXT_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 200] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Rotate 90 /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 34 >>\nstream\nBT /F1 24 Tf 40 150 Td (Hello) Tj ET\nendstream\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    // /Rotate 90 on a 300x200 page must present as 200x300, and every provider (render→content_x_span,
    // and text search) must report in that same rotated display frame so overlays land on the render.
    #[gtk::test]
    fn rotated_page_consistent_across_providers() {
        let dir = std::env::temp_dir().join("scrolex_rot");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rot.pdf");
        std::fs::write(&path, ROTATED_TEXT_PDF).unwrap();
        let uri = crate::test_support::file_uri(&path);

        // rotation applied: displayed dimensions are swapped
        assert_eq!(page_size(&uri, 0), Some((200.0, 300.0)));

        let (cx1, cx2) = content_x_span(&uri, 0).expect("content_x_span");
        assert!(
            (135.0..=155.0).contains(&cx1) && (160.0..=180.0).contains(&cx2),
            "content x span not in the rotated frame: {:?}",
            (cx1, cx2)
        );

        // the "Hello" search hit must fall in the same frame - overlapping the content bbox, not in
        // an unrotated frame (which would mean overlays are misplaced on rotated pages).
        let quad = with_doc(&uri, |doc| {
            let quads = doc.load_page(0).ok()?.search("Hello", 4).ok()?;
            quads.iter().next().map(|q| {
                let xs = [q.ul.x, q.ur.x, q.ll.x, q.lr.x];
                let ys = [q.ul.y, q.ur.y, q.ll.y, q.lr.y];
                (
                    xs.iter().cloned().fold(f32::INFINITY, f32::min) as f64,
                    ys.iter().cloned().fold(f32::INFINITY, f32::min) as f64,
                    xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max) as f64,
                    ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max) as f64,
                )
            })
        })
        .expect("search found 'Hello'");

        assert!(
            quad.0 < cx2 && quad.2 > cx1,
            "search hit {quad:?} does not overlap content x span ({cx1},{cx2}) - frame mismatch"
        );
    }

    // A 10x10 white Rgb24 buffer with black pixels at the given (x, y).
    fn ink_buffer(pixels: &[(usize, usize)]) -> (Vec<u8>, i32, i32, usize) {
        let (w, h) = (10i32, 10i32);
        let stride = (w * 4) as usize;
        let mut data = vec![0xffu8; stride * h as usize];
        for &(x, y) in pixels {
            let o = y * stride + x * 4;
            data[o..o + 3].fill(0);
        }
        (data, w, h, stride)
    }

    #[test]
    fn scan_x_span_finds_non_white_block() {
        // black block at x 3..=6, y 2..=5
        let block: Vec<_> = (2..=5).flat_map(|y| (3..=6).map(move |x| (x, y))).collect();
        let (data, w, h, stride) = ink_buffer(&block);
        assert_eq!(scan_x_span(&data, w, h, stride), Some((3, 6)));
    }

    // The widest row decides the span, even when a later row sits inside it.
    #[test]
    fn scan_x_span_spreads_across_rows() {
        let (data, w, h, stride) = ink_buffer(&[(4, 0), (1, 3), (8, 7)]);
        assert_eq!(scan_x_span(&data, w, h, stride), Some((1, 8)));
    }

    // Content touching both edges reports the full width.
    #[test]
    fn scan_x_span_covers_the_full_width() {
        let (data, w, h, stride) = ink_buffer(&[(0, 4), (9, 4)]);
        assert_eq!(scan_x_span(&data, w, h, stride), Some((0, 9)));
    }

    #[test]
    fn scan_x_span_none_when_all_white() {
        let stride = 10 * 4;
        assert_eq!(
            scan_x_span(&vec![0xffu8; stride * 10], 10, 10, stride),
            None
        );
    }

    // Regression guard for the crop bug: content_x_span must trim to the mark, not return the full
    // page. Renders a real page via MuPDF (opened by path), so it also covers the render+scale path.
    #[gtk::test]
    fn content_x_span_trims_to_content_not_full_page() {
        let uri = margin_pdf_uri();
        let (x1, x2) = content_x_span(&uri, 0).expect("content_x_span on a rendered page");
        // strictly inside the 200x200 page - the exact assertion the full-mediabox bug failed
        assert!(
            x1 > 0.0 && x2 < 200.0,
            "span not trimmed (returned ~full page?): {:?}",
            (x1, x2)
        );
        // roughly the mark: PDF rect (60,50)-(140,150)
        assert!((40.0..80.0).contains(&x1), "x1 off: {x1}");
        assert!((120.0..160.0).contains(&x2), "x2 off: {x2}");
    }

    // Cairo Rgb24 stores BGRx; write a P6 for eyeballing colors (catches an R/B swap).
    fn dump_ppm(surface: &ImageSurface, path: &str) {
        surface.flush();
        let (w, h, stride) = (surface.width(), surface.height(), surface.stride());
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        surface
            .with_data(|d| {
                for y in 0..h as usize {
                    let row = &d[y * stride as usize..];
                    for x in 0..w as usize {
                        ppm.extend_from_slice(&[row[x * 4 + 2], row[x * 4 + 1], row[x * 4]]);
                    }
                }
            })
            .unwrap();
        std::fs::write(path, ppm).unwrap();
    }
}
