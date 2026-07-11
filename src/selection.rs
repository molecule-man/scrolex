// Text selection. Given the drag's start and end points (page-local top-left points), find the run
// of characters between them in reading order via MuPDF's structured text, and return per-line
// highlight rects plus the selected text. The page's glyph list is cached (a drag fires per pointer
// move, so rebuilding the whole text page each time would be a real per-motion cost).

use std::cell::RefCell;
use std::rc::Rc;

use mupdf::TextPageFlags;

use crate::mupdf_render::{self, with_doc};

pub struct Selection {
    // highlight rects, one per selected line, in page-local top-left points
    pub rects: Vec<(f64, f64, f64, f64)>,
    pub text: String,
}

// One selectable glyph: character, quad centre and bbox, and the line it belongs to.
struct Glyph {
    ch: char,
    cx: f64,
    cy: f64,
    bbox: (f64, f64, f64, f64),
    line: usize,
}

// (uri, page, doc-generation) -> reading-order glyphs.
type GlyphCache = Option<(String, i32, u64, Rc<Vec<Glyph>>)>;

thread_local! {
    // Reused across the many motion events of a single drag; keyed on the generation so a reload
    // rebuilds.
    static GLYPHS: RefCell<GlyphCache> = const { RefCell::new(None) };
}

pub fn selection(uri: &str, page_num: i32, a: (f64, f64), b: (f64, f64)) -> Option<Selection> {
    let glyphs = cached_glyphs(uri, page_num)?;
    select_between(&glyphs, a, b)
}

fn cached_glyphs(uri: &str, page_num: i32) -> Option<Rc<Vec<Glyph>>> {
    let generation = mupdf_render::generation();
    GLYPHS.with(|cell| {
        if let Some((u, p, g, glyphs)) = cell.borrow().as_ref() {
            if u == uri && *p == page_num && *g == generation {
                return Some(glyphs.clone());
            }
        }
        let glyphs = Rc::new(build_glyphs(uri, page_num)?);
        *cell.borrow_mut() = Some((uri.to_string(), page_num, generation, glyphs.clone()));
        Some(glyphs)
    })
}

// Flatten the page's glyphs in reading order, numbering lines so a selection can be broken back into
// per-line highlight rects.
fn build_glyphs(uri: &str, page_num: i32) -> Option<Vec<Glyph>> {
    with_doc(uri, |doc| {
        let page = doc.load_page(page_num).ok()?;
        let text_page = page.to_text_page(TextPageFlags::PRESERVE_WHITESPACE).ok()?;

        let mut glyphs: Vec<Glyph> = Vec::new();
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
        Some(glyphs)
    })
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
