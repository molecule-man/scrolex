// Bounded LRU cache of whole-page and viewport-region textures. Tracks total pixel-buffer bytes so
// documents with very large pages cannot exhaust memory.

use std::collections::{HashMap, HashSet};

use gtk::gdk;
use gtk::prelude::TextureExt;

use crate::viewport::ViewportId;

// Budget for the CPU-side texture buffers (GSK holds its own GPU copy). Covers the active scrolling
// working set: visible page plus prefetched neighbours.
const DEFAULT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RenderScale(u64);

impl RenderScale {
    pub fn from_factors(scale: f64, device_scale: f64) -> Self {
        let pixel_scale = scale * device_scale;
        debug_assert!(pixel_scale.is_finite() && pixel_scale > 0.0);
        Self(pixel_scale.to_bits())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageRenderKey {
    pub page: i32,
    pub scale: RenderScale,
}

impl PageRenderKey {
    pub fn from_factors(page: i32, scale: f64, device_scale: f64) -> Self {
        Self {
            page,
            scale: RenderScale::from_factors(scale, device_scale),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub render: PageRenderKey,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CacheKey {
    Page(PageRenderKey),
    Tile(TileId),
}

struct Entry {
    texture: gdk::Texture,
    bytes: usize,
}

pub struct RenderCache {
    budget_bytes: usize,
    total_bytes: usize,
    entries: HashMap<CacheKey, Entry>,
    // texture identities ordered least- to most-recently used
    order: Vec<CacheKey>,
    // Keys currently presented by each mapped page widget. They may exceed the nominal budget: a
    // viewport cannot be made smaller by evicting its own pixels.
    pinned_by_page: HashMap<(ViewportId, i32), HashSet<CacheKey>>,
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

    pub fn get(&mut self, render: PageRenderKey) -> Option<gdk::Texture> {
        let key = CacheKey::Page(render);
        let texture = self.entries.get(&key)?.texture.clone();
        self.touch(key);
        Some(texture)
    }

    pub fn get_latest(&mut self, page: i32) -> Option<gdk::Texture> {
        let key = self.order.iter().rev().find_map(|key| match key {
            CacheKey::Page(render) if render.page == page => Some(*key),
            _ => None,
        })?;
        let texture = self.entries.get(&key)?.texture.clone();
        self.touch(key);
        Some(texture)
    }

    pub fn get_tile(&mut self, tile: TileId) -> Option<gdk::Texture> {
        let key = CacheKey::Tile(tile);
        let texture = self.entries.get(&key)?.texture.clone();
        self.touch(key);
        Some(texture)
    }

    // Check one page and scale without a recency change.
    pub fn contains(&self, render: PageRenderKey) -> bool {
        self.entries.contains_key(&CacheKey::Page(render))
    }

    pub fn contains_page(&self, page: i32) -> bool {
        self.entries
            .keys()
            .any(|key| matches!(key, CacheKey::Page(render) if render.page == page))
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

    pub fn insert(&mut self, render: PageRenderKey, texture: gdk::Texture) {
        self.insert_key(CacheKey::Page(render), texture);
    }

    #[cfg(test)]
    fn insert_tile(&mut self, tile: TileId, texture: gdk::Texture) {
        self.insert_key(CacheKey::Tile(tile), texture);
    }

    pub fn insert_tile_batch(&mut self, tiles: Vec<(TileId, gdk::Texture)>) {
        for (tile, texture) in tiles {
            self.insert_key_unbounded(CacheKey::Tile(tile), texture);
        }
        self.evict();
    }

    pub(crate) fn pin_page(&mut self, viewport: ViewportId, render: PageRenderKey) {
        let owner = (viewport, render.page);
        if self
            .pinned_by_page
            .get(&owner)
            .is_some_and(|keys| keys.len() == 1 && keys.contains(&CacheKey::Page(render)))
        {
            return;
        }
        self.pinned_by_page
            .insert(owner, HashSet::from([CacheKey::Page(render)]));
        self.evict();
    }

    pub(crate) fn pin_tiles(
        &mut self,
        viewport: ViewportId,
        render: PageRenderKey,
        tiles: &[TileId],
    ) {
        let owner = (viewport, render.page);
        if self.pinned_by_page.get(&owner).is_some_and(|keys| {
            keys.len() == tiles.len()
                && tiles
                    .iter()
                    .all(|tile| keys.contains(&CacheKey::Tile(*tile)))
        }) {
            return;
        }
        self.pinned_by_page
            .insert(owner, tiles.iter().copied().map(CacheKey::Tile).collect());
        self.evict();
    }

    pub(crate) fn unpin_page(&mut self, viewport: ViewportId, page: i32) {
        if self.pinned_by_page.remove(&(viewport, page)).is_some() {
            self.evict();
        }
    }

    pub(crate) fn clear_pins(&mut self, viewport: ViewportId) {
        let before = self.pinned_by_page.len();
        self.pinned_by_page
            .retain(|(owner, _), _| *owner != viewport);
        if self.pinned_by_page.len() == before {
            return;
        }
        self.evict();
    }

    pub(crate) fn has_tiled_pages(&self, viewport: ViewportId) -> bool {
        self.pinned_by_page
            .iter()
            .filter(|((owner, _), _)| *owner == viewport)
            .map(|(_, keys)| keys)
            .any(|keys| keys.is_empty() || keys.iter().any(|key| matches!(key, CacheKey::Tile(_))))
    }

    fn insert_key(&mut self, key: CacheKey, texture: gdk::Texture) {
        self.insert_key_unbounded(key, texture);
        self.evict();
    }

    fn insert_key_unbounded(&mut self, key: CacheKey, texture: gdk::Texture) {
        // 4 bytes/pixel (BGRx) - close enough to the resident buffer for the budget.
        let bytes = (texture.width() as usize) * (texture.height() as usize) * 4;
        self.remove_key(key);
        self.entries.insert(key, Entry { texture, bytes });
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

    fn render(page: i32, scale: f64) -> PageRenderKey {
        PageRenderKey::from_factors(page, scale, 1.0)
    }

    fn tile(page: i32, scale: f64, x: i32) -> TileId {
        TileId {
            render: render(page, scale),
            x,
            y: 0,
        }
    }

    #[test]
    fn tracks_tiled_pages_even_before_a_region_is_visible() {
        let mut cache = RenderCache::new(100);
        let left = ViewportId::from_raw(1);
        let right = ViewportId::from_raw(2);
        cache.pin_tiles(left, render(2, 2.0), &[]);

        assert!(cache.has_tiled_pages(left));
        assert!(!cache.has_tiled_pages(right));
        cache.pin_page(left, render(2, 1.0));
        assert!(!cache.has_tiled_pages(left));
    }

    #[gtk::test]
    fn evicts_least_recently_used_over_budget() {
        let mut cache = RenderCache::new(100);
        cache.insert(render(1, 1.0), texture(40));
        cache.insert(render(2, 1.0), texture(40));
        // 80 bytes used; inserting another 40 exceeds 100 and evicts page 1
        cache.insert(render(3, 1.0), texture(40));

        assert!(cache.get(render(1, 1.0)).is_none());
        assert!(cache.get(render(2, 1.0)).is_some());
        assert!(cache.get(render(3, 1.0)).is_some());
    }

    #[gtk::test]
    fn keeps_render_scales_for_the_same_page() {
        let mut cache = RenderCache::new(100);
        let first = PageRenderKey::from_factors(1, 1.25, 1.0);
        let second = PageRenderKey::from_factors(1, 1.5, 1.0);
        cache.insert(first, texture(40));
        cache.insert(second, texture(40));

        assert!(cache.contains(first));
        assert!(cache.contains(second));
        assert!(cache.get(first).is_some());
        assert!(cache.get(second).is_some());
    }

    #[gtk::test]
    fn one_viewport_cannot_remove_another_viewports_pin() {
        let mut cache = RenderCache::new(60);
        let key = PageRenderKey::from_factors(1, 1.0, 1.0);
        cache.pin_page(ViewportId::from_raw(1), key);
        cache.pin_page(ViewportId::from_raw(2), key);
        cache.insert(key, texture(40));

        cache.clear_pins(ViewportId::from_raw(1));
        let other = PageRenderKey::from_factors(2, 1.0, 1.0);
        cache.insert(other, texture(40));

        assert!(cache.get(key).is_some());
        assert!(cache.get(other).is_some());
    }

    #[gtk::test]
    fn touch_on_get_protects_from_eviction() {
        let mut cache = RenderCache::new(100);
        cache.insert(render(1, 1.0), texture(40));
        cache.insert(render(2, 1.0), texture(40));
        // touch page 1 so page 2 becomes least-recently-used
        assert!(cache.get(render(1, 1.0)).is_some());
        cache.insert(render(3, 1.0), texture(40));

        assert!(cache.get(render(1, 1.0)).is_some());
        assert!(cache.get(render(2, 1.0)).is_none());
    }

    #[gtk::test]
    fn always_keeps_most_recent_even_if_over_budget() {
        let mut cache = RenderCache::new(10);
        cache.insert(render(1, 1.0), texture(40));
        assert!(cache.get(render(1, 1.0)).is_some());
    }

    #[gtk::test]
    fn tiles_are_independent_and_scale_specific() {
        let mut cache = RenderCache::new(200);
        let left = tile(3, 4.0, 0);
        let right = tile(3, 4.0, 1024);
        let other_scale = tile(3, 5.0, 0);
        cache.insert_tile(left, texture(40));
        cache.insert_tile(right, texture(40));
        cache.insert_tile(other_scale, texture(40));

        assert!(cache.get_tile(left).is_some());
        assert!(cache.get_tile(right).is_some());
        assert!(cache.get_tile(other_scale).is_some());
        assert!(cache.get(render(3, 4.0)).is_none());
    }

    #[gtk::test]
    fn serial_tile_batch_keeps_the_complete_viewport_over_budget() {
        let mut cache = RenderCache::new(60);
        let stale = tile(1, 1.0, 0);
        cache.insert_tile(stale, texture(40));
        let left = tile(2, 2.0, 0);
        let right = tile(2, 2.0, 10);
        cache.pin_tiles(ViewportId::from_raw(1), render(2, 2.0), &[left, right]);
        cache.insert_tile_batch(vec![(left, texture(40)), (right, texture(40))]);

        assert!(cache.get_tile(stale).is_none());
        assert!(cache.get_tile(left).is_some());
        assert!(cache.get_tile(right).is_some());
    }

    #[gtk::test]
    fn insertion_preserves_pinned_entries_and_one_completed_prefetch() {
        let mut cache = RenderCache::new(60);
        let viewport = ViewportId::from_raw(1);
        let left = tile(2, 2.0, 0);
        let right = tile(2, 2.0, 10);
        cache.pin_tiles(viewport, render(2, 2.0), &[left, right]);
        cache.insert_tile_batch(vec![(left, texture(40)), (right, texture(40))]);

        cache.insert(render(9, 1.0), texture(40));

        assert!(cache.get_tile(left).is_some());
        assert!(cache.get_tile(right).is_some());
        assert!(cache.get(render(9, 1.0)).is_some());

        assert!(cache.get_tile(left).is_some());
        cache.pin_tiles(viewport, render(2, 2.0), &[left, right]);
        assert!(
            cache.get(render(9, 1.0)).is_some(),
            "an unchanged pin set should not run eviction"
        );

        cache.insert(render(10, 1.0), texture(40));
        assert!(cache.get_tile(left).is_some());
        assert!(cache.get_tile(right).is_some());
        assert!(cache.get(render(9, 1.0)).is_none());
        assert!(cache.get(render(10, 1.0)).is_some());
    }

    #[gtk::test]
    fn mapped_whole_page_and_tiles_can_jointly_exceed_the_budget() {
        let mut cache = RenderCache::new(60);
        let viewport = ViewportId::from_raw(1);
        let tile = tile(2, 2.0, 0);
        cache.pin_tiles(viewport, render(2, 2.0), &[tile]);
        cache.insert_tile_batch(vec![(tile, texture(40))]);
        cache.pin_page(viewport, render(9, 1.0));
        cache.insert(render(9, 1.0), texture(40));

        assert!(cache.get_tile(tile).is_some());
        assert!(cache.get(render(9, 1.0)).is_some());
    }
}
