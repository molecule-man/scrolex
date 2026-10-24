// Text selection uses page-local top-left points. MuPDF provides glyphs in text order.
// The owner reads glyphs once per page. Pointer moves use cached glyphs.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use mupdf::{Document, TextPageFlags};

use crate::mupdf_render;
use crate::page::Rectangle;

pub struct Selection {
    // highlight rects, one per selected line, in page-local top-left points
    pub rects: Vec<(f64, f64, f64, f64)>,
    pub text: String,
}

// The document's text selection: page, per-line highlight rects (page-local top-left points) and
// text. Held by the document, not by the page widget, since list recycling reassigns widgets to pages.
#[derive(Debug)]
pub struct PageSelection {
    pub page: i32,
    pub rects: Vec<Rectangle>,
    pub text: String,
}

// One selectable glyph: character, quad centre and bbox, and the line it belongs to.
pub(crate) struct Glyph {
    pub(crate) ch: char,
    cx: f64,
    cy: f64,
    pub(crate) bbox: (f64, f64, f64, f64),
    pub(crate) line: usize,
}

// One page's glyphs. None means that the owner has not replied.
struct GlyphCache {
    uri: String,
    page: i32,
    generation: u64,
    glyphs: Option<Rc<Vec<Glyph>>>,
}

impl GlyphCache {
    fn is_for(&self, uri: &str, page: i32, generation: u64) -> bool {
        self.uri == uri && self.page == page && self.generation == generation
    }
}

thread_local! {
    // A reload changes the generation and invalidates the cache.
    static GLYPHS: RefCell<Option<GlyphCache>> = const { RefCell::new(None) };
}

// Call load_glyphs() before selection().
pub fn selection(uri: &str, page_num: i32, a: (f64, f64), b: (f64, f64)) -> Option<Selection> {
    let generation = mupdf_render::generation();
    let glyphs = GLYPHS.with(|cell| {
        let cache = cell.borrow();
        let cache = cache.as_ref()?;
        if !cache.is_for(uri, page_num, generation) {
            return None;
        }
        cache.glyphs.clone()
    })?;
    select_between(&glyphs, a, b)
}

// Return None if the glyphs are present or a request exists.
// The future returns true only if its reply enters the cache.
pub fn load_glyphs(uri: &str, page_num: i32) -> Option<impl Future<Output = bool>> {
    let generation = mupdf_render::generation();
    let requested = GLYPHS.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache
            .as_ref()
            .is_some_and(|cache| cache.is_for(uri, page_num, generation))
        {
            return false;
        }
        *cache = Some(GlyphCache {
            uri: uri.to_string(),
            page: page_num,
            generation,
            glyphs: None,
        });
        true
    });
    if !requested {
        return None;
    }
    let answer = mupdf_render::page_glyphs(uri, page_num);
    let uri = uri.to_string();
    Some(async move {
        let reply = answer.await;
        // A reload makes this reply stale, even if the URI is the same.
        if generation != mupdf_render::generation() {
            return false;
        }
        GLYPHS.with(|cell| {
            let mut cache = cell.borrow_mut();
            let Some(entry) = cache
                .as_mut()
                .filter(|cache| cache.is_for(&uri, page_num, generation))
            else {
                return false;
            };
            match reply {
                Ok(glyphs) => {
                    entry.glyphs = Some(Rc::new(glyphs));
                    true
                }
                // Remove a failed request so the next press can retry.
                Err(_) => {
                    *cache = None;
                    false
                }
            }
        })
    })
}

// Keep MuPDF's text order and line boundaries. Return no glyphs if the page fails.
pub(crate) fn read_glyphs(doc: &Document, page_num: i32) -> Vec<Glyph> {
    let mut glyphs: Vec<Glyph> = Vec::new();
    let Ok(page) = doc.load_page(page_num) else {
        return glyphs;
    };
    let Ok(text_page) = page.to_text_page(TextPageFlags::PRESERVE_WHITESPACE) else {
        return glyphs;
    };

    let mut line_id = 0usize;
    for block in text_page.blocks() {
        for line in block.lines() {
            let before = glyphs.len();
            for tc in line.chars() {
                let Some(ch) = tc.char() else { continue };
                let bbox = quad_bbox(&tc.quad());
                glyphs.push(Glyph {
                    ch,
                    cx: (bbox.0 + bbox.2) / 2.0,
                    cy: (bbox.1 + bbox.3) / 2.0,
                    bbox,
                    line: line_id,
                });
            }
            if glyphs.len() > before {
                line_id += 1;
            }
        }
    }
    glyphs
}

fn select_between(glyphs: &[Glyph], a: (f64, f64), b: (f64, f64)) -> Option<Selection> {
    if glyphs.is_empty() {
        return None;
    }
    let (lo, hi) = {
        let ia = nearest(glyphs, a);
        let ib = nearest(glyphs, b);
        (ia.min(ib), ia.max(ib))
    };

    let mut rects = Vec::new();
    let mut text = String::new();
    let mut cur_line: Option<usize> = None;
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for g in &glyphs[lo..=hi] {
        if cur_line != Some(g.line) {
            if let Some(r) = acc.take() {
                rects.push(r);
            }
            if cur_line.is_some() {
                text.push('\n');
            }
            cur_line = Some(g.line);
        }
        text.push(g.ch);
        acc = Some(match acc {
            Some(r) => union(r, g.bbox),
            None => g.bbox,
        });
    }
    if let Some(r) = acc {
        rects.push(r);
    }

    Some(Selection { rects, text })
}

