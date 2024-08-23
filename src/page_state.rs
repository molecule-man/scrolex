mod imp;
use gtk::glib;

glib::wrapper! {
    pub struct PageState(ObjectSubclass<imp::PageState>);
}

impl PageState {
    pub fn new(zoom: f64, crop: bool) -> Self {
        glib::Object::builder()
            .property("zoom", zoom)
            .property("crop", crop)
            .build()
    }
}
