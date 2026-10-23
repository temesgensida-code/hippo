//! `amcostestimate`.
//!
//! The paper's cost model (section 6) assumes uniformly random data and therefore predicts the
//! same page-selection probability for every data layout. Here the build records a *profile*:
//! the measured probability that a predicate window covering `w` buckets overlaps an entry.
//! The planner combines that curve with the predicate's bucket interval (computed from the
//! constants in the query, using the stored histogram) to predict the fraction of the heap a
//! scan will visit, which is small for correlated data and close to 1 for random data.

use crate::buf::*;
use crate::keymap::*;
use crate::layout::*;
use pgrx::{pg_sys, PgList};

struct IndexFacts {
    meta: Meta,
    profile: Profile,
    bounds: Vec<i64>,
    index_pages: u32,
}

unsafe fn load_facts(index_oid: pg_sys::Oid) -> Option<IndexFacts> {
    let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as i32);
    let facts = (|| {
        let npages = nblocks(index);
        if npages < FIRST_ENTRY_BLK {
            return None;
        }
        let meta = {
            let b = Buf::read(index, META_BLK, Lock::Share);
            read_meta(b.page())
        };
        if meta.magic != MAGIC || meta.version != FORMAT_VERSION {
            return None;
        }
        let profile = {
            let b = Buf::read(index, PROFILE_BLK, Lock::Share);
            read_profile(b.page())
        };
        let bounds = {
            let b = Buf::read(index, BOUNDS_BLK, Lock::Share);
            read_bounds(b.page(), meta.nbuckets as usize)
        };
        Some(IndexFacts {
            meta,
            profile,
            bounds,
            index_pages: npages,
        })
    })();
    pg_sys::index_close(index, pg_sys::NoLock as i32);
    facts
}

/// Bucket interval implied by the constant index quals, if they are all simple constants.
unsafe fn const_interval(
    path: *mut pg_sys::IndexPath,
    facts: &IndexFacts,
) -> Option<Option<(usize, usize)>> {
    let nb = facts.meta.nbuckets as usize;
    let opfamily = *(*(*path).indexinfo).opfamily;
    let (mut lo, mut hi) = (0usize, nb - 1);
    let mut any = false;
    let clauses = PgList::<pg_sys::IndexClause>::from_pg((*path).indexclauses);
    for ic in clauses.iter_ptr() {
        let quals = PgList::<pg_sys::RestrictInfo>::from_pg((*ic).indexquals);
        for ri in quals.iter_ptr() {
            let clause = (*ri).clause as *mut pg_sys::Node;
            if !pgrx::is_a(clause, pg_sys::NodeTag::T_OpExpr) {
                return None;
            }
            let op = clause as *mut pg_sys::OpExpr;
            let args = PgList::<pg_sys::Node>::from_pg((*op).args);
            if args.len() != 2 {
                return None;
            }
            let rhs = args.get_ptr(1)?;
            if !pgrx::is_a(rhs, pg_sys::NodeTag::T_Const) {
                return None;
            }
            let c = rhs as *mut pg_sys::Const;
            if (*c).constisnull {
                return Some(None); // never matches
            }
            if domain_for_type((*c).consttype) != Some(facts.meta.domain) {
                return None;
            }
            let strat = pg_sys::get_op_opfamily_strategy((*op).opno, opfamily);
            if !(1..=5).contains(&strat) {
                return None;
            }
            let v = datum_to_key((*c).consttype, (*c).constvalue);
            let (l, h) = interval_for(&facts.bounds, nb, strat as u16, v);
            lo = lo.max(l);
            hi = hi.min(h);
            any = true;
            if lo > hi {
                return Some(None);
            }
        }
    }
    if any {
        Some(Some((lo, hi)))
    } else {
        None
    }
}

pub unsafe fn estimate(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    _loop_count: f64,
    startup: *mut f64,
    total: *mut f64,
    selectivity: *mut f64,
    correlation: *mut f64,
    pages: *mut f64,
) {
    let info = (*path).indexinfo;
    let rel = (*info).rel;

    // Row selectivity of the quals, from the ordinary statistics.
    let mut quals: *mut pg_sys::List = std::ptr::null_mut();
    {
        let clauses = PgList::<pg_sys::IndexClause>::from_pg((*path).indexclauses);
        for ic in clauses.iter_ptr() {
            let ql = PgList::<pg_sys::RestrictInfo>::from_pg((*ic).indexquals);
            for ri in ql.iter_ptr() {
                quals = pg_sys::lappend(quals, ri as *mut std::ffi::c_void);
            }
        }
    }
    let row_sel = pg_sys::clauselist_selectivity(
        root,
        quals,
        (*rel).relid as i32,
        pg_sys::JoinType::JOIN_INNER,
        std::ptr::null_mut(),
    );

    let facts = load_facts((*info).indexoid);
    let heap_pages = ((*rel).pages as f64).max(1.0);
    let heap_tuples = ((*rel).tuples).max(1.0);

    let (frac, nentries, index_pages) = match &facts {
        Some(f) => {
            let nb = f.meta.nbuckets as f64;
            let frac = match const_interval(path, f) {
                Some(None) => 0.0,
                Some(Some((lo, hi))) => interpolate_frac(&f.profile, (hi - lo + 1) as f64),
                None => interpolate_frac(&f.profile, (row_sel * nb).max(1.0)),
            };
            (
                frac.clamp(0.0, 1.0),
                f.meta.nentries as f64,
                f.index_pages as f64,
            )
        }
        None => (1.0, 1.0, 3.0), // unreadable index: be pessimistic
    };

    // The bitmap heap scan costing assumes the visited tuples are scattered at random
    // (Mackert-Lohman). Hippo visits whole contiguous page ranges, so for selective scans
    // report the selectivity that makes the planner's page estimate equal our predicted page
    // fraction `frac`; for broad scans (frac -> 1) fall back to `frac` itself so the CPU cost
    // of rechecking every tuple is not understated.
    let t = heap_pages;
    let inverted = (2.0 * frac * t / (2.0 - frac)) / heap_tuples;
    let blend = ((frac - 0.1) / 0.4).clamp(0.0, 1.0);
    let sel = ((1.0 - blend) * inverted + blend * frac).clamp(0.0, 1.0);

    *selectivity = sel;
    *correlation = 0.0;
    *pages = index_pages;
    *startup = 0.0;
    *total = pg_sys::seq_page_cost * index_pages
        + pg_sys::cpu_index_tuple_cost * nentries
        + pg_sys::cpu_operator_cost * nentries;
}
