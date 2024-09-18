mod imp;

use gtk::gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, glib::clone};

use crate::poppler::*;

glib::wrapper! {
    pub struct PageOverlay(ObjectSubclass<imp::PageOverlay>)
        @extends gtk::Widget;
}

impl PageOverlay {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub(crate) fn bind(&self, poppler_page: &poppler::Page, doc: &poppler::Document) {
        let overlay: &gtk::Overlay = self.imp().overlay.as_ref();

        let mut child = overlay.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if c.type_() == gtk::Button::static_type() {
                overlay.remove_overlay(&c);
            }
        }

        for raw_link in poppler_page.link_mapping() {
            let crate::poppler::Link(link_type, area) = raw_link.to_link();

            let btn = gtk::Button::builder()
                .valign(gtk::Align::Start)
                .halign(gtk::Align::Start)
                .opacity(0.0)
                .css_classes(vec!["link-overlay"])
                .cursor(&gtk::gdk::Cursor::from_name("pointer", None).unwrap())
                .build();

            self.imp().page.connect_zoom_notify(clone!(
                #[strong]
                btn,
                move |page| {
                    update_link_location(page, &btn, &area);
                }
            ));

            self.imp().page.connect_bbox_notify(clone!(
                #[strong]
                btn,
                move |page| {
                    update_link_location(page, &btn, &area);
                }
            ));

            update_link_location(&self.imp().page, &btn, &area);

            btn.connect_clicked(clone!(
                #[strong]
                doc,
                #[strong(rename_to = overlay)]
                self,
                move |_| {
                    match link_type.clone() {
                        LinkType::GotoNamedDest(name) => {
                            let Some(dest) = doc.find_dest(&name) else {
                                return;
                            };

                            let Dest::Xyz(page_num) = dest.to_dest() else {
                                return;
                            };

                            overlay.emit_by_name::<()>("page-link-clicked", &[&page_num]);
                        }
                        LinkType::Uri(uri) => {
                            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                                &uri,
                                gtk::gio::AppLaunchContext::NONE,
                            );
                        }
                        LinkType::Unknown(msg) => {
                            println!("unhandled link: {:?}", msg);
                        }
                        _ => {
                            println!("unhandled link: {:?}", link_type);
                        }
                    }
                }
            ));

            overlay.add_overlay(&btn);
        }
    }
}

impl Default for PageOverlay {
    fn default() -> Self {
        Self::new()
    }
}

fn update_link_location(page: &crate::page::Page, btn: &gtk::Button, area: &poppler::Rectangle) {
    let (_, height) = page.popplerpage().as_ref().unwrap().size();
    btn.set_margin_start((page.zoom() * (area.x1() - page.bbox().x1())) as i32);
    btn.set_margin_top((page.zoom() * (height - area.y2() - page.bbox().y1())) as i32);
    btn.set_width_request((page.zoom() * (area.x2() - area.x1())) as i32);
    btn.set_height_request((page.zoom() * (area.y2() - area.y1())) as i32);
}
