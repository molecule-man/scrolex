// Full-document text search. A background thread walks pages outward from the current page, runs
// MuPDF's per-page search, and streams matches back. An epoch counter cancels a superseded sweep.
// MuPDF search is case-insensitive. A match is a single logical hit and carries one rect per line it
// spans, so a phrase wrapping across lines still counts as one match (and highlights every line).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::channel::mpsc;
use gtk::prelude::FileExt;
use mupdf::text_page::SearchHitResponse;
use mupdf::TextPageFlags;

use crate::page::Rectangle;

// One match's highlight rects (one per line the hit spans), in page coords (top-left origin).
pub type Match = Vec<Rectangle>;

// Matches on one page, tagged with the sweep epoch so stale results can be dropped.
pub struct PageMatches {
    pub epoch: u64,
    pub page: i32,
    pub matches: Vec<Match>,
}

pub type MatchReceiver = mpsc::UnboundedReceiver<PageMatches>;

// Search state. Results are page-ordered so next/previous walks matches in reading order.
#[derive(Default, Debug)]
pub struct Search {
    pub query: String,
    // page -> matches, in page coords (top-left origin)
    pub results: BTreeMap<i32, Vec<Match>>,
    // the highlighted match: (page, index within that page's matches)
    pub current: Option<(i32, usize)>,
    // bumped per sweep; the background thread stops once it no longer matches
    epoch: Arc<AtomicU64>,
}

impl Search {
    pub fn clear(&mut self) {
        self.query.clear();
        self.results.clear();
        self.current = None;
        // abandon any in-flight sweep
        self.epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub fn total(&self) -> usize {
        self.results.values().map(Vec::len).sum()
    }

    // Start a new sweep: bump the epoch (cancelling the previous) and return it plus the shared handle.
    pub fn begin_sweep(&mut self) -> (u64, Arc<AtomicU64>) {
        self.results.clear();
        self.current = None;
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        (epoch, self.epoch.clone())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    // Matches in global reading order: (page, index-within-page) for every match.
    fn ordered(&self) -> Vec<(i32, usize)> {
        self.results
            .iter()
            .flat_map(|(&page, matches)| (0..matches.len()).map(move |i| (page, i)))
            .collect()
    }

    // 1-based position of the current match, for the counter.
    pub fn current_ordinal(&self) -> Option<usize> {
        let current = self.current?;
        self.ordered().iter().position(|&m| m == current).map(|i| i + 1)
    }

    // The match after/before the current one, wrapping. Without a current match, the first/last.
    pub fn step(&self, forward: bool) -> Option<(i32, usize)> {
        let order = self.ordered();
        if order.is_empty() {
            return None;
        }
        let Some(current) = self.current else {
            return Some(if forward { order[0] } else { order[order.len() - 1] });
        };
        let pos = order.iter().position(|&m| m == current).unwrap_or(0);
        let next = if forward {
            (pos + 1) % order.len()
        } else {
            (pos + order.len() - 1) % order.len()
        };
        Some(order[next])
    }

    // Representative rect of a match (its first line), for scrolling the match into view.
    pub fn rect(&self, page: i32, idx: usize) -> Option<Rectangle> {
        self.results
            .get(&page)
            .and_then(|matches| matches.get(idx))
            .and_then(|m| m.first())
            .copied()
    }
}

// Launch a background sweep. Only pages with matches are sent; aborts when the epoch changes or the
// receiver drops.
pub fn spawn_search(
    uri: String,
    query: String,
    n_pages: i32,
    start_page: i32,
    epoch: u64,
    shared_epoch: Arc<AtomicU64>,
) -> MatchReceiver {
    let (tx, rx) = mpsc::unbounded();

    std::thread::spawn(move || {
        // Own MuPDF Document on this short-lived thread (dropped at scope end, before the thread's
        // context TLS teardown, so no drop-order issue).
        let Some(path) = gtk::gio::File::for_uri(&uri).path() else {
            return;
        };
        let Ok(doc) = mupdf::Document::open(path.as_path()) else {
            return;
        };

        for page_num in search_order(n_pages, start_page) {
            if shared_epoch.load(Ordering::Relaxed) != epoch {
                return; // superseded by a newer query
            }
            let matches = search_page(&doc, page_num, &query);
            if !matches.is_empty()
                && tx
                    .unbounded_send(PageMatches {
                        epoch,
                        page: page_num,
                        matches,
                    })
                    .is_err()
            {
                return; // main loop dropped the receiver
            }
        }
    });

    rx
}

// All matches of `query` on one page. MuPDF's callback fires once per logical hit with that hit's
// quads (one per line it spans), so a match becomes one rect per line and streams with no fixed cap.
// Quads are already page-local top-left, so no origin flip.
fn search_page(doc: &mupdf::Document, page_num: i32, query: &str) -> Vec<Match> {
    let mut matches: Vec<Match> = Vec::new();
    let Ok(page) = doc.load_page(page_num) else {
        return matches;
    };
    let Ok(text_page) = page.to_text_page(TextPageFlags::empty()) else {
        return matches;
    };
    let _ = text_page.search_cb(query, &mut matches, |matches, quads| {
        matches.push(quads.iter().map(quad_rect).collect());
        SearchHitResponse::ContinueSearch
    });
    matches
}

// Axis-aligned bounding rect of a MuPDF quad (its four corners), in page-local top-left points.
fn quad_rect(q: &mupdf::Quad) -> Rectangle {
    let xs = [q.ul.x, q.ur.x, q.ll.x, q.lr.x];
    let ys = [q.ul.y, q.ur.y, q.ll.y, q.lr.y];
    let x1 = xs.iter().copied().fold(f32::INFINITY, f32::min) as f64;
    let x2 = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
    let y1 = ys.iter().copied().fold(f32::INFINITY, f32::min) as f64;
    let y2 = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
    Rectangle::new(x1, y1, x2, y2)
}

// Page order for a sweep: start page, then outward (start±1, start±2, …), clamped. Nearest matches
// stream first, so the initial jump lands close by.
fn search_order(n_pages: i32, start_page: i32) -> Vec<i32> {
    if n_pages <= 0 {
        return Vec::new();
    }
    let start = start_page.clamp(0, n_pages - 1);
    let mut order = Vec::with_capacity(n_pages as usize);
    order.push(start);
    let mut d = 1;
    while order.len() < n_pages as usize {
        if start + d < n_pages {
            order.push(start + d);
        }
        if start - d >= 0 {
            order.push(start - d);
        }
        d += 1;
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_order_walks_outward_from_start() {
        assert_eq!(search_order(6, 2), vec![2, 3, 1, 4, 0, 5]);
    }

    #[test]
    fn search_order_clamps_start_and_covers_all_pages() {
        let mut order = search_order(5, 99);
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2, 3, 4]);
        assert_eq!(search_order(5, 99)[0], 4); // clamped start visited first
    }

    #[test]
    fn search_order_from_first_page_is_sequential() {
        assert_eq!(search_order(4, 0), vec![0, 1, 2, 3]);
    }

    #[test]
    fn search_order_empty_document() {
        assert!(search_order(0, 0).is_empty());
    }

    // One line of text, the word "Hello" twice, for search-counting assertions.
    const TWO_HELLO_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 100] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 36 >>\nstream\nBT /F1 24 Tf 20 40 Td (Hello Hello) Tj ET\nendstream\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    #[test]
    fn search_page_counts_and_groups_hits() {
        let doc = mupdf::Document::from_bytes(TWO_HELLO_PDF, "pdf").unwrap();
        // two occurrences => two matches, each a single line (one rect) - NOT one match per quad
        let hits = search_page(&doc, 0, "Hello");
        assert_eq!(hits.len(), 2, "expected 2 matches, got {}", hits.len());
        assert!(hits.iter().all(|m| m.len() == 1), "single-line hits should be one rect each");
        // a phrase within a line is a single match
        assert_eq!(search_page(&doc, 0, "Hello Hello").len(), 1);
        // no match
        assert!(search_page(&doc, 0, "zzz").is_empty());
    }

