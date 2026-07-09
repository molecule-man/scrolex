// Fast render path for image-only (scanned) pages: decode the page's embedded JPEG directly and
// blit it, bypassing poppler's ~1.6s image pipeline. Falls back (returns None) for anything it
// can't confidently handle.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gtk::cairo::{Context, Format, ImageSurface};
use gtk::prelude::FileExt;
use lopdf::content::{Content, Operation};
use once_cell::sync::Lazy;

// One qualifying page: its JPEG's byte range plus the image placement rect (x, y, w, h) in
// top-left page-point space, taken from the page's content-stream matrix.
struct ImageEntry {
    content_start: u64,
    length: usize,
    place: (f64, f64, f64, f64),
    // stream is FlateDecode-wrapped around the JPEG (filter chain [FlateDecode, DCTDecode])
    flate: bool,
}

// Per-document map of qualifying image-only pages (0-based index). Small and immutable once built,
// so it is shared across render threads via Arc.
struct ImageIndex {
    path: PathBuf,
    pages: HashMap<i32, ImageEntry>,
}

enum IndexState {
    // carries a generation so a build superseded by an invalidate/reopen doesn't publish stale data
    Building(u64),
    Ready(Arc<ImageIndex>),
    Failed,
}

static INDICES: Lazy<Mutex<HashMap<String, IndexState>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static GENERATION: AtomicU64 = AtomicU64::new(0);

// Full-page surface for `page` if it is a single-JPEG scan we handle, else None.
pub fn render_image_page(
    uri: &str,
    page: &poppler::Page,
    scale: f64,
    dsf: f64,
) -> Option<ImageSurface> {
    let index = index_for(uri)?;
    let entry = index.pages.get(&page.index())?;

    let raw = read_bytes(&index.path, entry)?;
    let jpeg = if entry.flate { inflate(&raw)? } else { raw };
    if jpeg.get(..2) != Some(&[0xFF, 0xD8]) {
        return None;
    }

    let (page_w, page_h) = page.size();
    let place = flip_y(page_h, entry.place);
    let target_w = (place.2 * scale * dsf)
        .round()
        .clamp(1.0, f64::from(u16::MAX)) as u16;
    let target_h = (place.3 * scale * dsf)
        .round()
        .clamp(1.0, f64::from(u16::MAX)) as u16;
    let img = decode(&jpeg, target_w, target_h)?;

    Some(compose(page_w, page_h, place, &img, scale, dsf))
}

// Convert a placement rect from PDF's bottom-left origin to the top-left render space.
fn flip_y(page_h: f64, place: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (x, y_bottom, w, h) = place;
    (x, page_h - y_bottom - h, w, h)
}

// Start building the index for `uri` (once) so it is ready before rendering saturates the CPU.
// Fire-and-forget.
pub fn prewarm(uri: &str) {
    let _ = index_for(uri);
}

