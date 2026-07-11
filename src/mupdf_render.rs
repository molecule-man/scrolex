// Rasterize PDF pages with MuPDF, which downscale-decodes embedded images (JPEG/JPEG2000) to the
// requested resolution - scanned pages render at fit-to-page cost, not poppler's full-res decode.

use std::cell::RefCell;
use std::collections::{hash_map::Entry, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use gtk::cairo::{Format, ImageSurface};
use gtk::gio::prelude::InputStreamExtManual;
use gtk::prelude::FileExt;
use mupdf::{Colorspace, Document, Matrix};
use once_cell::sync::Lazy;

// Bumped on document load so every thread's cached Document is reopened - otherwise reloading the
// same path after the file changed on disk would keep serving the stale document.
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

// Invalidate every thread's cached Document (call on document load). The next `with_doc` on each
// thread reopens against the current bytes, and any staged remote copies are re-fetched.
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
    let mut reader = file.read(gtk::gio::Cancellable::NONE).ok()?.into_read();
    let mut tmp = tempfile::Builder::new()
        .prefix("scrolex-staged-")
        .suffix(".pdf")
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

// Stage `uri`: own path if local, else a temp copy of the bytes.
pub(crate) fn stage_candidate(uri: &str) -> Option<Candidate> {
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
    // Page count of a fresh, side-effect-free open; 0 if unopenable (a failed load).
    pub(crate) fn page_count(&self) -> i32 {
        Document::open(self.path.as_path())
            .and_then(|d| d.page_count())
            .unwrap_or(0)
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
            let doc = Document::open(path.as_path()).ok()?;
            *slot = Some((uri.to_string(), generation, doc));
        }
        f(&slot.as_ref().unwrap().2)
    })
}

// One page's raw pixels (cairo Rgb24/BGRx) — shipped from a worker since ImageSurface isn't Send.
pub struct PagePixels {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
}

// Page pixels at `scale`*`dsf`, or None if unrenderable. `page_pt` sizes the buffer to match the
// render cache's check — MuPDF's pixmap rounding differs ~1px, which would look endlessly stale.
// None → size from MuPDF bounds (bench only).
pub fn render_page_pixels(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    page_pt: Option<(f64, f64)>,
) -> Option<PagePixels> {
    with_doc(uri, |doc| {
        // device_bgr + no alpha yields B,G,R samples, matching cairo Rgb24's byte order.
        let colorspace = Colorspace::device_bgr();
        let page = doc.load_page(page_num).ok()?;
        let ctm = Matrix::new_scale((scale * dsf) as f32, (scale * dsf) as f32);
        // show_extras: render annotations/widgets too, matching a full page render.
        let pixmap = page.to_pixmap(&ctm, &colorspace, false, true).ok()?;

        let (pw, ph) = match page_pt {
            Some(size) => size,
            None => {
                let b = page.bounds().ok()?;
                ((b.x1 - b.x0) as f64, (b.y1 - b.y0) as f64)
            }
        };
        let width = ((pw * scale * dsf) as i32).max(1);
        let height = ((ph * scale * dsf) as i32).max(1);
        let (data, stride) = pack_pixmap(&pixmap, width, height)?;
        Some(PagePixels {
            data,
            width,
            height,
            stride,
        })
    })
}

// `render_page_pixels` as an ImageSurface, for callers that draw it (sync paint, thumbnails) or scan
// it (content_bbox).
pub fn render_page_surface(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    page_pt: Option<(f64, f64)>,
) -> Option<ImageSurface> {
    let px = render_page_pixels(uri, page_num, scale, dsf, page_pt)?;
    let surface =
        ImageSurface::create_for_data(px.data, Format::Rgb24, px.width, px.height, px.stride)
            .ok()?;
    surface.set_device_scale(dsf, dsf);
    Some(surface)
}

// Page size in points (width, height), or None.
pub fn page_size(uri: &str, page_num: i32) -> Option<(f64, f64)> {
    with_doc(uri, |doc| {
        let b = doc.load_page(page_num).ok()?.bounds().ok()?;
        Some(((b.x1 - b.x0) as f64, (b.y1 - b.y0) as f64))
    })
}

// Bounding box of the page's non-white content in page-local top-left points, or None for a blank
// page. Used for crop-to-content. MuPDF exposes no ink-bbox device via the Rust binding (and a
// display list's bounds are just its mediabox), so this renders the page small and scans for the
// tightest non-white rect - robust across text, vector and image content.
pub fn content_bbox(uri: &str, page_num: i32) -> Option<(f64, f64, f64, f64)> {
    const SCALE: f64 = 0.2; // 1 sampled pixel = 5pt; crop adds a 5pt margin anyway
    let surface = render_page_surface(uri, page_num, SCALE, 1.0, None)?;
    let (w, h, stride) = (surface.width(), surface.height(), surface.stride() as usize);

    let mut pixels = None;
    surface
        .with_data(|data| pixels = scan_bbox(data, w, h, stride))
        .ok()?;
    let (min_x, min_y, max_x, max_y) = pixels?;
    Some((
        min_x as f64 / SCALE,
        min_y as f64 / SCALE,
        (max_x + 1) as f64 / SCALE,
        (max_y + 1) as f64 / SCALE,
    ))
}

