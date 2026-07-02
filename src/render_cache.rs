// Bounded, least-recently-used cache of rendered page surfaces. Capped by total
// pixel-buffer bytes so documents with very large pages can't exhaust memory.

use std::collections::HashMap;

use gtk::cairo::ImageSurface;

// Default memory budget for cached page surfaces. Large scanned pages can be
// tens of MB each, so this bounds how many are kept resident.
const DEFAULT_BUDGET_BYTES: usize = 256 * 1024 * 1024;

struct Entry {
    surface: ImageSurface,
    bytes: usize,
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

    pub fn get(&mut self, page: i32) -> Option<ImageSurface> {
        let surface = self.entries.get(&page)?.surface.clone();
        self.touch(page);
        Some(surface)
    }

    // Whether a page is cached, without affecting recency (used by prefetch to
    // decide what still needs rendering).
    pub fn contains(&self, page: i32) -> bool {
        self.entries.contains_key(&page)
    }

    pub fn insert(&mut self, page: i32, surface: ImageSurface) {
        let bytes = (surface.stride() as usize) * (surface.height() as usize);
        self.remove(page);
        self.entries.insert(page, Entry { surface, bytes });
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

    fn surface(bytes_target: usize) -> ImageSurface {
        // Rgb24 stride is 4 bytes/pixel; height 1 keeps stride == total bytes.
        let width = (bytes_target / 4) as i32;
        ImageSurface::create(gtk::cairo::Format::Rgb24, width, 1).unwrap()
    }

    #[test]
    fn evicts_least_recently_used_over_budget() {
        let mut cache = RenderCache::new(100);
        cache.insert(1, surface(40));
        cache.insert(2, surface(40));
        // 80 bytes used; inserting another 40 exceeds 100 and evicts page 1
        cache.insert(3, surface(40));

        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_some());
        assert!(cache.get(3).is_some());
    }

    #[test]
    fn touch_on_get_protects_from_eviction() {
        let mut cache = RenderCache::new(100);
        cache.insert(1, surface(40));
        cache.insert(2, surface(40));
        // touch page 1 so page 2 becomes least-recently-used
        assert!(cache.get(1).is_some());
        cache.insert(3, surface(40));

        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn always_keeps_most_recent_even_if_over_budget() {
        let mut cache = RenderCache::new(10);
        cache.insert(1, surface(40));
        assert!(cache.get(1).is_some());
    }
}
