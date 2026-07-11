// Rasterize PDF pages with MuPDF, which downscale-decodes embedded images (JPEG/JPEG2000) to the
// requested resolution - scanned pages render at fit-to-page cost, not poppler's full-res decode.

use std::cell::RefCell;

use gtk::cairo::{Format, ImageSurface};
use gtk::prelude::FileExt;
use mupdf::{Colorspace, Document, Matrix};

thread_local! {
    // One Document per thread: it's bound to the thread's fz_context, so it can't cross threads.
    // Reused across renders, reopened only when the uri changes.
    static DOC: RefCell<Option<(String, Document)>> = const { RefCell::new(None) };
}

// Full-page surface for `page_num` at `scale` * `dsf`, or None if MuPDF can't open/render it (caller
// falls back to poppler). `page_pt` (poppler's page.size()) sizes the surface at exactly
// (page_pt * scale * dsf) truncated so it matches the render cache's dimension check; MuPDF's own
// pixmap rounding differs by ~1px, which would make every render look stale (endless re-render).
// None derives the size from MuPDF's bounds (bench only).
pub fn render_page_surface(
    uri: &str,
    page_num: i32,
    scale: f64,
    dsf: f64,
    page_pt: Option<(f64, f64)>,
) -> Option<ImageSurface> {
    // Touch MuPDF's TLS fz_context before our DOC thread-local so its destructor registers first and
    // runs last: our Document's Drop needs a live context, else it aborts ("thread local panicked on
    // drop") when a pool worker exits. device_bgr + no alpha yields B,G,R, matching cairo Rgb24.
    let colorspace = Colorspace::device_bgr();

    DOC.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|(u, _)| u.as_str()) != Some(uri) {
            let path = gtk::gio::File::for_uri(uri).path()?;
            let doc = Document::open(path.as_path()).ok()?;
            *slot = Some((uri.to_string(), doc));
        }
        let (_, doc) = slot.as_ref().unwrap();

        let page = doc.load_page(page_num).ok()?;
        let ctm = Matrix::new_scale((scale * dsf) as f32, (scale * dsf) as f32);
        // annotations/widgets on, to match poppler's full render.
        let pixmap = page.to_pixmap(&ctm, &colorspace, false, true).ok()?;

        let (pw, ph) = match page_pt {
            Some(size) => size,
            None => {
                let b = page.bounds().ok()?;
                ((b.x1 - b.x0) as f64, (b.y1 - b.y0) as f64)
            }
        };
        let target_w = ((pw * scale * dsf) as i32).max(1);
        let target_h = ((ph * scale * dsf) as i32).max(1);
        surface_from_pixmap(&pixmap, dsf, target_w, target_h)
    })
}

// Pack a MuPDF BGR pixmap into a cairo Rgb24 (BGRx) surface of exactly (target_w, target_h). The
// pixmap is within ~1px of the target; copy the overlap, leave padding white so no black seam shows.
fn surface_from_pixmap(pix: &mupdf::Pixmap, dsf: f64, target_w: i32, target_h: i32) -> Option<ImageSurface> {
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

    let surface =
        ImageSurface::create_for_data(data, Format::Rgb24, target_w, target_h, dst_stride as i32).ok()?;
    surface.set_device_scale(dsf, dsf);
    Some(surface)
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
        let scale: f64 = std::env::var("SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(0.25);

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
