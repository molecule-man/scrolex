// Link hit-testing and document targets. Coordinates use page-local top-left points.
use std::collections::{HashMap, HashSet};

use mupdf::{DestinationKind, Document};

use crate::page::Rectangle;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocumentLocation {
    pub page: i32,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    Open,
    OpenBeside,
    OpenInNewTab,
}

#[derive(Debug, Clone, Copy, PartialEq, glib::Boxed)]
#[boxed_type(name = "ScrolexLinkRequest")]
pub struct LinkRequest {
    pub source_page: i32,
    pub location: DocumentLocation,
    pub action: LinkAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Location(DocumentLocation),
    Uri(String),
}

#[derive(Debug, Clone)]
pub(crate) struct PageLink {
    rect: Rectangle,
    target: LinkTarget,
}

// Link cache for one document. The document owner thread reads the links. Hover and clicks read
// only this cache, so they never open a document on the main thread.
#[derive(Default, Debug)]
pub struct Links {
    pages: HashMap<i32, Vec<PageLink>>,
    pending: HashSet<i32>,
    // Bumped by clear(). A reply for an older epoch is stale.
    epoch: u64,
}

impl Links {
    pub(crate) fn clear(&mut self) {
        self.pages.clear();
        self.pending.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }

    // None also while the page's links are not loaded yet.
    pub fn get_link(&self, page_num: i32, x: f64, y: f64) -> Option<&LinkTarget> {
        self.pages
            .get(&page_num)?
            .iter()
            .find(|link| link.rect.contains(x, y))
            .map(|link| &link.target)
    }

    // The epoch for the reply, or None when the page is loaded or already requested.
    pub(crate) fn begin_load(&mut self, page_num: i32) -> Option<u64> {
        if self.pages.contains_key(&page_num) || !self.pending.insert(page_num) {
            return None;
        }
        Some(self.epoch)
    }

    pub(crate) fn finish_load(&mut self, epoch: u64, page_num: i32, links: Vec<PageLink>) {
        if epoch != self.epoch {
            return;
        }
        self.pending.remove(&page_num);
        self.pages.insert(page_num, links);
    }
}

// The page's links in page-local top-left points. Empty when the page can't be read.
pub(crate) fn read_page(doc: &Document, page_num: i32) -> Vec<PageLink> {
    let Ok(page) = doc.load_page(page_num) else {
        return Vec::new();
    };
    let Ok(page_links) = page.links() else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for link in page_links {
        let target = match &link.dest {
            Some(dest) => {
                LinkTarget::Location(destination_location(dest.loc.page_number as i32, dest.kind))
            }
            None if !link.uri.is_empty() => LinkTarget::Uri(link.uri.clone()),
            None => continue,
        };
        let b = link.bounds;
        links.push(PageLink {
            rect: Rectangle::new(b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64),
            target,
        });
    }
    links
}

fn destination_location(page: i32, kind: DestinationKind) -> DocumentLocation {
    let (x, top) = match kind {
        DestinationKind::Fit | DestinationKind::FitB => (None, None),
        DestinationKind::FitH { top } | DestinationKind::FitBH { top } => {
            (None, top.map(f64::from))
        }
        DestinationKind::FitV { left } | DestinationKind::FitBV { left } => {
            (left.map(f64::from), None)
        }
        DestinationKind::XYZ { left, top, .. } => (left.map(f64::from), top.map(f64::from)),
        DestinationKind::FitR { left, bottom, .. } => {
            (Some(f64::from(left)), Some(f64::from(bottom)))
        }
    };
    DocumentLocation { page, x, y: top }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Load `pages` the way the app does: request, owner thread reply, then finish.
    fn owner_links(uri: &str, pages: &[i32]) -> Links {
        let mut links = Links::default();
        for &page in pages {
            let epoch = links.begin_load(page).expect("first request");
            let answer = crate::mupdf_render::page_links(uri, page);
            let page_links = futures::executor::block_on(answer).expect("owner reply");
            links.finish_load(epoch, page, page_links);
        }
        links
    }

    fn uri_link(target: &str) -> Vec<PageLink> {
        vec![PageLink {
            rect: Rectangle::new(0.0, 0.0, 10.0, 10.0),
            target: LinkTarget::Uri(target.into()),
        }]
    }

    #[test]
    fn a_page_is_requested_once_and_hit_tests_only_after_its_reply() {
        let mut links = Links::default();
        let epoch = links.begin_load(3).expect("first request");
        assert_eq!(links.begin_load(3), None, "already pending");
        assert_eq!(links.get_link(3, 5.0, 5.0), None, "no reply yet");

        links.finish_load(epoch, 3, uri_link("https://a.example"));

        assert_eq!(
            links.get_link(3, 5.0, 5.0),
            Some(&LinkTarget::Uri("https://a.example".into()))
        );
        assert_eq!(links.begin_load(3), None, "already loaded");
    }

    #[test]
    fn a_reply_from_before_clear_is_dropped() {
        let mut links = Links::default();
        let stale = links.begin_load(0).expect("first request");
        links.clear();
        let fresh = links.begin_load(0).expect("request again after clear");

        links.finish_load(stale, 0, uri_link("https://old.example"));
        assert_eq!(links.get_link(0, 5.0, 5.0), None);

        links.finish_load(fresh, 0, uri_link("https://new.example"));
        assert_eq!(
            links.get_link(0, 5.0, 5.0),
            Some(&LinkTarget::Uri("https://new.example".into()))
        );
    }

    const LINK_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 200 200] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Annots [4 0 R] >>\nendobj\n\
