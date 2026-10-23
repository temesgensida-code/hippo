//! On-disk layout of a Hippo index.
//!
//! ```text
//! block 0   meta page      : `Meta`
//! block 1   bounds page    : (nbuckets-1) sorted i64 split points of the complete histogram
//! block 2   profile page   : `Profile` (planner statistics gathered at build time)
//! block 3.. entry pages    : fixed-size entries { start_blk u32, end_blk u32, bitmap [u64; words] }
//! ```
//!
//! Differences from the paper (Yu & Sarwat 2016), all deliberate:
//!
//! * Entries are fixed-size (bitmaps are stored uncompressed), so entry `i` lives at a computable
//!   position. The "sorted list of entry pointers" of the paper (section 5.3) is therefore not
//!   needed: ranges only ever grow at the tail, so entry order == heap block order and lookups
//!   are a plain binary search over entry numbers.
//! * Every page change is WAL-logged through PostgreSQL's Generic WAL facility, so the index is
//!   crash safe and replica safe (the paper's prototype is not).

use pgrx::pg_sys;

pub const MAGIC: u32 = 0x4850_5031; // "HPP1"
pub const FORMAT_VERSION: u32 = 2;

pub const META_BLK: u32 = 0;
pub const BOUNDS_BLK: u32 = 1;
pub const PROFILE_BLK: u32 = 2;
pub const FIRST_ENTRY_BLK: u32 = 3;

/// Upper limit of histogram buckets (bounds must fit in a single page).
pub const MAX_BUCKETS: usize = 1000;
pub const MIN_BUCKETS: i32 = 8;
pub const DEFAULT_BUCKETS: i32 = 128;
pub const DEFAULT_DENSITY: i32 = 20;
/// Upper bound on the heap pages one entry may cover (like BRIN's pages_per_range). Without it,
/// well-correlated data produces enormous ranges: density alone only closes a range after ~20%
/// of all buckets were seen, i.e. after ~20% of the table for sequential keys.
pub const DEFAULT_MAX_RANGE_PAGES: i32 = 128;

/// MAXALIGN(SizeOfPageHeaderData)
pub const PAGE_HDR: usize = 24;
/// Size of our special space at the end of every page.
pub const SPECIAL: usize = 8;

pub const PAGE_META: u16 = 1;
pub const PAGE_BOUNDS: u16 = 2;
pub const PAGE_PROFILE: u16 = 3;
pub const PAGE_ENTRIES: u16 = 4;

/// Key domains stored in the meta page.
pub const DOMAIN_INT: u32 = 1; // int2 / int4 / int8 / date / timestamp / timestamptz
pub const DOMAIN_FLOAT: u32 = 2; // float4 / float8

#[inline]
pub fn blcksz() -> usize {
    pg_sys::BLCKSZ as usize
}

#[inline]
pub fn usable() -> usize {
    blcksz() - PAGE_HDR - SPECIAL
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub magic: u32,
    pub version: u32,
    pub domain: u32,
    pub nbuckets: u32,
    pub density_pct: u32,
    pub max_range_pages: u32,
    pub words: u32,
    pub entry_size: u32,
    pub epp: u32,
    pub _pad: u32,
    pub nentries: u64,
    pub ntuples_built: u64,
    pub heap_pages_built: u64,
    pub bits_set_built: u64,
}

/// Window widths (in buckets) at which the build records the probability that a random
/// predicate window of that width overlaps an index entry. The planner interpolates this curve.
pub const NWIDTHS: usize = 20;
pub const WIDTHS: [u32; NWIDTHS] = [
    1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1000,
];

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub nsampled: u64,
    pub mean_bits: f32,
    pub _pad: u32,
    pub frac: [f32; NWIDTHS],
}

/// Entry geometry derived from the number of buckets.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub words: usize,
    pub entry_size: usize,
    pub epp: usize,
}

impl Geometry {
    pub fn for_buckets(nbuckets: usize) -> Geometry {
        let words = nbuckets.div_ceil(64).max(1);
        let entry_size = 8 + 8 * words;
        let epp = usable() / entry_size;
        Geometry {
            words,
            entry_size,
            epp,
        }
    }

    pub fn from_meta(m: &Meta) -> Geometry {
        Geometry {
            words: m.words as usize,
            entry_size: m.entry_size as usize,
            epp: m.epp as usize,
        }
    }

    /// (block number, byte offset in page) of entry `i`.
    #[inline]
    pub fn locate(&self, i: u64) -> (u32, usize) {
        let blk = FIRST_ENTRY_BLK as u64 + i / self.epp as u64;
        let off = PAGE_HDR + (i % self.epp as u64) as usize * self.entry_size;
        (blk as u32, off)
    }
}

/// Number of buckets a predicate window of width `w` overlaps on average, interpolated.
pub fn interpolate_frac(p: &Profile, w: f64) -> f64 {
    let w = w.max(1.0);
    let mut prev_w = WIDTHS[0] as f64;
    let mut prev_f = p.frac[0] as f64;
    if w <= prev_w {
        return prev_f;
    }
    for i in 1..NWIDTHS {
        let cw = WIDTHS[i] as f64;
        let cf = p.frac[i] as f64;
        if w <= cw {
            let t = (w - prev_w) / (cw - prev_w);
            return prev_f + t * (cf - prev_f);
        }
        prev_w = cw;
        prev_f = cf;
    }
    prev_f.max(1.0_f64.min(p.frac[NWIDTHS - 1] as f64))
}
