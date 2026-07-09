// Fast render path for image-only (scanned) pages: decode the page's embedded JPEG directly and
// blit it, bypassing poppler's ~1.6s image pipeline. Falls back (returns None) for anything it
// can't confidently handle.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gtk::cairo::{Context, Format, ImageSurface};
use gtk::prelude::FileExt;
use once_cell::sync::Lazy;

// Content byte range of one page's JPEG stream in the file.
struct ImageEntry {
    content_start: u64,
    length: usize,
}

// Per-document map of qualifying image-only pages (0-based index). Small and immutable once built,
// so it is shared across render threads via Arc.
struct ImageIndex {
    path: PathBuf,
    pages: HashMap<i32, ImageEntry>,
}

enum IndexState {
    Building,
    Ready(Arc<ImageIndex>),
    Failed,
}

static INDICES: Lazy<Mutex<HashMap<String, IndexState>>> = Lazy::new(|| Mutex::new(HashMap::new()));

// Full-page surface for `page` if it is a single-JPEG scan we handle, else None.
pub fn render_image_page(
    uri: &str,
    page: &poppler::Page,
    scale: f64,
    dsf: f64,
) -> Option<ImageSurface> {
    let index = index_for(uri)?;
    let entry = index.pages.get(&page.index())?;
    let area = single_image_area(page)?;

    let jpeg = read_bytes(&index.path, entry)?;
    if jpeg.get(..2) != Some(&[0xFF, 0xD8]) {
        return None;
    }

    let (page_w, page_h) = page.size();
    let (aw, ah) = (area.2 - area.0, area.3 - area.1);
    let target_w = (aw * scale * dsf).round().max(1.0) as u16;
    let target_h = (ah * scale * dsf).round().max(1.0) as u16;
    let img = decode(&jpeg, target_w, target_h)?;

    // Aspect transposed vs placement => page rotation we don't apply; fall back.
    if transposed(aw, ah, img.width as f64, img.height as f64) {
        return None;
    }

    Some(compose(page_w, page_h, area, &img, scale, dsf))
}

// Start building the index for `uri` (once) so it is ready before rendering saturates the CPU.
// Fire-and-forget.
pub fn prewarm(uri: &str) {
    let _ = index_for(uri);
}

// Lazily build the index once per uri on a background thread. Returns None while building or if the
// build failed, so callers fall back to poppler meanwhile.
fn index_for(uri: &str) -> Option<Arc<ImageIndex>> {
    let mut map = INDICES.lock().unwrap();
    match map.get(uri) {
        Some(IndexState::Ready(idx)) => return Some(idx.clone()),
        Some(_) => return None,
        None => {}
    }
    map.insert(uri.to_string(), IndexState::Building);
    drop(map);

    let uri = uri.to_string();
    std::thread::spawn(move || {
        let state = match build_index(&uri) {
            Some(idx) => IndexState::Ready(Arc::new(idx)),
            None => IndexState::Failed,
        };
        INDICES.lock().unwrap().insert(uri, state);
    });
    None
}

// Parse the file with lopdf (recovers broken xrefs), record the JPEG byte range of each qualifying
// page, then drop the parsed document to stay memory-lean.
fn build_index(uri: &str) -> Option<ImageIndex> {
    let path = gtk::gio::File::for_uri(uri).path()?;
    let doc = lopdf::Document::load(&path).ok()?;

    let mut file = std::fs::File::open(&path).ok()?;
    let mut pages = HashMap::new();
    for (pnum, pid) in doc.get_pages() {
        if let Some(entry) = page_entry(&doc, pid, &mut file) {
            pages.insert(pnum as i32 - 1, entry);
        }
    }
    Some(ImageIndex { path, pages })
}

// A page qualifies if it is upright and its only XObject is a lone DCTDecode image with no mask, no
// /Decode and a non-indexed colorspace.
fn page_entry(
    doc: &lopdf::Document,
    pid: lopdf::ObjectId,
    file: &mut std::fs::File,
) -> Option<ImageEntry> {
    let page = doc.get_dictionary(pid).ok()?;
    if page
        .get(b"Rotate")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0)
        != 0
    {
        return None;
    }

    let resources = deref_dict(doc, page.get(b"Resources").ok()?)?;
    let xobjects = deref_dict(doc, resources.get(b"XObject").ok()?)?;

    let mut images = xobjects.iter().filter_map(|(_, oref)| {
        let (_, obj) = doc.dereference(oref).ok()?;
        let stream = obj.as_stream().ok()?;
        (stream.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image").then_some((oref, stream))
    });
    let (oref, stream) = images.next()?;
    if images.next().is_some() {
        return None; // more than one image on the page
    }

    let dict = &stream.dict;
    if !filter_is_dct(dict)
        || dict.get(b"SMask").is_ok()
        || dict.get(b"Mask").is_ok()
        || dict.get(b"Decode").is_ok()
    {
        return None;
    }
    if colorspace_is_indexed(doc, dict) {
        return None;
    }

    let id = oref.as_reference().ok()?;
    let offset = match doc.reference_table.get(id.0)? {
        lopdf::xref::XrefEntry::Normal { offset, .. } => *offset as u64,
        _ => return None,
    };
    let content_start = stream_content_start(file, offset)?;
    Some(ImageEntry {
        content_start,
        length: stream.content.len(),
    })
}