    fn search_with(pages: &[(i32, usize)]) -> Search {
        let mut s = Search::default();
        for &(page, n) in pages {
            // n matches, each a single-line hit
            s.results.insert(page, vec![vec![Rectangle::default()]; n]);
        }
        s
    }

    #[test]
    fn total_counts_all_rects() {
        let s = search_with(&[(1, 2), (4, 3)]);
        assert_eq!(s.total(), 5);
    }

    #[test]
    fn step_forward_walks_reading_order_and_wraps() {
        let mut s = search_with(&[(1, 2), (4, 1)]);
        s.current = Some((1, 0));
        assert_eq!(s.step(true), Some((1, 1)));
        s.current = Some((1, 1));
        assert_eq!(s.step(true), Some((4, 0)));
        s.current = Some((4, 0));
        assert_eq!(s.step(true), Some((1, 0))); // wrap to first
    }

    #[test]
    fn step_backward_wraps_to_last() {
        let mut s = search_with(&[(1, 2), (4, 1)]);
        s.current = Some((1, 0));
        assert_eq!(s.step(false), Some((4, 0)));
    }

    #[test]
    fn step_without_current_returns_first_or_last() {
        let s = search_with(&[(1, 2), (4, 1)]);
        assert_eq!(s.step(true), Some((1, 0)));
        assert_eq!(s.step(false), Some((4, 0)));
    }

    #[test]
    fn current_ordinal_is_one_based_global_position() {
        let mut s = search_with(&[(1, 2), (4, 1)]);
        s.current = Some((4, 0));
        assert_eq!(s.current_ordinal(), Some(3));
    }
}