4 0 obj\n<< /Type /Annot /Subtype /Link /Rect [50 60 150 90] /A << /S /URI /URI (https://one.example) >> >>\nendobj\n\
5 0 obj\n<< /Type /Page /Parent 2 0 R /Annots [6 0 R] >>\nendobj\n\
6 0 obj\n<< /Type /Annot /Subtype /Link /Rect [20 20 80 50] /A << /S /URI /URI (https://two.example) >> >>\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    #[gtk::test]
    fn caches_links_for_alternate_pages() {
        let dir = std::env::temp_dir().join("scrolex_links_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("links.pdf");
        std::fs::write(&path, LINK_PDF).unwrap();
        let uri = crate::test_support::file_uri(&path);
        let links = owner_links(&uri, &[0, 1]);

        assert_eq!(
            links.get_link(0, 100.0, 125.0),
            Some(&LinkTarget::Uri("https://one.example".into()))
        );
        assert_eq!(links.get_link(0, 10.0, 10.0), None);
        assert_eq!(
            links.get_link(1, 50.0, 165.0),
            Some(&LinkTarget::Uri("https://two.example".into()))
        );
        assert_eq!(
            links.get_link(0, 100.0, 125.0),
            Some(&LinkTarget::Uri("https://one.example".into()))
        );
        assert_eq!(links.pages.len(), 2);
    }

    #[gtk::test]
    fn document_request_fills_the_links_from_the_owner_thread() {
        use gtk::subclass::prelude::ObjectSubclassIsExt;

        let dir = std::env::temp_dir().join("scrolex_links_request_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("links.pdf");
        std::fs::write(&path, LINK_PDF).unwrap();
        let document = crate::document::Document::new();
        document.set_uri(crate::test_support::file_uri(&path));

        document.request_links(1);
        crate::test_support::wait_until(|| {
            document
                .imp()
                .links
                .borrow()
                .get_link(1, 50.0, 165.0)
                .is_some()
        });

        assert_eq!(
            document.imp().links.borrow().get_link(1, 50.0, 165.0),
            Some(&LinkTarget::Uri("https://two.example".into()))
        );
    }

    // A 792pt page, so a mirrored y stands out: PDF 592 flips to 200, and back to 592.
    const GOTO_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 /MediaBox [0 0 612 792] >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /Annots [4 0 R 6 0 R 7 0 R 8 0 R] >>\nendobj\n\
4 0 obj\n<< /Type /Annot /Subtype /Link /Rect [50 700 150 730] /Dest [5 0 R /XYZ 20 792 null] >>\nendobj\n\
5 0 obj\n<< /Type /Page /Parent 2 0 R >>\nendobj\n\
6 0 obj\n<< /Type /Annot /Subtype /Link /Rect [50 600 150 630] /Dest [5 0 R /XYZ 20 592 null] >>\nendobj\n\
7 0 obj\n<< /Type /Annot /Subtype /Link /Rect [50 500 150 530] /Dest [5 0 R /FitR 30 492 200 592] >>\nendobj\n\
8 0 obj\n<< /Type /Annot /Subtype /Link /Rect [50 400 150 430] /Dest [5 0 R /Fit] >>\nendobj\n\
trailer\n<< /Root 1 0 R >>\n%%EOF";

    #[gtk::test]
    fn reads_destination_coordinates_in_page_space() {
        let dir = std::env::temp_dir().join("scrolex_goto_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("goto.pdf");
        std::fs::write(&path, GOTO_PDF).unwrap();
        let uri = crate::test_support::file_uri(&path);
        let links = owner_links(&uri, &[0]);
        let location = |x, y| Some(LinkTarget::Location(DocumentLocation { page: 1, x, y }));

        // Rect [50 700 150 730] -> fitz y 62..92. Top of the page -> y 0.
        assert_eq!(
            links.get_link(0, 100.0, 75.0).cloned(),
            location(Some(20.0), Some(0.0))
        );
        // Rect [50 600 150 630] -> fitz y 162..192. PDF y 592 -> fitz y 200.
        assert_eq!(
            links.get_link(0, 100.0, 175.0).cloned(),
            location(Some(20.0), Some(200.0))
        );
        // FitR: the upper edge is PDF y 592 -> fitz y 200, which mupdf names `bottom`.
        assert_eq!(
            links.get_link(0, 100.0, 275.0).cloned(),
            location(Some(30.0), Some(200.0))
        );
        // Fit names no coordinate.
        assert_eq!(
            links.get_link(0, 100.0, 375.0).cloned(),
            location(None, None)
        );
    }

    #[test]
    fn converts_supported_destination_coordinates() {
        let cases = [
            (DestinationKind::Fit, (None, None)),
            (DestinationKind::FitB, (None, None)),
            (
                DestinationKind::FitH { top: Some(30.0) },
                (None, Some(30.0)),
            ),
            (
                DestinationKind::FitBH { top: Some(50.0) },
                (None, Some(50.0)),
            ),
            (
                DestinationKind::FitV { left: Some(12.0) },
                (Some(12.0), None),
            ),
            (
                DestinationKind::FitBV { left: Some(14.0) },
                (Some(14.0), None),
            ),
            (
                DestinationKind::XYZ {
                    left: Some(20.0),
                    top: Some(40.0),
                    zoom: Some(400.0),
                },
                (Some(20.0), Some(40.0)),
            ),
            (
                DestinationKind::FitR {
                    left: 30.0,
                    bottom: 60.0,
                    right: 80.0,
                    top: 140.0,
                },
                (Some(30.0), Some(60.0)),
            ),
        ];
        for (kind, expected) in cases {
            let location = destination_location(2, kind);
            assert_eq!(location.page, 2);
            assert_eq!((location.x, location.y), expected);
        }
    }
}
