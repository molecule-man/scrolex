// Bounded, least-recently-used cache of rendered page textures. Capped by total
// pixel-buffer bytes so documents with very large pages can't exhaust memory.

use std::collections::HashMap;

use gtk::gdk;
use gtk::prelude::TextureExt;

// Budget for the CPU-side texture buffers (GSK holds its own GPU copy). Covers the active scrolling
// working set: visible page plus prefetched neighbours.
const DEFAULT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

struct Entry {
    texture: gdk::Texture,
    bytes: usize,
    pixel_scale: f64,
}

pub struct RenderCache {
    budget_bytes: usize,
    total_bytes: usize,
    entries: HashMap<i32, Entry>,
    // page indices ordered least- to most-recently used
    order: Vec<i32>,
}

impl Default for RenderCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

impl std::fmt::Debug for RenderCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderCache")
            .field("pages", &self.entries.len())
            .field("total_bytes", &self.total_bytes)
            .field("budget_bytes", &self.budget_bytes)
            .finish()
    }
}

impl RenderCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            total_bytes: 0,
            entries: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.budget_bytes = budget_bytes;
        self.evict();
    }

    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    pub fn get(&mut self, page: i32) -> Option<gdk::Texture> {
        let texture = self.entries.get(&page)?.texture.clone();
        self.touch(page);
        Some(texture)
    }

    // Whether a page is cached, without affecting recency (used by prefetch to
    // decide what still needs rendering).
    pub fn contains(&self, page: i32) -> bool {
        self.entries.contains_key(&page)
    }

    // Whether a page is cached at the requested render scale, without affecting recency. Exact
    // comparison is stable because insertion and lookup use the same zoom * scale_factor product.
    pub fn contains_at_scale(&self, page: i32, pixel_scale: f64) -> bool {
        self.entries
            .get(&page)
            .is_some_and(|entry| entry.pixel_scale == pixel_scale)
    }

    // Rough number of pages that fit the budget, from the average cached page size. 0 until
    // something is cached. Bounds the preview window so it can't schedule more than it can keep.
    pub fn page_capacity(&self) -> usize {
        if self.entries.is_empty() {
            return 0;
        }
        let avg = self.total_bytes / self.entries.len();
        self.budget_bytes.checked_div(avg).unwrap_or(0)
    }

    pub fn insert(&mut self, page: i32, texture: gdk::Texture, pixel_scale: f64) {
        // 4 bytes/pixel (BGRx) - close enough to the resident buffer for the budget.
        let bytes = (texture.width() as usize) * (texture.height() as usize) * 4;
        self.remove(page);
        self.entries.insert(
            page,
            Entry {
                texture,
                bytes,
                pixel_scale,
            },
        );
        self.order.push(page);
        self.total_bytes += bytes;
        self.evict();
    }

    pub fn remove(&mut self, page: i32) {
        if let Some(entry) = self.entries.remove(&page) {
            self.total_bytes -= entry.bytes;
            self.order.retain(|&p| p != page);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.total_bytes = 0;
    }

    fn touch(&mut self, page: i32) {
        if let Some(pos) = self.order.iter().position(|&p| p == page) {
            self.order.remove(pos);
            self.order.push(page);
        }
    }

    // Drop LRU entries until within budget, always keeping at least one (the just-inserted,
    // most-recently-used page).
    fn evict(&mut self) {
        while self.total_bytes > self.budget_bytes && self.order.len() > 1 {
            let lru = self.order.remove(0);
            if let Some(entry) = self.entries.remove(&lru) {
                self.total_bytes -= entry.bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::prelude::Cast;

    fn texture(bytes_target: usize) -> gdk::Texture {
        // 4 bytes/pixel; height 1 keeps the tracked byte size == width * 4.
        let width = (bytes_target / 4) as i32;
        let bytes = gtk::glib::Bytes::from_owned(vec![0u8; (width * 4) as usize]);
        gdk::MemoryTexture::new(
            width,
            1,
            gdk::MemoryFormat::B8g8r8x8,
            &bytes,
            (width * 4) as usize,
        )
        .upcast()
    }

    #[gtk::test]
    fn evicts_least_recently_used_over_budget() {
        let mut cache = RenderCache::new(100);
        cache.insert(1, texture(40), 1.0);
        cache.insert(2, texture(40), 1.0);
        // 80 bytes used; inserting another 40 exceeds 100 and evicts page 1
        cache.insert(3, texture(40), 1.0);

        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_some());
        assert!(cache.get(3).is_some());
    }

    #[gtk::test]
    fn distinguishes_render_scales_for_the_same_page() {
        let mut cache = RenderCache::new(100);
        cache.insert(1, texture(40), 1.25);

        assert!(cache.contains_at_scale(1, 1.25));
        assert!(!cache.contains_at_scale(1, 1.5));
        assert!(!cache.contains_at_scale(2, 1.25));
    }

    #[gtk::test]
    fn touch_on_get_protects_from_eviction() {
        let mut cache = RenderCache::new(100);
        cache.insert(1, texture(40), 1.0);
        cache.insert(2, texture(40), 1.0);
        // touch page 1 so page 2 becomes least-recently-used
        assert!(cache.get(1).is_some());
        cache.insert(3, texture(40), 1.0);

        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }

    #[gtk::test]
    fn always_keeps_most_recent_even_if_over_budget() {
        let mut cache = RenderCache::new(10);
        cache.insert(1, texture(40), 1.0);
        assert!(cache.get(1).is_some());
    }
}
