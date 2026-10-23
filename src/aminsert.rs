//! `aminsert`: eager index maintenance for inserted tuples (paper section 5.1).
//!
//! * A tuple on a heap block that is already covered by an entry only needs its bucket bit set
//!   in that entry. Most of the time the bit is already set, in which case we only ever take a
//!   share lock.
//! * A tuple beyond the covered area extends the last entry (if its density is still below the
//!   threshold) or appends a new entry. Growth is serialised by the meta page lock; page ranges
//!   only ever grow at the tail, which keeps entries sorted without a separate pointer list.

use crate::buf::*;
use crate::keymap::*;
use crate::layout::*;
use pgrx::pg_sys;
use std::ffi::c_void;

/// Per-IndexInfo cache: immutable index parameters + histogram bounds.
#[repr(C)]
struct CacheHdr {
    geom_words: u32,
    nbuckets: u32,
    threshold_bits: u32,
    nbounds: u32,
    max_pages: u32,
    _pad: u32,
}

struct Cache {
    hdr: *const CacheHdr,
}

impl Cache {
    unsafe fn bounds(&self) -> &[i64] {
        let p = (self.hdr as *const u8).add(std::mem::size_of::<CacheHdr>()) as *const i64;
        std::slice::from_raw_parts(p, (*self.hdr).nbounds as usize)
    }
    unsafe fn nbuckets(&self) -> usize {
        (*self.hdr).nbuckets as usize
    }
    unsafe fn threshold_bits(&self) -> usize {
        (*self.hdr).threshold_bits as usize
    }
    unsafe fn max_pages(&self) -> u32 {
        (*self.hdr).max_pages
    }
}

unsafe fn load_cache(index: pg_sys::Relation, info: *mut pg_sys::IndexInfo) -> (Cache, Geometry) {
    let meta;
    let bounds;
    {
        let mb = Buf::read(index, META_BLK, Lock::Share);
        meta = read_meta(mb.page());
    }
    if meta.magic != MAGIC {
        pgrx::error!("hippo: index is corrupt (bad magic number in meta page)");
    }
    if meta.version != FORMAT_VERSION {
        pgrx::error!("hippo: unsupported index format version {}", meta.version);
    }
    {
        let bb = Buf::read(index, BOUNDS_BLK, Lock::Share);
        bounds = read_bounds(bb.page(), meta.nbuckets as usize);
    }
    let geom = Geometry::from_meta(&meta);
    let size = std::mem::size_of::<CacheHdr>() + bounds.len() * 8;
    let mem = pg_sys::MemoryContextAlloc((*info).ii_Context, size) as *mut u8;
    let hdr = CacheHdr {
        geom_words: meta.words,
        nbuckets: meta.nbuckets,
        threshold_bits: ((meta.density_pct as usize * meta.nbuckets as usize).div_ceil(100)).max(1)
            as u32,
        nbounds: bounds.len() as u32,
        max_pages: meta.max_range_pages,
        _pad: 0,
    };
    std::ptr::write(mem as *mut CacheHdr, hdr);
    std::ptr::copy_nonoverlapping(
        bounds.as_ptr() as *const u8,
        mem.add(std::mem::size_of::<CacheHdr>()),
        bounds.len() * 8,
    );
    (*info).ii_AmCache = mem as *mut c_void;
    (
        Cache {
            hdr: mem as *const CacheHdr,
        },
        geom,
    )
}

unsafe fn get_cache(index: pg_sys::Relation, info: *mut pg_sys::IndexInfo) -> (Cache, Geometry) {
    if (*info).ii_AmCache.is_null() {
        return load_cache(index, info);
    }
    let hdr = (*info).ii_AmCache as *const CacheHdr;
    let geom = Geometry::for_buckets((*hdr).nbuckets as usize);
    debug_assert_eq!(geom.words, (*hdr).geom_words as usize);
    (Cache { hdr }, geom)
}

unsafe fn entry_range(index: pg_sys::Relation, geom: &Geometry, i: u64) -> (u32, u32) {
    let (blk, off) = geom.locate(i);
    let b = Buf::read(index, blk, Lock::Share);
    let p = b.page();
    (entry_start(p, off), entry_end(p, off))
}

/// Index of the entry whose range contains `blk`. Requires `blk <= end of last entry`.
unsafe fn find_entry(index: pg_sys::Relation, geom: &Geometry, n: u64, blk: u32) -> u64 {
    let (mut lo, mut hi) = (0i64, n as i64 - 1);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (s, e) = entry_range(index, geom, mid as u64);
        if blk < s {
            hi = mid - 1;
        } else if blk > e {
            lo = mid + 1;
        } else {
            return mid as u64;
        }
    }
    lo.max(0) as u64
}

