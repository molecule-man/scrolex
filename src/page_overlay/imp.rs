use std::sync::OnceLock;

use gtk::glib;
use gtk::glib::subclass::{prelude::*, Signal};
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::page::Page;

#[derive(Debug, Default, gtk::CompositeTemplate)]
#[template(resource = "/com/andr2i/hallyview/page.ui")]
pub struct PageOverlay {
    #[template_child]
    pub(crate) overlay: TemplateChild<gtk::Overlay>,
    #[template_child]
    pub(crate) page: TemplateChild<Page>,
}

#[glib::object_subclass]
impl ObjectSubclass for PageOverlay {
    const NAME: &'static str = "PageOverlay";
    type Type = super::PageOverlay;
    type ParentType = gtk::Widget;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
    }

    fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for PageOverlay {
    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![Signal::builder("page-link-clicked")
                .param_types([i32::static_type()])
                .build()]
        })
    }
    //fn constructed(&self) {
    //    self.parent_constructed();
    //
    //    let overlay = gtk::Overlay::new();
    //    let page = Page::new();
    //    overlay.set_child(Some(&page));
    //
    //    self.page.replace(Some(page));
    //    self.overlay.replace(Some(overlay));
    //}
}

impl WidgetImpl for PageOverlay {}
