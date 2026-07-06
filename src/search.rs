// Full-document text search. A background thread walks pages outward from the current page, runs
// poppler's per-page find_text, and streams matches back. An epoch counter cancels a superseded sweep.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::channel::mpsc;
use poppler::FindFlags;

use crate::page::Rectangle;

// Matches on one page, tagged with the sweep epoch so stale results can be dropped.
pub struct PageMatches {
    pub epoch: u64,
    pub page: i32,
    pub rects: Vec<Rectangle>,
}

pub type MatchReceiver = mpsc::UnboundedReceiver<PageMatches>;

// Search state. Results are page-ordered so next/previous walks matches in reading order.
#[derive(Default, Debug)]
pub struct Search {
    pub query: String,
    pub case_sensitive: bool,
    // page -> match rects, in page coords (top-left origin)
    pub results: BTreeMap<i32, Vec<Rectangle>>,
    // the highlighted match: (page, index within that page's rects)
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

    pub fn flags(&self) -> FindFlags {
        if self.case_sensitive {
            FindFlags::CASE_SENSITIVE
        } else {
            FindFlags::DEFAULT
        }
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

    // Matches in global reading order: (page, index-within-page) for every rect.
    fn ordered(&self) -> Vec<(i32, usize)> {
        self.results
            .iter()
            .flat_map(|(&page, rects)| (0..rects.len()).map(move |i| (page, i)))
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

    pub fn rect(&self, page: i32, idx: usize) -> Option<Rectangle> {
        self.results.get(&page).and_then(|r| r.get(idx)).copied()
    }
}

// Launch a background sweep. Only pages with matches are sent; aborts when the epoch changes or the
// receiver drops.
pub fn spawn_search(
    uri: String,
    query: String,
    flags: FindFlags,
    n_pages: i32,
    start_page: i32,
    epoch: u64,
    shared_epoch: Arc<AtomicU64>,
) -> MatchReceiver {
    let (tx, rx) = mpsc::unbounded();

    std::thread::spawn(move || {
        let file = gtk::gio::File::for_uri(&uri);
        let Ok(doc) = poppler::Document::from_gfile(&file, None, gtk::gio::Cancellable::NONE) else {
            return;
        };

        for page_num in search_order(n_pages, start_page) {
            if shared_epoch.load(Ordering::Relaxed) != epoch {
                return; // superseded by a newer query
            }
            let Some(page) = doc.page(page_num) else {
                continue;
            };
            let (_, height) = page.size();
            // find_text uses a bottom-left origin (like link mapping); flip into our top-left space.
            let rects: Vec<Rectangle> = page
                .find_text_with_options(&query, flags)
                .iter()
                .map(|r| Rectangle::from_poppler(r, height))
                .collect();
            if !rects.is_empty()
                && tx
                    .unbounded_send(PageMatches {
                        epoch,
                        page: page_num,
                        rects,
                    })
                    .is_err()
            {
                return; // main loop dropped the receiver
            }
        }
    });

    rx
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

    // "Hello" sits near the bottom of a 300pt page. find_text reports a bottom-left origin, so the
    // flip must land it in the lower half. Pins the flip against a poppler convention change.
    const BOTTOM_TEXT_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 300 300] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents 4 0 R >>\nendobj\n\
4 0 obj\n<< /Length 36 >>\nstream\nBT\n/F1 24 Tf\n40 30 Td\n(Hello) Tj\nET\nendstream\nendobj\n\
trailer\n<< /Size 5 /Root 1 0 R >>\n%%EOF";

    #[test]
    fn find_text_matches_flip_into_top_left_space() {
        let doc = poppler::Document::from_data(BOTTOM_TEXT_PDF, None).unwrap();
        let page = doc.page(0).unwrap();
        let (_, height) = page.size();
        assert!((height - 300.0).abs() < 0.001, "unexpected page height {height}");

        // DEFAULT is case-insensitive
        let raw = page.find_text_with_options("hello", FindFlags::DEFAULT);
        assert_eq!(raw.len(), 1, "expected exactly one match");

        let flipped = Rectangle::from_poppler(&raw[0], height);
        assert!(flipped.y1 < flipped.y2, "y1 must be the top edge, got {flipped:?}");
        assert!(
            flipped.y1 > height / 2.0,
            "bottom-of-page text must flip into the lower half, got y1={}",
            flipped.y1
        );
    }

    #[test]
    fn case_sensitive_flag_excludes_wrong_case() {
        let doc = poppler::Document::from_data(BOTTOM_TEXT_PDF, None).unwrap();
        let page = doc.page(0).unwrap();
        assert_eq!(
            page.find_text_with_options("hello", FindFlags::CASE_SENSITIVE).len(),
            0,
            "case-sensitive search must not match the wrong case"
        );
        assert_eq!(
            page.find_text_with_options("Hello", FindFlags::CASE_SENSITIVE).len(),
            1,
        );
    }

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

    fn search_with(pages: &[(i32, usize)]) -> Search {
        let mut s = Search::default();
        for &(page, n) in pages {
            s.results.insert(page, vec![Rectangle::default(); n]);
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
