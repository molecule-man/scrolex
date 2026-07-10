// Split an image-scan page that also carries visible marks (OCR text, annotations): build a
// single-page PDF holding everything except the background image's paint op, so poppler renders
// the cheap marks while we blit the expensive image ourselves. PoC for scrolex issue #5.

use std::collections::{HashMap, HashSet};

use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};

// Single-page PDF bytes for `page_id` with its image XObjects' `Do` ops removed, or None if the
// page has no image to strip or can't be rebuilt. Poppler renders these (text, annotations) on top
// of the image we blit ourselves.
pub fn overlay_pdf(src: &Document, page_id: ObjectId) -> Option<Vec<u8>> {
    let resources = effective_resources(src, page_id)?;
    let image_names = image_xobject_names(src, &resources);
    if image_names.is_empty() {
        return None;
    }

    let content = strip_image_ops(&src.get_page_content(page_id).ok()?, &image_names)?;
    let media_box = effective_media_box(src, page_id)?;

    let mut out = Document::with_version("1.5");
    let mut map = HashMap::new();
    let mut broken = false;

    let resources = strip_xobjects(src, &resources, &image_names);
    let resources = copy(src, &mut out, &resources, &mut map, &mut broken);
    let annots = copy_annots(src, &mut out, page_id, &mut map, &mut broken);
    // A dangling reference means we'd render the page with a mark's font/appearance missing; fall
    // back to full poppler instead of silently dropping it.
    if broken {
        return None;
    }
    let content_id = out.add_object(Stream::new(Dictionary::new(), content));

    let pages_id = out.new_object_id();
    let mut page = Dictionary::new();
    page.set("Type", "Page");
    page.set("Parent", pages_id);
    page.set("MediaBox", media_box);
    page.set("Resources", resources);
    page.set("Contents", content_id);
    if let Some(annots) = annots {
        page.set("Annots", annots);
    }
    let page_id = out.add_object(page);

    let mut pages = Dictionary::new();
    pages.set("Type", "Pages");
    pages.set("Kids", vec![Object::Reference(page_id)]);
    pages.set("Count", 1);
    out.set_object(pages_id, pages);

    let mut catalog = Dictionary::new();
    catalog.set("Type", "Catalog");
    catalog.set("Pages", pages_id);
    let catalog_id = out.add_object(catalog);
    out.trailer.set("Root", catalog_id);

    let mut buf = Vec::new();
    out.save_to(&mut buf).ok()?;
    Some(buf)
}

// Deep-copy an object graph from `src` into `dst`, remapping ids so referenced fonts/ExtGState/AP
// streams come along. A missing reference collapses to Null and sets `broken`.
fn copy(src: &Document, dst: &mut Document, obj: &Object, map: &mut HashMap<ObjectId, ObjectId>, broken: &mut bool) -> Object {
    match obj {
        Object::Reference(id) => {
            if let Some(new_id) = map.get(id) {
                return Object::Reference(*new_id);
            }
            let new_id = dst.new_object_id();
            map.insert(*id, new_id);
            let copied = match src.get_object(*id) {
                Ok(o) => copy(src, dst, &o.clone(), map, broken),
                Err(_) => {
                    *broken = true;
                    Object::Null
                }
            };
            dst.set_object(new_id, copied);
            Object::Reference(new_id)
        }
        Object::Array(items) => Object::Array(items.iter().map(|o| copy(src, dst, o, map, broken)).collect()),
        Object::Dictionary(dict) => Object::Dictionary(copy_dict(src, dst, dict, map, broken)),
        Object::Stream(stream) => {
            let dict = copy_dict(src, dst, &stream.dict, map, broken);
            let mut copied = Stream::new(dict, stream.content.clone());
            copied.allows_compression = stream.allows_compression;
            Object::Stream(copied)
        }
        other => other.clone(),
    }
}