// Tightest pixel bounding box (min_x, min_y, max_x, max_y, inclusive) of non-white content in a
// Rgb24 (BGRx) buffer, or None if every pixel is near-white.
fn scan_bbox(data: &[u8], w: i32, h: i32, stride: usize) -> Option<(i32, i32, i32, i32)> {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, -1, -1);
    for y in 0..h {
        let row = &data[y as usize * stride..];
        for x in 0..w {
            let p = &row[x as usize * 4..];
            let background = p[0] >= 245 && p[1] >= 245 && p[2] >= 245;
            if !background {
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    (max_x >= min_x && max_y >= min_y).then_some((min_x, min_y, max_x, max_y))
}

// Pack a MuPDF BGR pixmap into a Rgb24 (BGRx) buffer of exactly (target_w, target_h) plus its stride.
// The pixmap is within ~1px; copy the overlap, leave padding white so no black seam shows.
fn pack_pixmap(pix: &mupdf::Pixmap, target_w: i32, target_h: i32) -> Option<(Vec<u8>, i32)> {
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
            drow[x * 4] = s[0];
            drow[x * 4 + 1] = s[1];
            drow[x * 4 + 2] = s[2];
        }
    }

    Some((data, dst_stride as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cold (open+repair) vs warm (render) cost, plus a PPM dump to eyeball correctness. Needs a file:
    //   PDF_PATH=/abs/scan.pdf SCALE=0.25 cargo test --release \
    //     mupdf_render::tests::bench -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench() {
        let path = std::env::var("PDF_PATH").expect("PDF_PATH not set");
        let uri = format!("file://{path}");
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

    fn margin_pdf_uri() -> String {
        let dir = std::env::temp_dir().join("scrolex_content_bbox_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("margins.pdf");
        std::fs::write(&path, MARGIN_PDF).unwrap();
        format!("file://{}", path.display())
    }

    #[test]
    fn page_count_and_size_read_the_document() {
        let uri = margin_pdf_uri();
        assert_eq!(stage_candidate(&uri).unwrap().page_count(), 1);
        assert_eq!(page_size(&uri, 0), Some((200.0, 200.0)));
        // out-of-range / unopenable degrade rather than panic
        assert_eq!(page_size(&uri, 99), None);
        // a local uri always stages (the path exists as a value); an unopenable file counts 0
        assert_eq!(
            stage_candidate("file:///no/such/file.pdf")
                .unwrap()
                .page_count(),
            0
        );
    }

    // A 300x200 page with /Rotate 90 (displayed 200x300) and the word "Hello" near the top-left of
    // the unrotated page, for checking rotation-frame consistency across providers.
    const ROTATED_TEXT_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 200] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Rotate 90 /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 34 >>\nstream\nBT /F1 24 Tf 40 150 Td (Hello) Tj ET\nendstream\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    // /Rotate 90 on a 300x200 page must present as 200x300, and every provider (render→content_bbox,
    // and text search) must report in that same rotated display frame so overlays land on the render.
    #[gtk::test]
    fn rotated_page_consistent_across_providers() {
        let dir = std::env::temp_dir().join("scrolex_rot");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rot.pdf");
        std::fs::write(&path, ROTATED_TEXT_PDF).unwrap();
        let uri = format!("file://{}", path.display());

        // rotation applied: displayed dimensions are swapped
        assert_eq!(page_size(&uri, 0), Some((200.0, 300.0)));

        let (cx1, cy1, cx2, cy2) = content_bbox(&uri, 0).expect("content_bbox");
        assert!(
            cx1 >= 0.0 && cy1 >= 0.0 && cx2 <= 200.0 && cy2 <= 300.0,
            "content bbox outside rotated page: {:?}",
            (cx1, cy1, cx2, cy2)
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
            quad.0 < cx2 && quad.2 > cx1 && quad.1 < cy2 && quad.3 > cy1,
            "search hit {quad:?} does not overlap content bbox ({cx1},{cy1},{cx2},{cy2}) - frame mismatch"
        );
    }

    #[test]
    fn scan_bbox_finds_non_white_block() {
        // 10x10 white buffer with a black block at x 3..=6, y 2..=5
        let (w, h) = (10i32, 10i32);
        let stride = (w * 4) as usize;
        let mut data = vec![0xffu8; stride * h as usize];
        for y in 2..=5 {
            for x in 3..=6 {
                let o = y * stride + (x * 4) as usize;
                data[o] = 0;
                data[o + 1] = 0;
                data[o + 2] = 0;
            }
        }
        assert_eq!(scan_bbox(&data, w, h, stride), Some((3, 2, 6, 5)));
    }

    #[test]
    fn scan_bbox_none_when_all_white() {
        let stride = 10 * 4;
        assert_eq!(scan_bbox(&vec![0xffu8; stride * 10], 10, 10, stride), None);
    }

    // Regression guard for the crop bug: content_bbox must trim to the mark, not return the full
    // page. Renders a real page via MuPDF (opened by path), so it also covers the render+scale path.
    #[gtk::test]
    fn content_bbox_trims_to_content_not_full_page() {
        let uri = margin_pdf_uri();
        let (x1, y1, x2, y2) = content_bbox(&uri, 0).expect("content_bbox on a rendered page");
        // strictly inside the 200x200 page - the exact assertion the full-mediabox bug failed
        assert!(
            x1 > 0.0 && y1 > 0.0 && x2 < 200.0 && y2 < 200.0,
            "bbox not trimmed (returned ~full page?): {:?}",
            (x1, y1, x2, y2)
        );
        // roughly the mark: PDF rect (60,50)-(140,150) flips to top-left y (60,50)-(140,150)
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
