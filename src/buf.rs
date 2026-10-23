//! Thin RAII wrappers over the buffer manager and Generic WAL, plus raw page helpers.

use crate::layout::*;
use pgrx::pg_sys;
use std::ptr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    Share,
    Exclusive,
}

/// A pinned and locked buffer; unlocked and released on drop.
pub struct Buf {
    pub buf: pg_sys::Buffer,
}

impl Buf {
    /// Read block `blk` of the main fork and lock it.
    pub unsafe fn read(rel: pg_sys::Relation, blk: u32, lock: Lock) -> Buf {
        let buf = pg_sys::ReadBuffer(rel, blk);
        let mode = match lock {
            Lock::Share => pg_sys::BUFFER_LOCK_SHARE,
            Lock::Exclusive => pg_sys::BUFFER_LOCK_EXCLUSIVE,
        };
        pg_sys::LockBuffer(buf, mode as i32);
        Buf { buf }
    }

    /// Extend the relation by one block; returns it exclusively locked together with its number.
    pub unsafe fn extend(rel: pg_sys::Relation) -> (Buf, u32) {
        let is_temp = (*rel).rd_islocaltemp;
        if !is_temp {
            pg_sys::LockRelationForExtension(rel, pg_sys::ExclusiveLock as i32);
        }
        let buf = pg_sys::ReadBuffer(rel, pg_sys::InvalidBlockNumber);
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        if !is_temp {
            pg_sys::UnlockRelationForExtension(rel, pg_sys::ExclusiveLock as i32);
        }
        let blk = pg_sys::BufferGetBlockNumber(buf);
        (Buf { buf }, blk)
    }

    #[inline]
    pub unsafe fn page(&self) -> *mut u8 {
        pg_sys::BufferGetPage(self.buf) as *mut u8
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        unsafe { pg_sys::UnlockReleaseBuffer(self.buf) }
    }
}

/// A Generic WAL record under construction. Aborted on drop unless finished.
pub struct Wal {
    st: *mut pg_sys::GenericXLogState,
    done: bool,
}

impl Wal {
    pub unsafe fn start(rel: pg_sys::Relation) -> Wal {
        Wal {
            st: pg_sys::GenericXLogStart(rel),
            done: false,
        }
    }

    /// Register an exclusively-locked buffer; returns the private page copy to modify.
    pub unsafe fn register(&mut self, b: &Buf, full_image: bool) -> *mut u8 {
        let flags = if full_image {
            pg_sys::GENERIC_XLOG_FULL_IMAGE as i32
        } else {
            0
        };
        pg_sys::GenericXLogRegisterBuffer(self.st, b.buf, flags) as *mut u8
    }

    pub unsafe fn finish(mut self) {
        pg_sys::GenericXLogFinish(self.st);
        self.done = true;
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        if !self.done {
            unsafe { pg_sys::GenericXLogAbort(self.st) }
        }
    }
}

/// Initialise `page` as an empty Hippo page of the given kind.
pub unsafe fn init_page(page: *mut u8, kind: u16) {
    pg_sys::PageInit(page as pg_sys::Page, blcksz(), SPECIAL);
    let special = page.add(blcksz() - SPECIAL);
    ptr::write_unaligned(special as *mut u16, kind);
    ptr::write_unaligned(special.add(2) as *mut u16, 0);
    ptr::write_unaligned(special.add(4) as *mut u32, MAGIC);
}

/// Declare the used area of the page to end at `lower` (bytes from page start).
/// Needed so that WAL full-page images can drop the hole correctly.
pub unsafe fn set_lower(page: *mut u8, lower: usize) {
    (*(page as pg_sys::PageHeader)).pd_lower = lower as u16;
}

pub unsafe fn read_meta(page: *const u8) -> Meta {
    ptr::read_unaligned(page.add(PAGE_HDR) as *const Meta)
}

pub unsafe fn write_meta(page: *mut u8, m: &Meta) {
    ptr::write_unaligned(page.add(PAGE_HDR) as *mut Meta, *m);
    set_lower(page, PAGE_HDR + std::mem::size_of::<Meta>());
}

pub unsafe fn read_profile(page: *const u8) -> Profile {
    ptr::read_unaligned(page.add(PAGE_HDR) as *const Profile)
}

pub unsafe fn write_profile(page: *mut u8, p: &Profile) {
    ptr::write_unaligned(page.add(PAGE_HDR) as *mut Profile, *p);
    set_lower(page, PAGE_HDR + std::mem::size_of::<Profile>());
}

pub unsafe fn read_bounds(page: *const u8, nbuckets: usize) -> Vec<i64> {
    let n = nbuckets.saturating_sub(1);
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(ptr::read_unaligned(page.add(PAGE_HDR + i * 8) as *const i64));
    }
    v
}

pub unsafe fn write_bounds(page: *mut u8, bounds: &[i64]) {
    for (i, b) in bounds.iter().enumerate() {
        ptr::write_unaligned(page.add(PAGE_HDR + i * 8) as *mut i64, *b);
    }
    set_lower(page, PAGE_HDR + bounds.len() * 8);
}

#[inline]
pub unsafe fn entry_start(page: *const u8, off: usize) -> u32 {
    ptr::read_unaligned(page.add(off) as *const u32)
}

#[inline]
pub unsafe fn entry_end(page: *const u8, off: usize) -> u32 {
    ptr::read_unaligned(page.add(off + 4) as *const u32)
}

#[inline]
pub unsafe fn entry_word(page: *const u8, off: usize, w: usize) -> u64 {
    ptr::read_unaligned(page.add(off + 8 + w * 8) as *const u64)
}

#[inline]
pub unsafe fn entry_popcount(page: *const u8, off: usize, words: usize) -> usize {
    let mut c = 0usize;
    for w in 0..words {
        c += entry_word(page, off, w).count_ones() as usize;
    }
    c
}

/// Write an entry (start, end, bitmap) at `off`.
pub unsafe fn write_entry(page: *mut u8, off: usize, start: u32, end: u32, bits: &[u64]) {
    ptr::write_unaligned(page.add(off) as *mut u32, start);
    ptr::write_unaligned(page.add(off + 4) as *mut u32, end);
    for (w, x) in bits.iter().enumerate() {
        ptr::write_unaligned(page.add(off + 8 + w * 8) as *mut u64, *x);
    }
}

/// Block number stored in an item pointer.
#[inline]
pub unsafe fn tid_block(tid: pg_sys::ItemPointer) -> u32 {
    let b = (*tid).ip_blkid;
    ((b.bi_hi as u32) << 16) | b.bi_lo as u32
}

/// Total number of blocks in the main fork.
pub unsafe fn nblocks(rel: pg_sys::Relation) -> u32 {
    pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
}