fn copy_dict(src: &Document, dst: &mut Document, dict: &Dictionary, map: &mut HashMap<ObjectId, ObjectId>, broken: &mut bool) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in dict.iter() {
        out.set(key.clone(), copy(src, dst, value, map, broken));
    }
    out
}

// Rebuild the annotation array, dropping the back-references (/P, /Popup, /IRT, /Parent) that would
// otherwise drag the original page (and its image), or the AcroForm tree, into the copy.
fn copy_annots(src: &Document, dst: &mut Document, page_id: ObjectId, map: &mut HashMap<ObjectId, ObjectId>, broken: &mut bool) -> Option<Object> {
    let annots = src.get_page_annotations(page_id).ok()?;
    if annots.is_empty() {
        return None;
    }
    let mut refs = Vec::new();
    for annot in annots {
        let mut dict = Dictionary::new();
        for (key, value) in annot.iter() {
            if !matches!(key.as_slice(), b"P" | b"Popup" | b"IRT" | b"Parent") {
                dict.set(key.clone(), value.clone());
            }
        }
        let copied = copy(src, dst, &Object::Dictionary(dict), map, broken);
        refs.push(Object::Reference(dst.add_object(copied)));
    }
    Some(Object::Array(refs))
}

// The page's own /Resources, else the nearest inherited one.
fn effective_resources(src: &Document, page_id: ObjectId) -> Option<Dictionary> {
    let (own, inherited) = src.get_page_resources(page_id).ok()?;
    if let Some(dict) = own {
        return Some(dict.clone());
    }
    inherited.into_iter().find_map(|id| src.get_dictionary(id).ok().cloned())
}

fn effective_media_box(src: &Document, page_id: ObjectId) -> Option<Object> {
    let mut dict = src.get_dictionary(page_id).ok()?.clone();
    for _ in 0..32 {
        if let Ok(mb) = dict.get(b"MediaBox") {
            return Some(src.dereference(mb).ok()?.1.clone());
        }
        let parent = dict.get(b"Parent").ok()?.as_reference().ok()?;
        dict = src.get_dictionary(parent).ok()?.clone();
    }
    None
}

// Names in /Resources /XObject whose stream is an image.
fn image_xobject_names(src: &Document, resources: &Dictionary) -> HashSet<Vec<u8>> {
    let mut names = HashSet::new();
    let Ok(xobjects) = resources.get(b"XObject").and_then(|o| src.dereference(o)).map(|(_, o)| o) else {
        return names;
    };
    let Ok(xobjects) = xobjects.as_dict() else {
        return names;
    };
    for (name, oref) in xobjects.iter() {
        if let Ok((_, Object::Stream(stream))) = src.dereference(oref) {
            if stream.dict.get(b"Subtype").ok().and_then(|o| o.as_name().ok()).is_some_and(|n| n == b"Image") {
                names.insert(name.clone());
            }
        }
    }
    names
}

