use crate::{
    page::Rectangle,
    poppler::{LinkMappingExt, LinkType},
};

#[derive(Default, Debug)]
pub struct Links {
    current_page: i32,
    // Splitted to benefit from having all rects in L1 cache
    rects: Vec<Rectangle>,
    link_types: Vec<LinkType>,
}

impl Links {
    pub(crate) fn clear(&mut self) {
        self.rects.clear();
        self.link_types.clear();
    }

    fn add_link(&mut self, link: crate::poppler::Link, page_height: f64) {
        self.link_types.push(link.0);
        self.rects
            .push(Rectangle::from_poppler(&link.1, page_height));
    }

    pub fn get_link(&mut self, page: &poppler::Page, x: f64, y: f64) -> Option<&LinkType> {
        if page.index() != self.current_page {
            self.clear();
            let (_, height) = page.size();
            let raw_links = page.link_mapping();

            for raw_link in raw_links {
                self.add_link(raw_link.to_link(), height);
            }
            self.current_page = page.index();
        }

        let pos = self.rects.iter().position(|rect| rect.contains(x, y));

        pos.map(|i| &self.link_types[i])
    }
}
