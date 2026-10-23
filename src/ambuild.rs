//! `ambuild`: construct a Hippo index from scratch (paper section 4, algorithm 2).
//!
//! 1. Sample the key column (a few evenly spread block chunks, or the whole table when it is
//!    small) and derive an equi-depth histogram -> bucket split points.
//! 2. Scan the heap once, in block order, growing page ranges while the range's partial
//!    histogram density stays below the threshold, emitting fixed-size entries as ranges close.
//! 3. Record a planner profile (probability that a predicate window of width w overlaps an
//!    entry) so `amcostestimate` can predict selectivity from the *actual* data layout.

use crate::buf::*;
use crate::keymap::*;
use crate::layout::*;
use crate::options;
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::c_void;
use std::ptr;

const SAMPLE_BLOCKS: u32 = 4096;
const SAMPLE_CHUNKS: u32 = 64;
const SAMPLE_CAP: usize = 100_000;

unsafe fn new_page(kind: u16) -> *mut u8 {
    let p = pg_sys::palloc0(blcksz()) as *mut u8;
    init_page(p, kind);
    p
}

unsafe fn append_page(index: pg_sys::Relation, src: *const u8) -> u32 {
    let (b, blk) = Buf::extend(index);
    let mut wal = Wal::start(index);
    let dst = wal.register(&b, true);
    ptr::copy_nonoverlapping(src, dst, blcksz());
    wal.finish();
    blk
}

unsafe fn overwrite_page(index: pg_sys::Relation, blk: u32, src: *const u8) {
    let b = Buf::read(index, blk, Lock::Exclusive);
    let mut wal = Wal::start(index);
    let dst = wal.register(&b, true);
    ptr::copy_nonoverlapping(src, dst, blcksz());
    wal.finish();
}

struct SampleState {
    typ: pg_sys::Oid,
    vals: Vec<i64>,
    seen: u64,
    rng: u64,
}

