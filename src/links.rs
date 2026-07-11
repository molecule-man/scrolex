// Interactive links: hit-test the pointer against a page's link rects (from MuPDF) and resolve to
// either a target page (internal goto) or a URI (external). Rects are page-local top-left points,
// matching MuPDF's coordinate space.

use crate::page::Rectangle;

#[derive(Debug, Clone)]
pub enum LinkTarget {
    // 1-based target page of an internal goto link (what the "named-link-clicked" handler expects).
    Page(i32),
    Uri(String),
}

#[derive(Default, Debug)]
pub struct Links {
    current_page: i32,
    loaded: bool,
    // Split so the hit-test scans rects packed in cache; parallel to `targets`.
    rects: Vec<Rectangle>,
    targets: Vec<LinkTarget>,
}

impl Links {
    pub(crate) fn clear(&mut self) {
        self.rects.clear();
        self.targets.clear();
        self.loaded = false;
        self.current_page = -1;
    }

    // Link target at (x, y) in page-local top-left points, loading this page's links on first hit.
    pub fn get_link(&mut self, uri: &str, page_num: i32, x: f64, y: f64) -> Option<&LinkTarget> {
        if !self.loaded || page_num != self.current_page {
            self.load(uri, page_num);
        }
        let pos = self.rects.iter().position(|rect| rect.contains(x, y))?;
        Some(&self.targets[pos])
    }

    fn load(&mut self, uri: &str, page_num: i32) {
        self.rects.clear();
        self.targets.clear();
        self.current_page = page_num;
        self.loaded = true;

        crate::mupdf_render::with_doc(uri, |doc| {
            let page = doc.load_page(page_num).ok()?;
            for link in page.links().ok()? {
                let target = match &link.dest {
                    // internal goto: MuPDF resolves to a 0-based page; the handler wants 1-based.
                    Some(dest) => LinkTarget::Page(dest.loc.page_number as i32 + 1),
                    None if !link.uri.is_empty() => LinkTarget::Uri(link.uri.clone()),
                    None => continue,
                };
                let b = link.bounds;
                self.rects
                    .push(Rectangle::new(b.x0 as f64, b.y0 as f64, b.x1 as f64, b.y1 as f64));
                self.targets.push(target);
            }
            Some(())
        });
    }
}
