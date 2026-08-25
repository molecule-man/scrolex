use std::collections::HashMap;

use mupdf::DestinationKind;

use crate::page::Rectangle;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DocumentLocation {
    pub page: i32,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkTarget {
    Location(DocumentLocation),
    Uri(String),
}

#[derive(Debug, Clone)]
struct PageLink {
    rect: Rectangle,
    target: LinkTarget,
}

#[derive(Default, Debug)]
struct PageLinks {
    links: Vec<PageLink>,
}

#[derive(Default, Debug)]
pub struct Links {
    pages: HashMap<i32, PageLinks>,
}

impl Links {
    pub(crate) fn clear(&mut self) {
        self.pages.clear();
    }

    pub fn get_link(&mut self, uri: &str, page_num: i32, x: f64, y: f64) -> Option<&LinkTarget> {
        self.pages
            .entry(page_num)
            .or_insert_with(|| Self::load(uri, page_num));
        self.pages
            .get(&page_num)?
            .links
            .iter()
            .find(|link| link.rect.contains(x, y))
            .map(|link| &link.target)
    }

    fn load(uri: &str, page_num: i32) -> PageLinks {
        let mut page_links = PageLinks::default();
        crate::mupdf_render::with_doc(uri, |doc| {
            let page = doc.load_page(page_num).ok()?;
            for link in page.links().ok()? {
                let target = match &link.dest {
                    Some(dest) => {
                        let Some(bounds) = doc
                            .load_page(dest.loc.page_number as i32)
                            .ok()
                            .and_then(|page| page.bounds().ok())
                        else {
                            continue;
                        };
                        LinkTarget::Location(destination_location(
                            dest.loc.page_number as i32,
                            bounds.height() as f64,
                            dest.kind,
                        ))
                    }
                    None if !link.uri.is_empty() => LinkTarget::Uri(link.uri.clone()),
                    None => continue,
                };
                let b = link.bounds;
                page_links.links.push(PageLink {
                    rect: Rectangle::new(b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64),
                    target,
                });
            }
            Some(())
        });
        page_links
    }
}

fn destination_location(page: i32, page_height: f64, kind: DestinationKind) -> DocumentLocation {
    let (x, top) = match kind {
        DestinationKind::Fit | DestinationKind::FitB => (0.0, None),
        DestinationKind::FitH { top } | DestinationKind::FitBH { top } => (0.0, top.map(f64::from)),
        DestinationKind::FitV { left } | DestinationKind::FitBV { left } => {
            (left.map(f64::from).unwrap_or(0.0), None)
        }
        DestinationKind::XYZ { left, top, .. } => {
            (left.map(f64::from).unwrap_or(0.0), top.map(f64::from))
        }
        DestinationKind::FitR { left, top, .. } => (f64::from(left), Some(f64::from(top))),
    };
    DocumentLocation {
        page,
        x,
        y: top.map_or(0.0, |top| page_height - top),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut links = Links::default();

        assert_eq!(
            links.get_link(&uri, 0, 100.0, 125.0),
            Some(&LinkTarget::Uri("https://one.example".into()))
        );
        assert_eq!(
            links.get_link(&uri, 1, 50.0, 165.0),
            Some(&LinkTarget::Uri("https://two.example".into()))
        );
        assert_eq!(
            links.get_link(&uri, 0, 100.0, 125.0),
            Some(&LinkTarget::Uri("https://one.example".into()))
        );
        assert_eq!(links.pages.len(), 2);
    }

    #[test]
    fn converts_supported_destination_coordinates() {
        let cases = [
            (DestinationKind::Fit, (0.0, 0.0)),
            (DestinationKind::FitB, (0.0, 0.0)),
            (DestinationKind::FitH { top: Some(170.0) }, (0.0, 30.0)),
            (DestinationKind::FitBH { top: Some(150.0) }, (0.0, 50.0)),
            (DestinationKind::FitV { left: Some(12.0) }, (12.0, 0.0)),
            (DestinationKind::FitBV { left: Some(14.0) }, (14.0, 0.0)),
            (
                DestinationKind::XYZ {
                    left: Some(20.0),
                    top: Some(160.0),
                    zoom: Some(400.0),
                },
                (20.0, 40.0),
            ),
            (
                DestinationKind::FitR {
                    left: 30.0,
                    bottom: 20.0,
                    right: 80.0,
                    top: 140.0,
                },
                (30.0, 60.0),
            ),
        ];
        for (kind, expected) in cases {
            let location = destination_location(2, 200.0, kind);
            assert_eq!(location.page, 2);
            assert_eq!((location.x, location.y), expected);
        }
    }
}
