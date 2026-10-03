//! A bounded cache of rasterized glyphs.
//!
//! Entries are keyed by (font id, glyph id, size in 1/64 px, horizontal subpixel bin out of
//! [`SUBPIXEL_BINS`]) and stored in a slot array indexed by an open-addressing hash table (linear
//! probing, backward-shift deletion). When the memory budget is exceeded, entries are evicted with
//! the CLOCK (second chance) algorithm. Single-threaded.

use alloc::vec::Vec;

use crate::GlyphId;
use crate::font::Font;
use crate::raster::{GlyphBitmap, GlyphRasterizer, MAX_GLYPH_SIZE, RasterOptions};
use vraster::math;

/// Number of horizontal subpixel positions per pixel.
pub const SUBPIXEL_BINS: u8 = 4;

/// Bookkeeping bytes charged per entry in addition to the bitmap data.
const ENTRY_OVERHEAD: usize = 64;

/// Splits a horizontal pen position into an integer pixel and a subpixel bin
/// (`0..SUBPIXEL_BINS`), rounding to the nearest bin. Draw the glyph rasterized for that bin at
/// `pixel + bitmap.left`.
#[inline]
pub fn subpixel_position(x: f32) -> (i32, u8) {
    if !x.is_finite() {
        return (0, 0);
    }
    let fl = math::floor(x);
    let mut ix = fl as i32;
    let mut bin = math::round((x - fl) * SUBPIXEL_BINS as f32) as i32;
    if bin >= SUBPIXEL_BINS as i32 {
        bin = 0;
        ix = ix.saturating_add(1);
    }
    (ix, bin as u8)
}

/// Cache key of a rasterized glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// [`Font::id`] of the font.
    pub font_id: u32,
    /// Glyph id.
    pub glyph: GlyphId,
    /// Size in 1/64 pixels per em.
    pub size_64: u32,
    /// Horizontal subpixel bin (`0..SUBPIXEL_BINS`).
    pub subpixel: u8,
}

impl GlyphKey {
    /// Builds a key, quantizing the size to 1/64 px and clamping the bin.
    pub fn new(font: &Font<'_>, glyph: GlyphId, size_px: f32, subpixel: u8) -> Self {
        let s = if size_px > 0.0 { size_px.min(MAX_GLYPH_SIZE) } else { 0.0 };
        GlyphKey {
            font_id: font.id(),
            glyph,
            size_64: math::round(s * 64.0) as u32,
            subpixel: subpixel.min(SUBPIXEL_BINS - 1),
        }
    }

    #[inline]
    fn hash(&self) -> u64 {
        let a = ((self.font_id as u64) << 32) ^ ((self.glyph.0 as u64) << 8) ^ self.subpixel as u64;
        let b = (self.size_64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut h = a.wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ b;
        h ^= h >> 31;
        h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^ (h >> 29)
    }
}

/// Cache statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that had to rasterize.
    pub misses: u64,
    /// Entries evicted to stay within the budget.
    pub evictions: u64,
    /// Current number of entries.
    pub entries: usize,
    /// Current charged memory in bytes.
    pub bytes: usize,
}

struct Entry {
    key: GlyphKey,
    bitmap: GlyphBitmap,
    referenced: bool,
    live: bool,
}

/// A memory-bounded cache of rasterized glyphs (see the [module documentation](self)).
pub struct GlyphCache {
    budget: usize,
    used: usize,
    entries: Vec<Entry>,
    free: Vec<u32>,
    /// Slot index + 1 per bucket, 0 = empty. Length is a power of two.
    table: Vec<u32>,
    live: usize,
    hand: usize,
    raster: GlyphRasterizer,
    stats: CacheStats,
}

impl GlyphCache {
    /// Creates a cache that keeps at most about `budget_bytes` of glyph bitmaps, using
    /// [`RasterOptions::default`].
    pub fn new(budget_bytes: usize) -> Self {
        GlyphCache::with_options(budget_bytes, RasterOptions::default())
    }

    /// Creates a cache with explicit rendering options.
    pub fn with_options(budget_bytes: usize, options: RasterOptions) -> Self {
        GlyphCache {
            budget: budget_bytes,
            used: 0,
            entries: Vec::new(),
            free: Vec::new(),
            table: alloc::vec![0; 64],
            live: 0,
            hand: 0,
            raster: GlyphRasterizer::with_options(options),
            stats: CacheStats::default(),
        }
    }

    /// The rendering options.
    pub fn options(&self) -> &RasterOptions {
        self.raster.options()
    }

    /// Changes the rendering options and clears the cache.
    pub fn set_options(&mut self, options: RasterOptions) {
        self.raster.set_options(options);
        self.clear();
    }

    /// The memory budget in bytes.
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Changes the memory budget, evicting entries if necessary.
    pub fn set_budget(&mut self, budget_bytes: usize) {
        self.budget = budget_bytes;
        while self.used > self.budget && self.live > 0 {
            self.evict_one();
        }
    }

