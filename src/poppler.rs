use gtk::glib::translate::{FromGlib, ToGlibPtr};
use poppler::{Document, LinkMapping};
use std::ffi::CStr;

#[derive(Debug, Clone)]
pub enum LinkType {
    Unknown(String),
    Invalid,
    GotoNamedDest(String),
    Uri(String),
}

pub struct Link(pub LinkType, pub poppler::Rectangle);

pub trait LinkMappingExt {
    fn to_link(&self) -> Link;
}

impl LinkMappingExt for LinkMapping {
    fn to_link(&self) -> Link {
        let raw_link = self.as_ptr();
        unsafe {
            let link_mapping: &poppler_sys::PopplerLinkMapping = &*raw_link;

            let mut area = poppler::Rectangle::default();
            area.set_x1(link_mapping.area.x1);
            area.set_x2(link_mapping.area.x2);
            area.set_y1(link_mapping.area.y1);
            area.set_y2(link_mapping.area.y2);

            let action_ptr = link_mapping.action;
            if action_ptr.is_null() {
                return Link(LinkType::Invalid, area);
            }

            let action = &*action_ptr;
            match poppler::ActionType::from_glib(action.type_) {
                poppler::ActionType::GotoDest => {
                    let goto_action = action.goto_dest;
                    let destination_ptr = goto_action.dest;

                    if destination_ptr.is_null() {
                        return Link(LinkType::Invalid, area);
                    }
                    let destination = (*destination_ptr).to_dest();

                    let name = match destination {
                        Dest::Named(name) => name,
                        Dest::Unknown(dest_type) => {
                            return Link(
                                LinkType::Unknown(format!("link dest is unknown: {dest_type:?}")),
                                area,
                            )
                        }
                        t => {
                            return Link(
                                LinkType::Unknown(format!("link dest is unhandled {t:?}")),
                                area,
                            )
                        }
                    };

                    Link(LinkType::GotoNamedDest(name), area)
                }
                poppler::ActionType::Uri => {
                    let uri_action = action.uri;
                    let uri_ptr = uri_action.uri;

                    if uri_ptr.is_null() {
                        return Link(LinkType::Invalid, area);
                    }

                    let c_str = CStr::from_ptr(uri_ptr);
                    let rust_string = c_str.to_string_lossy().into_owned();
                    Link(LinkType::Uri(rust_string), area)
                }

                t => Link(
                    LinkType::Unknown(format!("link action is unhandled: {t:?}")),
                    area,
                ),
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum Dest {
    Unknown(poppler::DestType),
    Invalid,
    Named(String),
    Xyz(i32),
}

pub(crate) trait DestExt {
    fn to_dest(&self) -> Dest;
}

impl DestExt for poppler::Dest {
    fn to_dest(&self) -> Dest {
        let raw_dest = self.as_ptr();
        unsafe {
            let dest = &*raw_dest;
            dest.to_dest()
        }
    }
}

impl DestExt for poppler_sys::PopplerDest {
    fn to_dest(&self) -> Dest {
        unsafe {
            match poppler::DestType::from_glib(self.type_) {
                poppler::DestType::Named => {
                    let named_dest_ptr = self.named_dest;
                    if named_dest_ptr.is_null() {
                        return Dest::Invalid;
                    }

                    let c_str = CStr::from_ptr(named_dest_ptr);
                    let rust_string = c_str.to_string_lossy().into_owned();
                    Dest::Named(rust_string)
                }
                poppler::DestType::Xyz => Dest::Xyz(self.page_num),
                t => Dest::Unknown(t),
            }
        }
    }
}

// One outline (table of contents) entry, flattened. `depth` is the nesting level (0 = top-level).
// `page` is 1-based, None when the entry has no resolvable go-to destination.
pub struct OutlineEntry {
    pub title: String,
    pub depth: u32,
    pub page: Option<i32>,
}

// Flattened outline in document order; empty when the document has no index.
pub fn outline(doc: &Document) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    // poppler-rs leaves index_iter_get_action unbound, so walk via FFI
    unsafe {
        let root = poppler_sys::poppler_index_iter_new(doc.to_glib_none().0);
        if root.is_null() {
            return entries;
        }
        walk_index(doc, root, 0, &mut entries);
        poppler_sys::poppler_index_iter_free(root);
    }
    entries
}

unsafe fn walk_index(
    doc: &Document,
    iter: *mut poppler_sys::PopplerIndexIter,
    depth: u32,
    entries: &mut Vec<OutlineEntry>,
) {
    loop {
        let action = poppler_sys::poppler_index_iter_get_action(iter);
        if !action.is_null() {
            if let Some(entry) = read_outline_action(doc, action, depth) {
                entries.push(entry);
            }
            poppler_sys::poppler_action_free(action);
        }

        let child = poppler_sys::poppler_index_iter_get_child(iter);
        if !child.is_null() {
            walk_index(doc, child, depth + 1, entries);
            poppler_sys::poppler_index_iter_free(child);
        }

        if poppler_sys::poppler_index_iter_next(iter) == gtk::glib::ffi::GFALSE {
            break;
        }
    }
}

// `title` shares an offset across all action variants, so read it through `any`.
unsafe fn read_outline_action(
    doc: &Document,
    action: *mut poppler_sys::PopplerAction,
    depth: u32,
) -> Option<OutlineEntry> {
    let title_ptr = (*action).any.title;
    if title_ptr.is_null() {
        return None;
    }
    let title = CStr::from_ptr(title_ptr).to_string_lossy().into_owned();

    let page = if poppler::ActionType::from_glib((*action).type_) == poppler::ActionType::GotoDest {
        let dest = (*action).goto_dest.dest;
        (!dest.is_null()).then(|| dest_page(doc, dest)).flatten()
    } else {
        None
    };
    Some(OutlineEntry { title, depth, page })
}

// Explicit dests (Xyz/Fit/FitH/...) carry a 1-based page_num; named dests resolve through the name tree.
unsafe fn dest_page(doc: &Document, dest: *mut poppler_sys::PopplerDest) -> Option<i32> {
    if poppler::DestType::from_glib((*dest).type_) == poppler::DestType::Named {
        let name_ptr = (*dest).named_dest;
        if name_ptr.is_null() {
            return None;
        }
        let name = CStr::from_ptr(name_ptr).to_string_lossy();
        return dest_page(doc, doc.find_dest(&name)?.as_ptr() as *mut _);
    }
    match (*dest).page_num {
        n if n > 0 => Some(n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_nested_outline_with_depths_and_pages() {
        let doc =
            Document::from_data(include_bytes!("../tests/fixtures/outline.pdf"), None).unwrap();
        let got: Vec<_> = outline(&doc)
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
        let doc =
            Document::from_data(include_bytes!("../tests/fixtures/fit_outline.pdf"), None).unwrap();
        let pages: Vec<_> = outline(&doc).iter().map(|e| e.page).collect();
        assert_eq!(pages, vec![Some(1), Some(2)]);
    }

    #[test]
    fn empty_when_document_has_no_outline() {
        let doc =
            Document::from_data(include_bytes!("../tests/fixtures/no_outline.pdf"), None).unwrap();
        assert!(outline(&doc).is_empty());
    }
}