impl SampleState {
    #[inline]
    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn add(&mut self, v: i64) {
        self.seen += 1;
        if self.vals.len() < SAMPLE_CAP {
            self.vals.push(v);
        } else {
            let j = (self.next_rand() % self.seen) as usize;
            if j < SAMPLE_CAP {
                self.vals[j] = v;
            }
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn sample_cb(
    _index: pg_sys::Relation,
    _tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _alive: bool,
    state: *mut c_void,
) {
    if *isnull {
        return;
    }
    let st = &mut *(state as *mut SampleState);
    st.add(datum_to_key(st.typ, *values));
}

unsafe fn run_scan(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    info: *mut pg_sys::IndexInfo,
    start: u32,
    nblocks: u32,
    cb: unsafe extern "C-unwind" fn(
        pg_sys::Relation,
        pg_sys::ItemPointer,
        *mut pg_sys::Datum,
        *mut bool,
        bool,
        *mut c_void,
    ),
    state: *mut c_void,
) -> f64 {
    let f = (*(*heap).rd_tableam)
        .index_build_range_scan
        .expect("table access method does not support index builds");
    // Run inside an FFI boundary so errors raised below unwind through our Rust frames.
    pg_sys::submodules::ffi::pg_guard_ffi_boundary(|| {
        f(
            heap,
            index,
            info,
            false,
            false,
            false,
            start,
            nblocks,
            Some(cb),
            state,
            ptr::null_mut(),
        )
    })
}

unsafe fn sample_bounds(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    info: *mut pg_sys::IndexInfo,
    typ: pg_sys::Oid,
    buckets: usize,
) -> Vec<i64> {
    let mut st = SampleState {
        typ,
        vals: Vec::new(),
        seen: 0,
        rng: 0x9E37_79B9_7F4A_7C15 ^ (pg_sys::MyProcPid as u64).wrapping_mul(0x1000_0000_01B3),
    };
    let nblk = nblocks(heap);
    let sp = &mut st as *mut SampleState as *mut c_void;
    if nblk <= SAMPLE_BLOCKS {
        run_scan(
            heap,
            index,
            info,
            0,
            pg_sys::InvalidBlockNumber,
            sample_cb,
            sp,
        );
    } else {
        let stride = nblk / SAMPLE_CHUNKS;
        let len = SAMPLE_BLOCKS / SAMPLE_CHUNKS;
        for c in 0..SAMPLE_CHUNKS {
            run_scan(heap, index, info, c * stride, len, sample_cb, sp);
            pgrx::check_for_interrupts!();
        }
    }
    let mut vals = std::mem::take(&mut st.vals);
    if vals.is_empty() {
        return Vec::new();
    }
    vals.sort_unstable();
    let n = vals.len();
    let mut bounds: Vec<i64> = Vec::with_capacity(buckets);
    for i in 1..buckets {
        let b = vals[(i * n / buckets).min(n - 1)];
        if bounds.last() != Some(&b) {
            bounds.push(b);
        }
    }
    // A split point equal to the global maximum would create an unreachable top bucket.
    while bounds.last() == vals.last() {
        bounds.pop();
    }
    bounds
}

struct EntryWriter {
    index: pg_sys::Relation,
    geom: Geometry,
    page: *mut u8,
    n_in_page: usize,
    pages_written: u32,
}

impl EntryWriter {
    unsafe fn new(index: pg_sys::Relation, geom: Geometry) -> Self {
        EntryWriter {
            index,
            geom,
            page: new_page(PAGE_ENTRIES),
            n_in_page: 0,
            pages_written: 0,
        }
    }

    unsafe fn push(&mut self, start: u32, end: u32, bits: &[u64]) {
        if self.n_in_page == self.geom.epp {
            self.flush_page();
        }
        let off = PAGE_HDR + self.n_in_page * self.geom.entry_size;
        write_entry(self.page, off, start, end, bits);
        self.n_in_page += 1;
        set_lower(self.page, PAGE_HDR + self.n_in_page * self.geom.entry_size);
    }

    unsafe fn flush_page(&mut self) {
        let blk = append_page(self.index, self.page);
        if blk != FIRST_ENTRY_BLK + self.pages_written {
            error!(
                "hippo: unexpected block {} while building index (expected {})",
                blk,
                FIRST_ENTRY_BLK + self.pages_written
            );
        }
        self.pages_written += 1;
        init_page(self.page, PAGE_ENTRIES);
        self.n_in_page = 0;
    }

    unsafe fn finish(&mut self) {
        if self.n_in_page > 0 {
            self.flush_page();
        }
        pg_sys::pfree(self.page as *mut c_void);
        self.page = ptr::null_mut();
    }
}

struct BuildState {
    typ: pg_sys::Oid,
    bounds: Vec<i64>,
    nb: usize,
    words: usize,
    threshold_bits: usize,
    max_pages: u32,
    writer: EntryWriter,

    cur_valid: bool,
    cur_start: u32,
    cur_end: u32,
    cur_bits: Vec<u64>,
    cur_pop: usize,

    ntuples: u64,
    nentries: u64,
    bits_total: u64,

    prof_sum: [f64; NWIDTHS],
    prof_n: u64,
    prof_bits: u64,
    positions: Vec<usize>,
}

impl BuildState {
    #[inline]
    fn add(&mut self, blk: u32, bucket: usize) {
        if !self.cur_valid {
            self.cur_valid = true;
            self.cur_start = 0;
            self.cur_end = blk;
        } else if blk > self.cur_end {
            if self.cur_pop >= self.threshold_bits || blk - self.cur_start >= self.max_pages {
                unsafe { self.close_entry() };
                self.cur_start = self.cur_end + 1;
                self.cur_end = blk;
            } else {
                self.cur_end = blk;
            }
        } else if blk < self.cur_end {
            error!("hippo: heap scan returned blocks out of order; the table access method is not supported");
        }
        let (w, m) = (bucket / 64, 1u64 << (bucket % 64));
        if self.cur_bits[w] & m == 0 {
            self.cur_bits[w] |= m;
            self.cur_pop += 1;
        }
    }

    unsafe fn close_entry(&mut self) {
        self.writer
            .push(self.cur_start, self.cur_end, &self.cur_bits);
        self.nentries += 1;
        self.bits_total += self.cur_pop as u64;
        if self.nentries < 65536 || self.nentries % 16 == 0 {
            self.profile_entry();
        }
        for w in self.cur_bits.iter_mut() {
            *w = 0;
        }
        self.cur_pop = 0;
    }

    fn profile_entry(&mut self) {
        let nb = self.nb;
        self.positions.clear();
        for (wi, &word) in self.cur_bits.iter().enumerate() {
            let mut x = word;
            while x != 0 {
                let t = x.trailing_zeros() as usize;
                self.positions.push(wi * 64 + t);
                x &= x - 1;
            }
        }
        for (i, &wd) in WIDTHS.iter().enumerate() {
            let w = (wd as usize).min(nb);
            let maxs = nb - w;
            let mut covered = 0usize;
            let mut last_hi: isize = -1;
            for &j in &self.positions {
                let lo = j.saturating_sub(w - 1);
                let hi = j.min(maxs);
                if lo > hi {
                    continue;
                }
                let from = (lo as isize).max(last_hi + 1);
                if hi as isize >= from {
                    covered += (hi as isize - from + 1) as usize;
                }
                if hi as isize > last_hi {
                    last_hi = hi as isize;
                }
            }
            self.prof_sum[i] += covered as f64 / (maxs + 1) as f64;
        }
        self.prof_n += 1;
        self.prof_bits += self.positions.len() as u64;
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn build_cb(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _alive: bool,
    state: *mut c_void,
) {
    if *isnull {
        return;
    }
    let st = &mut *(state as *mut BuildState);
    st.ntuples += 1;
    let v = datum_to_key(st.typ, *values);
    let bucket = bucket_of(&st.bounds, v);
    st.add(tid_block(tid), bucket);
}

pub unsafe fn build(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    if nblocks(index) != 0 {
        error!(
            "index \"{}\" already contains data",
            pgrx::name_data_to_str(&(*(*index).rd_rel).relname)
        );
    }
    if (*(*index).rd_index).indnatts != 1 {
        error!("hippo indexes support exactly one column");
    }
    let typ = *(*index).rd_opcintype;
    let domain = domain_for_type(typ)
        .unwrap_or_else(|| error!("hippo does not support key type with oid {:?}", typ));
    let (req_buckets, density, max_pages) = options::for_index(index);

    let bounds = sample_bounds(heap, index, info, typ, req_buckets);
    let nb = bounds.len() + 1;
    let geom = Geometry::for_buckets(nb);
    if geom.epp == 0 {
        error!("hippo: bucket bitmap does not fit in a page");
    }

    let blank_meta = new_page(PAGE_META);
    let bounds_page = new_page(PAGE_BOUNDS);
    write_bounds(bounds_page, &bounds);
    let blank_profile = new_page(PAGE_PROFILE);
    for (want, p) in [
        (META_BLK, blank_meta),
        (BOUNDS_BLK, bounds_page),
        (PROFILE_BLK, blank_profile),
    ] {
        let got = append_page(index, p);
        if got != want {
            error!(
                "hippo: unexpected block {} while reserving header pages",
                got
            );
        }
    }

    let threshold_bits = ((density as usize * nb).div_ceil(100)).max(1);
    let mut st = BuildState {
        typ,
        bounds,
        nb,
        words: geom.words,
        threshold_bits,
        max_pages,
        writer: EntryWriter::new(index, geom),
        cur_valid: false,
        cur_start: 0,
        cur_end: 0,
        cur_bits: vec![0u64; geom.words],
        cur_pop: 0,
        ntuples: 0,
        nentries: 0,
        bits_total: 0,
        prof_sum: [0.0; NWIDTHS],
        prof_n: 0,
        prof_bits: 0,
        positions: Vec::new(),
    };
    let _ = st.words;
    let reltuples = run_scan(
        heap,
        index,
        info,
        0,
        pg_sys::InvalidBlockNumber,
        build_cb,
        &mut st as *mut BuildState as *mut c_void,
    );
    if st.cur_valid {
        st.close_entry();
    }
    st.writer.finish();

    let mut prof = Profile {
        nsampled: st.prof_n,
        mean_bits: 0.0,
        _pad: 0,
        frac: [1.0; NWIDTHS],
    };
    if st.prof_n > 0 {
        prof.mean_bits = (st.prof_bits as f64 / st.prof_n as f64) as f32;
        for i in 0..NWIDTHS {
            prof.frac[i] = (st.prof_sum[i] / st.prof_n as f64) as f32;
        }
    }
    let pp = new_page(PAGE_PROFILE);
    write_profile(pp, &prof);
    overwrite_page(index, PROFILE_BLK, pp);

    let meta = Meta {
        magic: MAGIC,
        version: FORMAT_VERSION,
        domain,
        nbuckets: nb as u32,
        density_pct: density,
        max_range_pages: max_pages,
        words: geom.words as u32,
        entry_size: geom.entry_size as u32,
        epp: geom.epp as u32,
        _pad: 0,
        nentries: st.nentries,
        ntuples_built: st.ntuples,
        heap_pages_built: nblocks(heap) as u64,
        bits_set_built: st.bits_total,
    };
    let mp = new_page(PAGE_META);
    write_meta(mp, &meta);
    overwrite_page(index, META_BLK, mp);

    for p in [blank_meta, bounds_page, blank_profile, pp, mp] {
        pg_sys::pfree(p as *mut c_void);
    }

    let res = pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBuildResult>())
        as *mut pg_sys::IndexBuildResult;
    (*res).heap_tuples = reltuples;
    (*res).index_tuples = st.ntuples as f64;
    res
}