// Drop any cached index for `uri`, so a reopen after the file changed rebuilds against the new
// bytes (offsets are absolute) and a prior indexing failure is retried.
pub fn invalidate(uri: &str) {
    INDICES.lock().unwrap().remove(uri);
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
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    map.insert(uri.to_string(), IndexState::Building(generation));
    drop(map);

    let uri = uri.to_string();
    std::thread::spawn(move || {
        let state = match build_index(&uri) {
            Some(idx) => IndexState::Ready(Arc::new(idx)),
            None => IndexState::Failed,
        };
        // Publish only if this is still the same build; an invalidate/reopen bumps the generation
        // so a superseded build silently discards its (now stale) result.
        let mut map = INDICES.lock().unwrap();
        if matches!(map.get(&uri), Some(IndexState::Building(g)) if *g == generation) {
            map.insert(uri, state);
        }
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

// A page qualifies only if it is upright, unannotated, and its content stream paints nothing but a
// single lone DCTDecode image (invisible OCR text is fine; visible text/vector/overlay is not) with
// a handled colorspace, no mask and no /Decode. Anything else falls back to poppler's render.
fn page_entry(
    doc: &lopdf::Document,
    pid: lopdf::ObjectId,
    file: &mut std::fs::File,
) -> Option<ImageEntry> {
    let page = doc.get_dictionary(pid).ok()?;
    if effective_rotate(doc, page) != 0 || !mediabox_origin_zero(doc, page) {
        return None;
    }
    // Any annotation (stamp, signature, widget, ...) is painted by poppler but not by us.
    if doc.get_page_annotations(pid).is_ok_and(|a| !a.is_empty()) {
        return None;
    }

    let resources = deref_dict(doc, page.get(b"Resources").ok()?)?;
    let xobjects = deref_dict(doc, resources.get(b"XObject").ok()?)?;

    let mut images = xobjects.iter().filter_map(|(name, oref)| {
        let (_, obj) = doc.dereference(oref).ok()?;
        let stream = obj.as_stream().ok()?;
        (stream.dict.get(b"Subtype").ok()?.as_name().ok()? == b"Image")
            .then(|| (name.clone(), stream))
    });
    let (name, stream) = images.next()?;
    if images.next().is_some() {
        return None; // more than one image on the page
    }

    let dict = &stream.dict;
    let flate = dct_flate_wrapped(doc, dict)?;
    if dict.get(b"SMask").is_ok()
        || dict.get(b"Mask").is_ok()
        || dict.get(b"Decode").is_ok()
        || !colorspace_supported(doc, dict)
    {
        return None;
    }

    // Content stream must draw only this image, and give its placement (upright, unrotated).
    let content = Content::decode(&doc.get_page_content(pid).ok()?).ok()?;
    let place = image_placement(&content.operations, &name)?;

    let id = deref_ref(&xobjects, &name)?;
    let offset = match doc.reference_table.get(id.0)? {
        lopdf::xref::XrefEntry::Normal { offset, .. } => *offset as u64,
        _ => return None,
    };
    let content_start = stream_content_start(file, offset)?;
    Some(ImageEntry {
        content_start,
        length: stream.content.len(),
        place,
        flate,
    })
}

// Object id the XObject `name` references.
fn deref_ref(xobjects: &lopdf::Dictionary, name: &[u8]) -> Option<lopdf::ObjectId> {
    xobjects.get(name).ok()?.as_reference().ok()
}

// Effective page rotation in degrees, honoring /Rotate inherited from the page tree (nearest wins).
// Any non-zero value means we can't place the image uprightly, so the page falls back to poppler.
fn effective_rotate(doc: &lopdf::Document, page: &lopdf::Dictionary) -> i64 {
    let mut dict = page.clone();
    for _ in 0..32 {
        if let Ok(rot) = dict.get(b"Rotate") {
            // /Rotate defined here wins (nearest); resolve indirect values. If it can't be read as a
            // number, report non-zero so the page falls back rather than risk unrotated placement.
            return doc
                .dereference(rot)
                .ok()
                .and_then(|(_, o)| o.as_i64().ok())
                .unwrap_or(90);
        }
        let Ok(parent) = dict.get(b"Parent").and_then(lopdf::Object::as_reference) else {
            return 0;
        };
        let Ok(pd) = doc.get_dictionary(parent) else {
            return 0;
        };
        dict = pd.clone();
    }
    0
}

// Whether the page's MediaBox origin is (0,0) (honoring inheritance). Placement is computed in
// default user space against page.size(), so a non-zero origin would offset the image; such pages
// fall back to poppler.
fn mediabox_origin_zero(doc: &lopdf::Document, page: &lopdf::Dictionary) -> bool {
    let mut dict = page.clone();
    for _ in 0..32 {
        if let Ok(mb) = dict.get(b"MediaBox") {
            let Ok((_, lopdf::Object::Array(a))) = doc.dereference(mb) else {
                return false;
            };
            let x0 = a.first().and_then(num).unwrap_or(0.0);
            let y0 = a.get(1).and_then(num).unwrap_or(0.0);
            return x0.abs() < 1.0 && y0.abs() < 1.0;
        }
        let Ok(parent) = dict.get(b"Parent").and_then(lopdf::Object::as_reference) else {
            return false; // MediaBox is required; if we can't find it, don't risk the fast path
        };
        let Ok(pd) = doc.get_dictionary(parent) else {
            return false;
        };
        dict = pd.clone();
    }
    false
}

// Numeric value of an Integer or Real object.
fn num(obj: &lopdf::Object) -> Option<f64> {
    match obj {
        lopdf::Object::Integer(i) => Some(*i as f64),
        lopdf::Object::Real(r) => Some(f64::from(*r)),
        _ => None,
    }
}

// Resolve an object (following one reference) to an owned dictionary.
fn deref_dict(doc: &lopdf::Document, obj: &lopdf::Object) -> Option<lopdf::Dictionary> {
    doc.dereference(obj).ok()?.1.as_dict().ok().cloned()
}

// Whether a DCTDecode image stream is additionally FlateDecode-wrapped (`Some(true)` for the chain
// [FlateDecode, DCTDecode]), a bare JPEG (`Some(false)`), or a filter we can't turn into raw JPEG
// bytes (`None` -> fall back). Scanners commonly Flate-wrap the JPEG, so both must be accepted.
fn dct_flate_wrapped(doc: &lopdf::Document, dict: &lopdf::Dictionary) -> Option<bool> {
    let names: Vec<Vec<u8>> = match doc.dereference(dict.get(b"Filter").ok()?).ok()?.1 {
        lopdf::Object::Name(n) => vec![n.clone()],
        lopdf::Object::Array(a) => {
            let mut v = Vec::with_capacity(a.len());
            for o in a {
                v.push(doc.dereference(o).ok()?.1.as_name().ok()?.to_vec());
            }
            v
        }
        _ => return None,
    };
    match names.len() {
        1 if names[0] == b"DCTDecode" => Some(false),
        2 if names[0] == b"FlateDecode" && names[1] == b"DCTDecode" => Some(true),
        _ => None,
    }
}

// Inflate a zlib/FlateDecode stream to the JPEG bytes it wraps.
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

// Only colorspaces our JPEG decode reproduces faithfully. Device/Cal/ICCBased map straight onto the
// JPEG's own gray/RGB/CMYK components; Indexed, Separation, DeviceN, Lab need PDF-side handling we
// don't do, so those fall back to poppler. Absent (some DCT images omit it) is fine - trust the JPEG.
fn colorspace_supported(doc: &lopdf::Document, dict: &lopdf::Dictionary) -> bool {
    let Ok(cs) = dict.get(b"ColorSpace") else {
        return true;
    };
    let Ok((_, cs)) = doc.dereference(cs) else {
        return false;
    };
    let name = match cs {
        lopdf::Object::Name(n) => n.clone(),
        lopdf::Object::Array(a) => match a.first() {
            Some(lopdf::Object::Name(n)) => n.clone(),
            _ => return false,
        },
        _ => return false,
    };
    matches!(
        name.as_slice(),
        b"DeviceGray"
            | b"DeviceRGB"
            | b"DeviceCMYK"
            | b"CalGray"
            | b"CalRGB"
            | b"ICCBased"
            | b"G"
            | b"RGB"
            | b"CMYK"
    )
}

// Placement (x, y_bottom, w, h) in PDF bottom-left page-point space for a page whose content stream
// draws exactly one image (`Do` of `image`) and nothing else visible, or None otherwise. Invisible
// OCR text (render mode 3) is allowed - poppler paints no marks for it, so output still matches.
// Visible text, vector paint, clipping, shading, inline images, transparency (`gs`) and other
// XObjects all disqualify the page, as does a rotated/skewed/flipped placement.
fn image_placement(ops: &[Operation], image: &[u8]) -> Option<(f64, f64, f64, f64)> {
    const ID: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut ctm = ID;
    let mut tr = 0i64; // text render mode; 3 = invisible
    let mut stack: Vec<([f64; 6], i64)> = Vec::new();
    let mut placed: Option<[f64; 6]> = None;

    for op in ops {
        match op.operator.as_str() {
            "q" => stack.push((ctm, tr)),
            "Q" => (ctm, tr) = stack.pop().unwrap_or((ID, 0)),
            "cm" => ctm = concat(matrix(&op.operands)?, ctm),
            "Tr" => tr = op.operands.first().and_then(num).unwrap_or(0.0) as i64,
            "Do" => {
                if placed.is_some() || op.operands.first()?.as_name().ok()? != image {
                    return None; // second Do, or a different (form) XObject
                }
                placed = Some(ctm);
            }
            // Text shows marks unless it is invisible (render mode 3).
            "Tj" | "TJ" | "'" | "\"" if tr != 3 => return None,
            // Anything that paints, clips or changes compositing breaks output parity with poppler.
            "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "W" | "W*" | "sh" | "BI"
            | "gs" => return None,
            _ => {}
        }
    }

    let m = placed?;
    if m[1].abs() > 0.01 || m[2].abs() > 0.01 || m[0] <= 0.0 || m[3] <= 0.0 {
        return None; // rotated, skewed, or flipped placement
    }
    Some((m[4], m[5], m[0], m[3])) // e, f, w, h
}

// 2x3 affine [a b c d e f] from the operands of a `cm`.
fn matrix(ops: &[lopdf::Object]) -> Option<[f64; 6]> {
    let mut m = [0.0; 6];
    if ops.len() != 6 {
        return None;
    }
    for (i, o) in ops.iter().enumerate() {
        m[i] = num(o)?;
    }
    Some(m)
}

// PDF matrix concatenation: `a` applied first, then `b` (CTM' = a x b).
fn concat(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
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

// Paint the decoded image into its placement rect on a full-page canvas surface, matching
// render_surface's dimensions and coordinate space. `place` is (x, y, w, h) in top-left points.
fn compose(
    page_w: f64,
    page_h: f64,
    place: (f64, f64, f64, f64),
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
    cr.translate(place.0, place.1);
    cr.scale(place.2 / img.width as f64, place.3 / img.height as f64);
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
    fn renders_ocr_scan_page() {
        // A full-page image with an invisible OCR text layer must still take the fast path.
        let uri = fixture_uri("image_scan_ocr.pdf");
        let doc = poppler::Document::from_file(&uri, None).unwrap();
        let page = doc.page(0).unwrap();
        let (w, h) = page.size();

        let surface =
            render_when_ready(&uri, &page).expect("fast path should accept an OCR'd scan");
        assert_eq!((surface.width(), surface.height()), (w as i32, h as i32));
    }

    #[gtk::test]
    fn renders_flate_wrapped_jpeg() {
        // Real OCR scans often wrap the JPEG in FlateDecode ([FlateDecode, DCTDecode]); the fast
        // path must inflate first and still render it.
        let uri = fixture_uri("image_scan_flate.pdf");
        let doc = poppler::Document::from_file(&uri, None).unwrap();
        let page = doc.page(0).unwrap();
        let (w, h) = page.size();

        let surface = render_when_ready(&uri, &page).expect("fast path should accept Flate+DCT");
        assert_eq!((surface.width(), surface.height()), (w as i32, h as i32));

        // Non-white pixels prove inflate produced a real JPEG (not a stream that decoded to blank).
        let mut colored = false;
        surface
            .with_data(|d| {
                colored = d
                    .chunks_exact(4)
                    .any(|p| p[0] != 255 || p[1] != 255 || p[2] != 255);
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

    fn op(operator: &str, operands: Vec<lopdf::Object>) -> Operation {
        Operation {
            operator: operator.into(),
            operands,
        }
    }

    fn cm(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Operation {
        op(
            "cm",
            vec![a.into(), b.into(), c.into(), d.into(), e.into(), f.into()],
        )
    }

    fn do_(name: &str) -> Operation {
        op("Do", vec![lopdf::Object::Name(name.into())])
    }

    fn tr(mode: i64) -> Operation {
        op("Tr", vec![mode.into()])
    }

    fn tj() -> Operation {
        op("Tj", vec![lopdf::Object::string_literal("text")])
    }

    #[test]
    fn placement_full_page_image() {
        let ops = [
            op("q", vec![]),
            cm(48.0, 0.0, 0.0, 36.0, 0.0, 0.0),
            do_("Im0"),
            op("Q", vec![]),
        ];
        assert_eq!(image_placement(&ops, b"Im0"), Some((0.0, 0.0, 48.0, 36.0)));
    }

    #[test]
    fn placement_partial_image() {
        let ops = [cm(10.0, 0.0, 0.0, 20.0, 5.0, 6.0), do_("Im0")];
        assert_eq!(image_placement(&ops, b"Im0"), Some((5.0, 6.0, 10.0, 20.0)));
    }

    #[test]
    fn flip_y_maps_pdf_bottom_left_to_top_left() {
        // image 10x20 at PDF (5,6) on a 36-tall page => top-left y = 36 - 6 - 20 = 10.
        assert_eq!(
            flip_y(36.0, (5.0, 6.0, 10.0, 20.0)),
            (5.0, 10.0, 10.0, 20.0)
        );
        // a full-page image is flip-invariant.
        assert_eq!(flip_y(36.0, (0.0, 0.0, 48.0, 36.0)), (0.0, 0.0, 48.0, 36.0));
    }

    #[test]
    fn accepts_invisible_ocr_text() {
        // full-page image plus an invisible (render mode 3) OCR text layer: still image-only visually.
        let ops = [
            cm(48.0, 0.0, 0.0, 36.0, 0.0, 0.0),
            do_("Im0"),
            op("BT", vec![]),
            tr(3),
            tj(),
            op("ET", vec![]),
        ];
        assert_eq!(image_placement(&ops, b"Im0"), Some((0.0, 0.0, 48.0, 36.0)));
    }

    #[test]
    fn placement_rejects_marks_clip_and_transparency() {
        let base = cm(48.0, 0.0, 0.0, 36.0, 0.0, 0.0);
        let reject = |extra: Operation| {
            image_placement(&[base.clone(), extra, do_("Im0")], b"Im0").is_none()
        };
        assert!(reject(tj())); // visible text (default render mode 0)
        assert!(image_placement(
            &[base.clone(), tr(3), tj(), tr(0), tj(), do_("Im0")],
            b"Im0"
        )
        .is_none()); // becomes visible again
        assert!(reject(op("f", vec![]))); // filled path
        assert!(reject(op("W", vec![]))); // clipping
        assert!(reject(op("gs", vec![lopdf::Object::Name(b"GS1".into())]))); // transparency/blend
        assert!(reject(op("sh", vec![lopdf::Object::Name(b"Sh1".into())]))); // shading
                                                                             // a second image draw, and a draw of a different XObject
        assert!(image_placement(&[base.clone(), do_("Im0"), do_("Im0")], b"Im0").is_none());
        assert!(image_placement(&[base.clone(), do_("Fm0")], b"Im0").is_none());
        // rotated placement
        assert!(
            image_placement(&[cm(0.0, 48.0, -36.0, 0.0, 0.0, 0.0), do_("Im0")], b"Im0").is_none()
        );
    }

    #[test]
    fn filter_chain_acceptance() {
        let dict = |f: lopdf::Object| {
            let mut d = lopdf::Dictionary::new();
            d.set("Filter", f);
            d
        };
        let name = |s: &[u8]| lopdf::Object::Name(s.to_vec());
        let arr = |v: Vec<&[u8]>| lopdf::Object::Array(v.into_iter().map(name).collect());
        let doc = lopdf::Document::new();
        let w = |f: lopdf::Object| dct_flate_wrapped(&doc, &dict(f));

        assert_eq!(w(name(b"DCTDecode")), Some(false));
        assert_eq!(w(arr(vec![b"DCTDecode"])), Some(false));
        assert_eq!(w(arr(vec![b"FlateDecode", b"DCTDecode"])), Some(true));
        // unsupported chains fall back
        assert_eq!(w(name(b"FlateDecode")), None);
        assert_eq!(w(arr(vec![b"JPXDecode"])), None);
        assert_eq!(w(arr(vec![b"DCTDecode", b"FlateDecode"])), None);
    }

    #[test]
    fn colorspace_whitelist() {
        let sup = |cs: lopdf::Object| {
            let mut d = lopdf::Dictionary::new();
            d.set("ColorSpace", cs);
            colorspace_supported(&lopdf::Document::new(), &d)
        };
        assert!(sup(lopdf::Object::Name(b"DeviceRGB".to_vec())));
        assert!(sup(lopdf::Object::Array(vec![lopdf::Object::Name(
            b"ICCBased".to_vec()
        )])));
        assert!(!sup(lopdf::Object::Name(b"Separation".to_vec())));
        assert!(!sup(lopdf::Object::Array(vec![lopdf::Object::Name(
            b"Indexed".to_vec()
        )])));
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