// Resolve an object (following one reference) to an owned dictionary.
fn deref_dict(doc: &lopdf::Document, obj: &lopdf::Object) -> Option<lopdf::Dictionary> {
    doc.dereference(obj).ok()?.1.as_dict().ok().cloned()
}

fn filter_is_dct(dict: &lopdf::Dictionary) -> bool {
    match dict.get(b"Filter") {
        Ok(lopdf::Object::Name(n)) => n == b"DCTDecode",
        Ok(lopdf::Object::Array(a)) => {
            a.len() == 1 && matches!(&a[0], lopdf::Object::Name(n) if n == b"DCTDecode")
        }
        _ => false,
    }
}

fn colorspace_is_indexed(doc: &lopdf::Document, dict: &lopdf::Dictionary) -> bool {
    let Ok(cs) = dict.get(b"ColorSpace") else {
        return false;
    };
    let Ok((_, cs)) = doc.dereference(cs) else {
        return false;
    };
    match cs {
        lopdf::Object::Name(n) => n == b"Indexed" || n == b"I",
        lopdf::Object::Array(a) => {
            matches!(a.first(), Some(lopdf::Object::Name(n)) if n == b"Indexed" || n == b"I")
        }
        _ => false,
    }
}

// Byte offset of a stream's content: scan from the object start for the `stream` keyword and skip
// its trailing EOL.
fn stream_content_start(file: &mut std::fs::File, object_offset: u64) -> Option<u64> {
    file.seek(SeekFrom::Start(object_offset)).ok()?;
    let mut head = [0u8; 4096];
    let n = file.read(&mut head).ok()?;
    let head = &head[..n];
    let kw = b"stream";
    let pos = head.windows(kw.len()).position(|w| w == kw)?;
    let after = pos + kw.len();
    let skip = match head.get(after)? {
        b'\r' => 2, // CRLF
        b'\n' => 1,
        _ => return None,
    };
    Some(object_offset + (after + skip) as u64)
}

fn read_bytes(path: &PathBuf, entry: &ImageEntry) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(entry.content_start)).ok()?;
    let mut buf = vec![0u8; entry.length];
    file.read_exact(&mut buf).ok()?;
    Some(buf)
}

struct Decoded {
    width: u16,
    height: u16,
    rgb: Vec<u8>, // 3 bytes/px
}

// Decode the JPEG, DCT-downscaled toward the target size, as interleaved RGB.
fn decode(jpeg: &[u8], target_w: u16, target_h: u16) -> Option<Decoded> {
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
    dec.read_info().ok()?;
    dec.scale(target_w, target_h).ok()?;
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => gray_to_rgb(&pixels),
        jpeg_decoder::PixelFormat::CMYK32 => cmyk_to_rgb(&pixels),
        jpeg_decoder::PixelFormat::L16 => return None,
    };
    Some(Decoded {
        width: info.width,
        height: info.height,
        rgb,
    })
}

fn gray_to_rgb(g: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(g.len() * 3);
    for &v in g {
        out.extend_from_slice(&[v, v, v]);
    }
    out
}

// Adobe JPEGs store inverted CMYK; jpeg-decoder returns those raw values.
fn cmyk_to_rgb(cmyk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(cmyk.len() / 4 * 3);
    for px in cmyk.chunks_exact(4) {
        let (c, m, y, k) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        out.extend_from_slice(&[
            (c * k / 255) as u8,
            (m * k / 255) as u8,
            (y * k / 255) as u8,
        ]);
    }
    out
}

// True when one of the rectangles is landscape and the other portrait.
fn transposed(aw: f64, ah: f64, iw: f64, ih: f64) -> bool {
    (aw >= ah) != (iw >= ih)
}

// Paint the decoded image onto a full-page canvas surface, matching render_surface's dimensions and
// coordinate space.
fn compose(
    page_w: f64,
    page_h: f64,
    area: (f64, f64, f64, f64),
    img: &Decoded,
    scale: f64,
    dsf: f64,
) -> ImageSurface {
    let cw = (page_w * scale * dsf) as i32;
    let ch = (page_h * scale * dsf) as i32;
    let surface = ImageSurface::create(Format::Rgb24, cw, ch).expect("surface");
    surface.set_device_scale(dsf, dsf);

    let cr = Context::new(&surface).expect("context");
    cr.scale(scale, scale);
    cr.rectangle(0.0, 0.0, page_w, page_h);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.fill().expect("fill");

    let src = source_surface(img);
    cr.translate(area.0, area.1);
    cr.scale(
        (area.2 - area.0) / img.width as f64,
        (area.3 - area.1) / img.height as f64,
    );
    cr.set_source_surface(&src, 0.0, 0.0).expect("source");
    cr.paint().expect("paint");
    cr.set_source_rgb(0.0, 0.0, 0.0);
    drop(cr);
    surface
}