fn quad_bbox(q: &mupdf::Quad) -> (f64, f64, f64, f64) {
    let xs = [q.ul.x, q.ur.x, q.ll.x, q.lr.x];
    let ys = [q.ul.y, q.ur.y, q.ll.y, q.lr.y];
    (
        xs.iter().copied().fold(f32::INFINITY, f32::min) as f64,
        ys.iter().copied().fold(f32::INFINITY, f32::min) as f64,
        xs.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64,
        ys.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64,
    )
}

fn union(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3))
}

// Index of the glyph whose centre is nearest point `p`.
fn nearest(glyphs: &[Glyph], p: (f64, f64)) -> usize {
    let mut best = 0;
    let mut best_d = f64::INFINITY;
    for (i, g) in glyphs.iter().enumerate() {
        let d = (g.cx - p.0).powi(2) + (g.cy - p.1).powi(2);
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    // A glyph 10 wide, 10 tall, centred at (x, y) on `line`.
    fn glyph(ch: char, x: f64, y: f64, line: usize) -> Glyph {
        Glyph {
            ch,
            cx: x,
            cy: y,
            bbox: (x - 5.0, y - 5.0, x + 5.0, y + 5.0),
            line,
        }
    }

    #[test]
    fn select_between_picks_reading_order_run() {
        // "abc" on one line at x = 10, 20, 30
        let glyphs = [
            glyph('a', 10.0, 5.0, 0),
            glyph('b', 20.0, 5.0, 0),
            glyph('c', 30.0, 5.0, 0),
        ];
        // drag from near 'a' to near 'c' selects the whole run; endpoints may be given either way
        let sel = select_between(&glyphs, (32.0, 5.0), (8.0, 5.0)).unwrap();
        assert_eq!(sel.text, "abc");
        assert_eq!(sel.rects.len(), 1, "single line => one rect");
    }

    #[test]
    fn select_between_spans_lines_with_a_rect_each() {
        // "ab" on line 0, "cd" on line 1
        let glyphs = [
            glyph('a', 10.0, 5.0, 0),
            glyph('b', 20.0, 5.0, 0),
            glyph('c', 10.0, 25.0, 1),
            glyph('d', 20.0, 25.0, 1),
        ];
        let sel = select_between(&glyphs, (8.0, 5.0), (22.0, 25.0)).unwrap();
        assert_eq!(sel.text, "ab\ncd");
        assert_eq!(sel.rects.len(), 2, "two lines => a rect each");
    }

    // "Hello" starts at PDF (40, 150) on a 300x200 page.
    const HELLO_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 200] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 34 >>\nstream\nBT /F1 24 Tf 40 150 Td (Hello) Tj ET\nendstream\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    fn hello_uri(name: &str) -> String {
        let dir = std::env::temp_dir().join("scrolex_selection_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, HELLO_PDF).unwrap();
        crate::test_support::file_uri(&path)
    }

    #[gtk::test]
    fn glyphs_load_once_and_select_only_after_the_reply() {
        let uri = hello_uri("hello.pdf");
        let whole_page = ((0.0, 0.0), (300.0, 200.0));

        let loaded = load_glyphs(&uri, 0).expect("first request");
        assert!(load_glyphs(&uri, 0).is_none(), "already pending");
        assert!(selection(&uri, 0, whole_page.0, whole_page.1).is_none());

        assert!(futures::executor::block_on(loaded), "reply cached");

        let sel = selection(&uri, 0, whole_page.0, whole_page.1).expect("glyphs cached");
        assert_eq!(sel.text, "Hello");
        assert_eq!(sel.rects.len(), 1);
        assert!(load_glyphs(&uri, 0).is_none(), "already cached");
    }

    #[gtk::test]
    fn a_reply_for_a_replaced_request_is_dropped() {
        let (first, second) = (hello_uri("first.pdf"), hello_uri("second.pdf"));
        let whole_page = ((0.0, 0.0), (300.0, 200.0));
        let stale = load_glyphs(&first, 0).expect("first request");
        let fresh = load_glyphs(&second, 0).expect("second request replaces the first");

        assert!(
            !futures::executor::block_on(stale),
            "replaced reply is not cached"
        );
        assert!(selection(&first, 0, whole_page.0, whole_page.1).is_none());
        assert!(selection(&second, 0, whole_page.0, whole_page.1).is_none());

        assert!(futures::executor::block_on(fresh));
        assert!(selection(&second, 0, whole_page.0, whole_page.1).is_some());
    }

    #[gtk::test]
    fn a_reply_from_before_a_reload_is_dropped() {
        let uri = hello_uri("reload.pdf");
        let loaded = load_glyphs(&uri, 0).expect("first request");
        mupdf_render::invalidate();

        assert!(
            !futures::executor::block_on(loaded),
            "reply for the old bytes"
        );
        assert!(load_glyphs(&uri, 0).is_some(), "a new press requests again");
    }

    #[test]
    fn select_between_empty_is_none() {
        assert!(select_between(&[], (0.0, 0.0), (1.0, 1.0)).is_none());
    }
}
