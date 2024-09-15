use gtk::glib::translate::FromGlib;
use poppler::LinkMapping;
use std::ffi::CStr;

#[derive(Debug, Clone)]
pub(crate) enum LinkType {
    Unknown,
    Invalid,
    //GotoPage(i32),
    GotoNamedDest(String),
    //Launch(String),
    Uri(String),
}

pub(crate) struct Link(pub(crate) LinkType, pub(crate) poppler::Rectangle);

pub(crate) trait LinkMappingExt {
    fn from_raw(&self) -> Link;
}

impl LinkMappingExt for LinkMapping {
    fn from_raw(&self) -> Link {
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
                    let destination = (*destination_ptr).from_raw();

                    let Dest::Named(name) = destination else {
                        return Link(LinkType::Unknown, area);
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

                _ => Link(LinkType::Unknown, area),
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
    fn from_raw(&self) -> Dest;
}

impl DestExt for poppler::Dest {
    fn from_raw(&self) -> Dest {
        let raw_dest = self.as_ptr();
        unsafe {
            let dest = &*raw_dest;
            dest.from_raw()
        }
    }
}

impl DestExt for poppler_sys::PopplerDest {
    fn from_raw(&self) -> Dest {
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
