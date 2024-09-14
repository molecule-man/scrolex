use gtk::glib::translate::FromGlib;
use poppler::LinkMapping;
use std::ffi::CStr;

#[derive(Debug)]
pub(crate) enum Link {
    Unknown,
    Invalid,
    //GotoPage(i32),
    GotoNamedDest(String, poppler::Rectangle),
    //Launch(String),
    Uri(String),
}

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
                return Link::Invalid;
            }

            let action = &*action_ptr;
            match poppler::ActionType::from_glib(action.type_) {
                poppler::ActionType::GotoDest => {
                    let goto_action = action.goto_dest;
                    let destination_ptr = goto_action.dest;

                    if destination_ptr.is_null() {
                        return Link::Invalid;
                    }
                    let destination = &*destination_ptr;

                    if poppler::DestType::from_glib(destination.type_) != poppler::DestType::Named {
                        return Link::Unknown;
                    }

                    let named_dest_ptr = destination.named_dest;
                    if named_dest_ptr.is_null() {
                        return Link::Invalid;
                    }

                    let c_str = CStr::from_ptr(named_dest_ptr);
                    let rust_string = c_str.to_string_lossy().into_owned();
                    return Link::GotoNamedDest(rust_string, area);
                }
                poppler::ActionType::Uri => {
                    let uri_action = action.uri;
                    let uri_ptr = uri_action.uri;

                    if uri_ptr.is_null() {
                        return Link::Invalid;
                    }

                    let c_str = CStr::from_ptr(uri_ptr);
                    let rust_string = c_str.to_string_lossy().into_owned();
                    return Link::Uri(rust_string);
                }

                _ => {}
            }
        }

        Link::Unknown
    }
}