    /// Number of cached glyphs.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Returns `true` if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats { entries: self.live, bytes: self.used, ..self.stats }
    }

    /// Removes all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.free.clear();
        self.table.clear();
        self.table.resize(64, 0);
        self.used = 0;
        self.live = 0;
        self.hand = 0;
    }

    /// Returns the bitmap of `glyph` at `size_px` for subpixel bin `subpixel` (see
    /// [`subpixel_position`]), rasterizing and caching it on a miss.
    pub fn get(&mut self, font: &Font<'_>, glyph: GlyphId, size_px: f32, subpixel: u8) -> &GlyphBitmap {
        let key = GlyphKey::new(font, glyph, size_px, subpixel);
        if let Some(slot) = self.find(&key) {
            self.stats.hits += 1;
            let e = &mut self.entries[slot];
            e.referenced = true;
            return &e.bitmap;
        }
        self.stats.misses += 1;
        let size = key.size_64 as f32 / 64.0;
        let offset = key.subpixel as f32 / SUBPIXEL_BINS as f32;
        let bitmap = self.raster.rasterize(font, glyph, size, offset);
        let slot = self.insert(key, bitmap);
        &self.entries[slot].bitmap
    }

    /// Returns a cached bitmap without rasterizing.
    pub fn peek(&self, key: &GlyphKey) -> Option<&GlyphBitmap> {
        self.find(key).map(|s| &self.entries[s].bitmap)
    }

    fn cost(bitmap: &GlyphBitmap) -> usize {
        bitmap.data.len() + ENTRY_OVERHEAD
    }

    fn find(&self, key: &GlyphKey) -> Option<usize> {
        let mask = self.table.len() - 1;
        let mut i = key.hash() as usize & mask;
        loop {
            let s = self.table[i];
            if s == 0 {
                return None;
            }
            let slot = (s - 1) as usize;
            if self.entries[slot].key == *key {
                return Some(slot);
            }
            i = (i + 1) & mask;
        }
    }

    fn insert(&mut self, key: GlyphKey, bitmap: GlyphBitmap) -> usize {
        let cost = Self::cost(&bitmap);
        while self.used + cost > self.budget && self.live > 0 {
            self.evict_one();
        }
        let slot = match self.free.pop() {
            Some(s) => {
                self.entries[s as usize] = Entry { key, bitmap, referenced: true, live: true };
                s as usize
            }
            None => {
                self.entries.push(Entry { key, bitmap, referenced: true, live: true });
                self.entries.len() - 1
            }
        };
        self.used += cost;
        self.live += 1;
        if (self.live + 1) * 2 > self.table.len() {
            self.rebuild_table(self.table.len() * 2);
        } else {
            self.table_insert(slot);
        }
        slot
    }

    fn table_insert(&mut self, slot: usize) {
        let mask = self.table.len() - 1;
        let mut i = self.entries[slot].key.hash() as usize & mask;
        while self.table[i] != 0 {
            i = (i + 1) & mask;
        }
        self.table[i] = slot as u32 + 1;
    }

    fn rebuild_table(&mut self, size: usize) {
        self.table.clear();
        self.table.resize(size.max(64).next_power_of_two(), 0);
        for slot in 0..self.entries.len() {
            if self.entries[slot].live {
                self.table_insert(slot);
            }
        }
    }

    fn table_remove(&mut self, slot: usize) {
        let mask = self.table.len() - 1;
        let mut i = self.entries[slot].key.hash() as usize & mask;
        while self.table[i] != slot as u32 + 1 {
            if self.table[i] == 0 {
                return; // not present (cannot happen)
            }
            i = (i + 1) & mask;
        }
        // Backward-shift deletion keeps probe sequences intact without tombstones.
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let s = self.table[j];
            if s == 0 {
                break;
            }
            let home = self.entries[(s - 1) as usize].key.hash() as usize & mask;
            // Move the entry at j into the hole at i unless its home lies cyclically in (i, j].
            let in_range = if i <= j { home > i && home <= j } else { home > i || home <= j };
            if !in_range {
                self.table[i] = s;
                i = j;
            }
        }
        self.table[i] = 0;
    }

    fn evict_one(&mut self) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        // Two sweeps clear every reference bit, so this terminates while entries are live.
        for _ in 0..2 * n + 1 {
            if self.hand >= n {
                self.hand = 0;
            }
            let h = self.hand;
            self.hand += 1;
            let e = &mut self.entries[h];
            if !e.live {
                continue;
            }
            if e.referenced {
                e.referenced = false;
                continue;
            }
            self.table_remove(h);
            let e = &mut self.entries[h];
            self.used -= Self::cost(&e.bitmap);
            e.live = false;
            e.bitmap = GlyphBitmap::default();
            self.free.push(h as u32);
            self.live -= 1;
            self.stats.evictions += 1;
            return;
        }
    }
}
