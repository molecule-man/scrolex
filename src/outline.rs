// Document outline (table of contents), flattened from MuPDF's nested outline tree.

use mupdf::Document;

// One outline entry. `depth` is the nesting level (0 = top-level). `page` is 1-based, None when the
// entry has no resolvable internal destination (e.g. an external URL).
pub struct OutlineEntry {
    pub title: String,
    pub depth: u32,
    pub page: Option<i32>,
}

// Flattened outline in document order; empty when the document has no index.
pub fn entries(uri: &str) -> Vec<OutlineEntry> {
    crate::mupdf_render::with_doc(uri, |doc| Some(from_doc(doc))).unwrap_or_default()
}

fn from_doc(doc: &Document) -> Vec<OutlineEntry> {
    match doc.outlines() {
        Ok(items) => flatten(&items, 0),
        Err(_) => Vec::new(),
    }
}

fn flatten(items: &[mupdf::Outline], depth: u32) -> Vec<OutlineEntry> {
    let mut out = Vec::new();
    for item in items {
        out.push(OutlineEntry {
            title: item.title.clone(),
            depth,
            // MuPDF resolves an internal destination to a 0-based page; the TOC handler wants 1-based.
            page: item.dest.map(|d| d.loc.page_number as i32 + 1),
        });
        out.extend(flatten(&item.down, depth + 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_bytes(bytes: &[u8]) -> Vec<OutlineEntry> {
        from_doc(&Document::from_bytes(bytes, "pdf").unwrap())
    }

    #[test]
    fn flattens_nested_outline_with_depths_and_pages() {
        let got: Vec<_> = from_bytes(include_bytes!("../tests/fixtures/outline.pdf"))
            .iter()
            .map(|e| (e.title.clone(), e.depth, e.page))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Chapter 1".into(), 0, Some(1)),
                ("Chapter 2".into(), 0, Some(2)),
                ("Section 2.1".into(), 1, Some(3)),
            ]
        );
    }

    #[test]
    fn resolves_pages_from_fit_destinations() {
        let pages: Vec<_> = from_bytes(include_bytes!("../tests/fixtures/fit_outline.pdf"))
            .iter()
            .map(|e| e.page)
            .collect();
        assert_eq!(pages, vec![Some(1), Some(2)]);
    }

    #[test]
    fn empty_when_document_has_no_outline() {
        assert!(from_bytes(include_bytes!("../tests/fixtures/no_outline.pdf")).is_empty());
    }
}