// Clone `resources`, dropping the named images from its /XObject dictionary.
fn strip_xobjects(src: &Document, resources: &Dictionary, images: &HashSet<Vec<u8>>) -> Object {
    let mut out = resources.clone();
    if let Ok((_, Object::Dictionary(xobjects))) = resources.get(b"XObject").and_then(|o| src.dereference(o)) {
        let kept = xobjects
            .iter()
            .filter(|(name, _)| !images.contains(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let mut dict = Dictionary::new();
        for (name, value) in kept {
            dict.set(name, value);
        }
        out.set("XObject", dict);
    }
    Object::Dictionary(out)
}

// Drop every `Do` that paints one of `images`, keeping all other operators (text, annotations'
// content stays untouched).
fn strip_image_ops(content: &[u8], images: &HashSet<Vec<u8>>) -> Option<Vec<u8>> {
    let mut content = Content::decode(content).ok()?;
    content.operations.retain(|op| {
        if op.operator == "Do" {
            if let Some(Object::Name(name)) = op.operands.first() {
                return !images.contains(name);
            }
        }
        true
    });
    content.encode().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::cairo::{Context, Format, ImageSurface};

    // Strip + overlay a real page and compare cost against a full poppler render. Ignored (needs a
    // file):
    //   PDF_PATH=/abs/scan.pdf PAGE=0 SCALE=2 cargo test --release \
    //     page_overlay::tests::bench_overlay -- --ignored --nocapture
    #[gtk::test]
    #[ignore]
    fn bench_overlay() {
        let path = std::env::var("PDF_PATH").expect("PDF_PATH not set");
        let page_index: usize = std::env::var("PAGE").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        let scale: f64 = std::env::var("SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(2.0);
        let uri = format!("file://{path}");

        let doc = poppler::Document::from_file(&uri, None).unwrap();
        let orig = doc.page(page_index as i32).unwrap();

        let t = std::time::Instant::now();
        let _ = crate::page::render_surface(&orig, scale, 1.0);
        let full = t.elapsed();

        let src = Document::load(&path).unwrap();
        let (_, page_id) = src.get_pages().into_iter().nth(page_index).unwrap();
        let t = std::time::Instant::now();
        let Some(bytes) = overlay_pdf(&src, page_id) else {
            println!("page {page_index} has no image to strip; full poppler render is the path ({full:?})");
            return;
        };
        let build = t.elapsed();

        let overlay_doc = poppler::Document::from_data(&bytes, None).unwrap();
        let overlay = overlay_doc.page(0).unwrap();

        let t = std::time::Instant::now();
        let _ = render_overlay(&overlay, scale);
        let overlay_render = t.elapsed();

        // End-to-end for a fast-path-eligible image: blit the image, then draw marks on top. The
        // image can come from an eligible sibling (IMAGE_PDF) since the current fast path still
        // rejects the visible-text/annotation page we are overlaying.
        let image_uri = std::env::var("IMAGE_PDF").map(|p| format!("file://{p}")).unwrap_or_else(|_| uri.clone());
        let image_doc = poppler::Document::from_file(&image_uri, None).unwrap();
        let image_page = image_doc.page(page_index as i32).unwrap();
        let image = wait_for_image(&image_uri, &image_page, scale);
        if let Some(surface) = &image {
            let cr = Context::new(surface).unwrap();
            cr.scale(scale, scale);
            overlay.render(&cr);
            drop(cr);
            let out = std::env::temp_dir().join("overlay_poc.ppm");
            dump_ppm(surface, out.to_str().unwrap());
            println!("wrote composited {}", out.display());
        }

        println!(
            "page {page_index} @ {scale}x | overlay pdf {} KiB | full poppler {full:?} | \
             build {build:?} + overlay render {overlay_render:?}{}",
            bytes.len() / 1024,
            match image {
                Some(_) => " + fast-path image (composited)",
                None => " (image not fast-path-eligible)",
            }
        );

        assert!(marks_painted(&overlay, scale), "overlay produced no marks");
    }

    fn render_overlay(page: &poppler::Page, scale: f64) -> ImageSurface {
        let (w, h) = page.size();
        let surface = ImageSurface::create(Format::ARgb32, (w * scale) as i32, (h * scale) as i32).unwrap();
        let cr = Context::new(&surface).unwrap();
        cr.scale(scale, scale);
        page.render(&cr);
        drop(cr);
        surface
    }

    fn marks_painted(page: &poppler::Page, scale: f64) -> bool {
        let surface = render_overlay(page, scale);
        let mut painted = false;
        surface.with_data(|d| painted = d.chunks_exact(4).any(|p| p[3] != 0)).unwrap();
        painted
    }

    // Cairo Rgb24/ARgb32 store BGRx; write a plain P6 for eyeballing without a png dep.
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

    fn wait_for_image(uri: &str, page: &poppler::Page, scale: f64) -> Option<ImageSurface> {
        for _ in 0..300 {
            if let Some(s) = crate::image_page::render_image_page(uri, page, scale, 1.0) {
                return Some(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }
}