// Pack interleaved RGB into a cairo Rgb24 (BGRx) source surface.
fn source_surface(img: &Decoded) -> ImageSurface {
    let (w, h) = (img.width as i32, img.height as i32);
    let stride = Format::Rgb24
        .stride_for_width(img.width as u32)
        .expect("stride");
    let mut data = vec![0u8; (stride * h) as usize];
    for y in 0..h as usize {
        let row = &img.rgb[y * w as usize * 3..];
        let out = &mut data[y * stride as usize..];
        for x in 0..w as usize {
            let (r, g, b) = (row[x * 3], row[x * 3 + 1], row[x * 3 + 2]);
            out[x * 4] = b;
            out[x * 4 + 1] = g;
            out[x * 4 + 2] = r;
        }
    }
    ImageSurface::create_for_data(data, Format::Rgb24, w, h, stride).expect("src surface")
}

// The page's single image placement rect (x1,y1,x2,y2) in page points, or None unless poppler sees
// exactly one image (matches the index's single-image rule).
fn single_image_area(page: &poppler::Page) -> Option<(f64, f64, f64, f64)> {
    let mappings = page.image_mapping();
    if mappings.len() != 1 {
        return None;
    }
    let raw = mappings[0].as_ptr();
    let area = unsafe { (*raw).area };
    Some((area.x1, area.y1, area.x2, area.y2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_uri(name: &str) -> String {
        format!(
            "file://{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    // Poll the lazily-built index until the fast path resolves (or give up).
    fn render_when_ready(uri: &str, page: &poppler::Page) -> Option<ImageSurface> {
        for _ in 0..300 {
            if let Some(s) = render_image_page(uri, page, 1.0, 1.0) {
                return Some(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    #[gtk::test]
    fn renders_image_only_page_as_surface() {
        let uri = fixture_uri("image_scan.pdf");
        let doc = poppler::Document::from_file(&uri, None).unwrap();
        let page = doc.page(0).unwrap();
        let (w, h) = page.size();

        let surface = render_when_ready(&uri, &page).expect("fast path should render the scan");
        assert_eq!((surface.width(), surface.height()), (w as i32, h as i32));

        // The gradient image must actually be painted (not a blank white canvas).
        let mut colored = false;
        surface
            .with_data(|d| {
                colored = d
                    .chunks_exact(4)
                    .any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255)
            })
            .unwrap();
        assert!(colored, "surface is blank white");
    }

    #[gtk::test]
    fn falls_back_for_non_image_page() {
        let uri = fixture_uri("outline.pdf");
        let doc = poppler::Document::from_file(&uri, None).unwrap();
        let page = doc.page(0).unwrap();
        // None both while building and, once ready, because the page has no image.
        for _ in 0..30 {
            assert!(render_image_page(&uri, &page, 1.0, 1.0).is_none());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // Fast-path vs poppler on a real scan. Ignored (needs a file):
    //   PDF_PATH=/abs/scan.pdf cargo test --release image_page::tests::bench -- --ignored --nocapture
    #[gtk::test]
    #[ignore]
    fn bench_fast_path_vs_poppler() {
        let path = std::env::var("PDF_PATH").expect("PDF_PATH not set");
        let uri = format!("file://{path}");
        let doc = poppler::Document::from_file(&uri, None).unwrap();
        let page = doc.page(0).unwrap();

        let surface = render_when_ready(&uri, &page).expect("expected an image page");

        let t = std::time::Instant::now();
        let _ = render_image_page(&uri, &page, 1.0, 1.0).unwrap();
        let fast = t.elapsed();

        let t = std::time::Instant::now();
        let _ = crate::page::render_surface(&page, 1.0, 1.0);
        let poppler = t.elapsed();

        println!(
            "page 0: {}x{} | fast path {fast:?} | poppler {poppler:?} | speedup {:.1}x",
            surface.width(),
            surface.height(),
            poppler.as_secs_f64() / fast.as_secs_f64()
        );
    }

    #[test]
    fn gray_expands_to_rgb() {
        assert_eq!(
            gray_to_rgb(&[0, 128, 255]),
            vec![0, 0, 0, 128, 128, 128, 255, 255, 255]
        );
    }

    #[test]
    fn cmyk_inverted_black_is_black() {
        // Adobe-inverted CMYK: k=0 => black.
        assert_eq!(cmyk_to_rgb(&[255, 255, 255, 0]), vec![0, 0, 0]);
        // k=255, no ink => white.
        assert_eq!(cmyk_to_rgb(&[255, 255, 255, 255]), vec![255, 255, 255]);
    }
}
