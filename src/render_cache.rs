// Bounded LRU cache of whole-page and viewport-region textures. Tracks total pixel-buffer bytes so
// documents with very large pages cannot exhaust memory.

use std::collections::{HashMap, HashSet};

use gtk::gdk;
use gtk::prelude::TextureExt;

// Budget for the CPU-side texture buffers (GSK holds its own GPU copy). Covers the active scrolling
// working set: visible page plus prefetched neighbours.
const DEFAULT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileId {
    pub page: i32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheKey {
    Page(i32),
    Tile(TileId),
}

struct Entry {
    texture: gdk::Texture,
    bytes: usize,
    pixel_scale: f64,
}

pub struct RenderCache {
    budget_bytes: usize,
    total_bytes: usize,
    entries: HashMap<CacheKey, Entry>,
    // texture identities ordered least- to most-recently used
    order: Vec<CacheKey>,
    // Keys currently presented by each mapped page widget. They may exceed the nominal budget: a
    // viewport cannot be made smaller by evicting its own pixels.
    pinned_by_page: HashMap<i32, HashSet<CacheKey>>,
}

impl Default for RenderCache {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

impl std::fmt::Debug for RenderCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderCache")
            .field("textures", &self.entries.len())
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
            pinned_by_page: HashMap::new(),
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
        let key = CacheKey::Page(page);
        let texture = self.entries.get(&key)?.texture.clone();
        self.touch(key);
        Some(texture)
    }

    pub fn get_tile(&mut self, tile: TileId, pixel_scale: f64) -> Option<gdk::Texture> {
        let key = CacheKey::Tile(tile);
        let entry = self.entries.get(&key)?;
        if entry.pixel_scale != pixel_scale {
            return None;
        }
        let texture = entry.texture.clone();
        self.touch(key);
        Some(texture)
    }

    // Whether a page is cached, without affecting recency (used by prefetch to
    // decide what still needs rendering).
    pub fn contains(&self, page: i32) -> bool {
        self.entries.contains_key(&CacheKey::Page(page))
    }

    // Whether a page is cached at the requested render scale, without affecting recency. Exact
    // comparison is stable because insertion and lookup use the same zoom * scale_factor product.
    pub fn contains_at_scale(&self, page: i32, pixel_scale: f64) -> bool {
        self.entries
            .get(&CacheKey::Page(page))
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
        self.insert_key(CacheKey::Page(page), texture, pixel_scale);
    }

    #[cfg(test)]
    fn insert_tile(&mut self, tile: TileId, texture: gdk::Texture, pixel_scale: f64) {
        self.insert_key(CacheKey::Tile(tile), texture, pixel_scale);
    }

    pub fn insert_tile_batch(&mut self, tiles: Vec<(TileId, gdk::Texture)>, pixel_scale: f64) {
        for (tile, texture) in tiles {
            self.insert_key_unbounded(CacheKey::Tile(tile), texture, pixel_scale);
        }
        self.evict();
    }

    pub fn pin_page(&mut self, page: i32) {
        if self
            .pinned_by_page
            .get(&page)
            .is_some_and(|keys| keys.len() == 1 && keys.contains(&CacheKey::Page(page)))
        {
            return;
        }
        self.pinned_by_page
            .insert(page, HashSet::from([CacheKey::Page(page)]));
        self.evict();
    }

    pub fn pin_tiles(&mut self, page: i32, tiles: &[TileId]) {
        if self.pinned_by_page.get(&page).is_some_and(|keys| {
            keys.len() == tiles.len()
                && tiles
                    .iter()
                    .all(|tile| keys.contains(&CacheKey::Tile(*tile)))
        }) {
            return;
        }
        self.pinned_by_page
            .insert(page, tiles.iter().copied().map(CacheKey::Tile).collect());
        self.evict();
    }

    pub fn unpin_page(&mut self, page: i32) {
        if self.pinned_by_page.remove(&page).is_some() {
            self.evict();
        }
    }

    pub fn clear_pins(&mut self) {
        if self.pinned_by_page.is_empty() {
            return;
        }
        self.pinned_by_page.clear();
        self.evict();
    }

    pub fn has_tiled_pages(&self) -> bool {
        self.pinned_by_page
            .values()
            .any(|keys| keys.is_empty() || keys.iter().any(|key| matches!(key, CacheKey::Tile(_))))
    }

    fn insert_key(&mut self, key: CacheKey, texture: gdk::Texture, pixel_scale: f64) {
        self.insert_key_unbounded(key, texture, pixel_scale);
        self.evict();
    }

    fn insert_key_unbounded(&mut self, key: CacheKey, texture: gdk::Texture, pixel_scale: f64) {
        // 4 bytes/pixel (BGRx) - close enough to the resident buffer for the budget.
        let bytes = (texture.width() as usize) * (texture.height() as usize) * 4;
        self.remove_key(key);
        self.entries.insert(
            key,
            Entry {
                texture,
                bytes,
                pixel_scale,
            },
        );
        self.order.push(key);
        self.total_bytes += bytes;
    }

    fn remove_key(&mut self, key: CacheKey) {
        if let Some(entry) = self.entries.remove(&key) {
            self.total_bytes -= entry.bytes;
            self.order.retain(|&entry_key| entry_key != key);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.pinned_by_page.clear();
        self.total_bytes = 0;
    }

    fn touch(&mut self, key: CacheKey) {
        if let Some(pos) = self.order.iter().position(|&entry_key| entry_key == key) {
            self.order.remove(pos);
            self.order.push(key);
        }
    }

    // Drop unpinned LRU entries until within budget. Preserve the most-recent entry even when it is
    // oversized: visible pages pin it before rendering, while retaining one completed prefetch
    // avoids immediately scheduling the same render again.
    fn evict(&mut self) {
        while self.total_bytes > self.budget_bytes {
            let mru = self.order.last().copied();
            let Some(pos) = self
                .order
                .iter()
                .position(|key| Some(*key) != mru && !self.is_pinned(key))
            else {
                break;
            };
            let lru = self.order.remove(pos);
            if let Some(entry) = self.entries.remove(&lru) {
                self.total_bytes -= entry.bytes;
            }
        }
    }

    fn is_pinned(&self, key: &CacheKey) -> bool {
        self.pinned_by_page.values().any(|keys| keys.contains(key))
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

    #[test]
    fn tracks_tiled_pages_even_before_a_region_is_visible() {
        let mut cache = RenderCache::new(100);
        cache.pin_tiles(2, &[]);

        assert!(cache.has_tiled_pages());
        cache.pin_page(2);
        assert!(!cache.has_tiled_pages());
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

    #[gtk::test]
    fn tiles_are_independent_and_scale_specific() {
        let mut cache = RenderCache::new(200);
        let left = TileId {
            page: 3,
            x: 0,
            y: 0,
        };
        let right = TileId {
            page: 3,
            x: 1024,
            y: 0,
        };
        cache.insert_tile(left, texture(40), 4.0);
        cache.insert_tile(right, texture(40), 4.0);

        assert!(cache.get_tile(left, 4.0).is_some());
        assert!(cache.get_tile(right, 4.0).is_some());
        assert!(cache.get_tile(left, 5.0).is_none());
        assert!(cache.get(3).is_none());
    }

    #[gtk::test]
    fn serial_tile_batch_keeps_the_complete_viewport_over_budget() {
        let mut cache = RenderCache::new(60);
        let stale = TileId {
            page: 1,
            x: 0,
            y: 0,
        };
        cache.insert_tile(stale, texture(40), 1.0);
        let left = TileId {
            page: 2,
            x: 0,
            y: 0,
        };
        let right = TileId {
            page: 2,
            x: 10,
            y: 0,
        };
        cache.pin_tiles(2, &[left, right]);
        cache.insert_tile_batch(vec![(left, texture(40)), (right, texture(40))], 2.0);

        assert!(cache.get_tile(stale, 1.0).is_none());
        assert!(cache.get_tile(left, 2.0).is_some());
        assert!(cache.get_tile(right, 2.0).is_some());
    }

    #[gtk::test]
    fn insertion_preserves_pinned_entries_and_one_completed_prefetch() {
        let mut cache = RenderCache::new(60);
        let left = TileId {
            page: 2,
            x: 0,
            y: 0,
        };
        let right = TileId {
            page: 2,
            x: 10,
            y: 0,
        };
        cache.pin_tiles(2, &[left, right]);
        cache.insert_tile_batch(vec![(left, texture(40)), (right, texture(40))], 2.0);

        cache.insert(9, texture(40), 1.0);

        assert!(cache.get_tile(left, 2.0).is_some());
        assert!(cache.get_tile(right, 2.0).is_some());
        assert!(cache.get(9).is_some());

        assert!(cache.get_tile(left, 2.0).is_some());
        cache.pin_tiles(2, &[left, right]);
        assert!(
            cache.get(9).is_some(),
            "an unchanged pin set should not run eviction"
        );

        cache.insert(10, texture(40), 1.0);
        assert!(cache.get_tile(left, 2.0).is_some());
        assert!(cache.get_tile(right, 2.0).is_some());
        assert!(cache.get(9).is_none());
        assert!(cache.get(10).is_some());
    }

    #[gtk::test]
    fn mapped_whole_page_and_tiles_can_jointly_exceed_the_budget() {
        let mut cache = RenderCache::new(60);
        let tile = TileId {
            page: 2,
            x: 0,
            y: 0,
        };
        cache.pin_tiles(2, &[tile]);
        cache.insert_tile_batch(vec![(tile, texture(40))], 2.0);
        cache.pin_page(9);
        cache.insert(9, texture(40), 1.0);

        assert!(cache.get_tile(tile, 2.0).is_some());
        assert!(cache.get(9).is_some());
    }
}
