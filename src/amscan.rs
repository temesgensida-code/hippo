//! `amgetbitmap`: index search (paper section 3, algorithm 1).
//!
//! 1. Convert each scan key to an interval of histogram buckets and AND the intervals into a
//!    single predicate bitmap.
//! 2. Walk the entries; any entry whose partial histogram shares a bucket with the predicate
//!    (`entry & predicate != 0`, one AND per 64 buckets) contributes its whole page range.
//! 3. Return the pages as lossy TID-bitmap entries; the executor's Bitmap Heap Scan rechecks
//!    the predicate on every tuple, so the answer is always exact.

use crate::buf::*;
use crate::keymap::*;
use crate::layout::*;
use pgrx::pg_sys;

/// Predicate bitmap for the scan keys; `None` when the keys can never match.
pub unsafe fn predicate_mask(
    index: pg_sys::Relation,
    keys: *const pg_sys::ScanKeyData,
    nkeys: usize,
    meta: &Meta,
    bounds: &[i64],
) -> Option<Vec<u64>> {
    let nb = meta.nbuckets as usize;
    let (mut lo, mut hi) = (0usize, nb - 1);
    let key_type = *(*index).rd_opcintype;
    for i in 0..nkeys {
        let k = &*keys.add(i);
        if k.sk_flags & (pg_sys::SK_ISNULL as i32) != 0 {
            return None;
        }
        if k.sk_flags
            & ((pg_sys::SK_SEARCHARRAY | pg_sys::SK_SEARCHNULL | pg_sys::SK_SEARCHNOTNULL) as i32)
            != 0
        {
            pgrx::error!("hippo: unsupported scan key flags {:#x}", k.sk_flags);
        }
        let arg_type = if k.sk_subtype == pg_sys::InvalidOid {
            key_type
        } else {
            k.sk_subtype
        };
        if domain_for_type(arg_type) != Some(meta.domain) {
            pgrx::error!("hippo: unsupported comparison type {:?}", arg_type);
        }
        let v = datum_to_key(arg_type, k.sk_argument);
        let (l, h) = interval_for(bounds, nb, k.sk_strategy, v);
        lo = lo.max(l);
        hi = hi.min(h);
        if lo > hi {
            return None;
        }
    }
    let mut mask = vec![0u64; meta.words as usize];
    for b in lo..=hi {
        mask[b / 64] |= 1u64 << (b % 64);
    }
    Some(mask)
}

pub unsafe fn get_bitmap(scan: pg_sys::IndexScanDesc, tbm: *mut pg_sys::TIDBitmap) -> i64 {
    let index = (*scan).indexRelation;
    let meta;
    let bounds;
    {
        let mb = Buf::read(index, META_BLK, Lock::Share);
        meta = read_meta(mb.page());
    }
    if meta.magic != MAGIC {
        pgrx::error!("hippo: index is corrupt (bad magic number in meta page)");
    }
    {
        let bb = Buf::read(index, BOUNDS_BLK, Lock::Share);
        bounds = read_bounds(bb.page(), meta.nbuckets as usize);
    }
    let nkeys = (*scan).numberOfKeys as usize;
    let mask = match predicate_mask(index, (*scan).keyData, nkeys, &meta, &bounds) {
        Some(m) => m,
        None => return 0,
    };
    let geom = Geometry::from_meta(&meta);
    let words = geom.words;
    // Bitmap scans carry no heap relation in the scan descriptor; open it briefly (as BRIN does)
    // to learn how many blocks exist, so ranges are clamped to the real heap size.
    let heap_oid = pg_sys::IndexGetRelation((*index).rd_id, false);
    let heap = pg_sys::table_open(heap_oid, pg_sys::AccessShareLock as i32);
    let heap_blocks = nblocks(heap);
    pg_sys::table_close(heap, pg_sys::NoLock as i32);

    let mut pages: i64 = 0;
    let mut remaining = meta.nentries;
    let mut blk = FIRST_ENTRY_BLK;
    while remaining > 0 {
        pgrx::check_for_interrupts!();
        let b = Buf::read(index, blk, Lock::Share);
        let page = b.page();
        let on_page = remaining.min(geom.epp as u64) as usize;
        for e in 0..on_page {
            let off = PAGE_HDR + e * geom.entry_size;
            let mut hit = false;
            for w in 0..words {
                if entry_word(page, off, w) & mask[w] != 0 {
                    hit = true;
                    break;
                }
            }
            if hit {
                let start = entry_start(page, off);
                // clamp to the heap size at scan start: blocks past it cannot hold visible tuples
                let end = entry_end(page, off).min(heap_blocks.saturating_sub(1));
                let mut p = start;
                while p <= end {
                    pg_sys::tbm_add_page(tbm, p);
                    pages += 1;
                    if p == u32::MAX {
                        break;
                    }
                    p += 1;
                }
            }
        }
        remaining -= on_page as u64;
        blk += 1;
    }
    pages * 10
}