/// Set `bucket` in entry `i`, taking the exclusive lock only when the bit is not yet set.
unsafe fn set_bit(index: pg_sys::Relation, geom: &Geometry, i: u64, bucket: usize) {
    let (blk, off) = geom.locate(i);
    let (w, m) = (bucket / 64, 1u64 << (bucket % 64));
    {
        let b = Buf::read(index, blk, Lock::Share);
        if entry_word(b.page(), off, w) & m != 0 {
            return;
        }
    }
    let b = Buf::read(index, blk, Lock::Exclusive);
    let cur = entry_word(b.page(), off, w);
    if cur & m != 0 {
        return;
    }
    let mut wal = Wal::start(index);
    let page = wal.register(&b, false);
    std::ptr::write_unaligned(page.add(off + 8 + w * 8) as *mut u64, cur | m);
    wal.finish();
}

/// Grow the covered area so that `blk` is included. Returns false if somebody else already
/// covered it (the caller then retries the fast path).
unsafe fn grow(
    index: pg_sys::Relation,
    geom: &Geometry,
    threshold_bits: usize,
    max_pages: u32,
    blk: u32,
    bucket: usize,
) -> bool {
    let mb = Buf::read(index, META_BLK, Lock::Exclusive);
    let mut meta = read_meta(mb.page());
    let n = meta.nentries;
    let (w, m) = (bucket / 64, 1u64 << (bucket % 64));

    let mut last: Option<(Buf, usize, u32)> = None;
    if n > 0 {
        let (lblk, loff) = geom.locate(n - 1);
        let lb = Buf::read(index, lblk, Lock::Exclusive);
        let lend = entry_end(lb.page(), loff);
        if blk <= lend {
            return false;
        }
        let lstart = entry_start(lb.page(), loff);
        if entry_popcount(lb.page(), loff, geom.words) < threshold_bits && blk - lstart < max_pages
        {
            let cur = entry_word(lb.page(), loff, w);
            let mut wal = Wal::start(index);
            let page = wal.register(&lb, false);
            std::ptr::write_unaligned(page.add(loff + 4) as *mut u32, blk);
            std::ptr::write_unaligned(page.add(loff + 8 + w * 8) as *mut u64, cur | m);
            wal.finish();
            return true;
        }
        last = Some((lb, loff, lend));
    }

    let start = match &last {
        Some((_, _, lend)) => lend + 1,
        None => 0,
    };
    let (eblk, eoff) = geom.locate(n);
    let mut bits = vec![0u64; geom.words];
    bits[w] = m;

    let mut wal = Wal::start(index);
    let meta_page = wal.register(&mb, false);
    meta.nentries = n + 1;
    write_meta(meta_page, &meta);

    if eoff == PAGE_HDR {
        let (nbuf, got) = Buf::extend(index);
        if got != eblk {
            pgrx::error!(
                "hippo: unexpected block {} while extending index (expected {})",
                got,
                eblk
            );
        }
        let page = wal.register(&nbuf, true);
        init_page(page, PAGE_ENTRIES);
        write_entry(page, eoff, start, blk, &bits);
        set_lower(page, eoff + geom.entry_size);
        wal.finish();
    } else {
        let (lb, _, _) = last.as_ref().expect("entry page must exist");
        let page = wal.register(lb, false);
        write_entry(page, eoff, start, blk, &bits);
        set_lower(page, eoff + geom.entry_size);
        wal.finish();
    }
    true
}

pub unsafe fn insert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    tid: pg_sys::ItemPointer,
    info: *mut pg_sys::IndexInfo,
) -> bool {
    if *isnull {
        return false; // NULLs are never indexed; no supported operator matches them
    }
    let (cache, geom) = get_cache(index, info);
    let typ = *(*index).rd_opcintype;
    let key = datum_to_key(typ, *values);
    let bucket = bucket_of(cache.bounds(), key);
    debug_assert!(bucket < cache.nbuckets());
    let blk = tid_block(tid);

    loop {
        let n = {
            let mb = Buf::read(index, META_BLK, Lock::Share);
            read_meta(mb.page()).nentries
        };
        if n > 0 {
            let (_, last_end) = entry_range(index, &geom, n - 1);
            if blk <= last_end {
                let i = find_entry(index, &geom, n, blk);
                set_bit(index, &geom, i, bucket);
                return false;
            }
        }
        if grow(
            index,
            &geom,
            cache.threshold_bits(),
            cache.max_pages(),
            blk,
            bucket,
        ) {
            return false;
        }
    }
}
