//! Hippo: a sparse, histogram-based index access method for PostgreSQL.
//!
//! Implements the index structure of Yu & Sarwat, "Hippo: A Fast, yet Scalable, Database
//! Indexing Approach" (2016) as a loadable extension, with crash-safe (WAL-logged) maintenance,
//! a data-layout-aware cost model and support for PostgreSQL 14-18.
//!
//! Usage:
//! ```sql
//! CREATE EXTENSION hippo;
//! CREATE INDEX ON events USING hippo (created_at) WITH (buckets = 128, density = 20);
//! ```

#[cfg(not(target_pointer_width = "64"))]
compile_error!("hippo requires a 64-bit platform (key types must be pass-by-value)");

use pgrx::prelude::*;
use pgrx::{pg_sys, PgBox};
use std::ffi::c_void;

mod ambuild;
mod amcost;
mod aminsert;
mod amscan;
mod buf;
mod keymap;
mod layout;
mod options;

::pgrx::pg_module_magic!(name, version);

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    unsafe { options::register() };
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_build(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    ambuild::build(heap, index, info)
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_buildempty(_index: pg_sys::Relation) {
    error!("hippo indexes are not supported on unlogged tables");
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_insert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    info: *mut pg_sys::IndexInfo,
) -> bool {
    aminsert::insert(index, values, isnull, heap_tid, info)
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_bulkdelete(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    // Entries only over-approximate the live data, so deletions never invalidate the index.
    if stats.is_null() {
        pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
            as *mut pg_sys::IndexBulkDeleteResult
    } else {
        stats
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_vacuumcleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    if (*info).analyze_only {
        return stats;
    }
    let stats = if stats.is_null() {
        pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
            as *mut pg_sys::IndexBulkDeleteResult
    } else {
        stats
    };
    (*stats).num_pages = buf::nblocks((*info).index);
    (*stats).estimated_count = true;
    stats
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_costestimate(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    amcost::estimate(
        root,
        path,
        loop_count,
        startup,
        total,
        selectivity,
        correlation,
        pages,
    )
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_options(
    reloptions: pg_sys::Datum,
    validate: bool,
) -> *mut pg_sys::bytea {
    options::parse(reloptions, validate)
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_validate(_opclass: pg_sys::Oid) -> bool {
    true
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_beginscan(
    index: pg_sys::Relation,
    nkeys: std::ffi::c_int,
    norderbys: std::ffi::c_int,
) -> pg_sys::IndexScanDesc {
    pg_sys::RelationGetIndexScan(index, nkeys, norderbys)
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_rescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: std::ffi::c_int,
    _orderbys: pg_sys::ScanKey,
    _norderbys: std::ffi::c_int,
) {
    if !keys.is_null() && nkeys > 0 {
        std::ptr::copy(keys, (*scan).keyData, nkeys as usize);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_getbitmap(
    scan: pg_sys::IndexScanDesc,
    tbm: *mut pg_sys::TIDBitmap,
) -> i64 {
    amscan::get_bitmap(scan, tbm)
}

#[pg_guard]
unsafe extern "C-unwind" fn hippo_endscan(_scan: pg_sys::IndexScanDesc) {}

#[pg_extern(
    sql = "CREATE FUNCTION hippo_handler(internal) RETURNS index_am_handler PARALLEL SAFE IMMUTABLE STRICT LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';"
)]
fn hippo_handler() -> PgBox<pg_sys::IndexAmRoutine> {
    unsafe {
        let mut r = PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine);
        r.amstrategies = 5; // btree numbering: < <= = >= >
        r.amsupport = 0;
        r.amoptsprocnum = 0;
        r.amcanorder = false;
        r.amcanorderbyop = false;
        r.amcanbackward = false;
        r.amcanunique = false;
        r.amcanmulticol = false;
        r.amoptionalkey = false;
        r.amsearcharray = false;
        r.amsearchnulls = false;
        r.amstorage = false;
        r.amclusterable = false;
        r.ampredlocks = false;
        r.amcanparallel = false;
        r.amcaninclude = false;
        r.amusemaintenanceworkmem = false;
        // Summarizing index: HOT updates stay possible when the indexed column changes, because
        // an entry summarises whole pages and the new tuple version lives on the same page.
        #[cfg(any(feature = "pg16", feature = "pg17", feature = "pg18"))]
        {
            r.amsummarizing = true;
        }
        r.amparallelvacuumoptions = 0;
        r.amkeytype = pg_sys::InvalidOid;

        r.ambuild = Some(hippo_build);
        r.ambuildempty = Some(hippo_buildempty);
        r.aminsert = Some(hippo_insert);
        r.ambulkdelete = Some(hippo_bulkdelete);
        r.amvacuumcleanup = Some(hippo_vacuumcleanup);
        r.amcanreturn = None;
        r.amcostestimate = Some(hippo_costestimate);
        r.amoptions = Some(hippo_options);
        r.amproperty = None;
        r.ambuildphasename = None;
        r.amvalidate = Some(hippo_validate);
        r.amadjustmembers = None;
        r.ambeginscan = Some(hippo_beginscan);
        r.amrescan = Some(hippo_rescan);
        r.amgettuple = None;
        r.amgetbitmap = Some(hippo_getbitmap);
        r.amendscan = Some(hippo_endscan);
        r.ammarkpos = None;
        r.amrestrpos = None;
        r.amestimateparallelscan = None;
        r.aminitparallelscan = None;
        r.amparallelrescan = None;
        PgBox::from_pg(r.into_pg())
    }
}

#[pg_extern(
    sql = "CREATE FUNCTION hippo_index_stats(index regclass) RETURNS TABLE (metric text, value double precision) STRICT LANGUAGE c AS 'MODULE_PATHNAME', '@FUNCTION_NAME@';"
)]
fn hippo_index_stats(
    index: pg_sys::Oid,
) -> TableIterator<'static, (name!(metric, String), name!(value, f64))> {
    use buf::*;
    use layout::*;
    let rows: Vec<(String, f64)> = unsafe {
        let rel = pg_sys::index_open(index, pg_sys::AccessShareLock as i32);
        if buf::nblocks(rel) < FIRST_ENTRY_BLK {
            pg_sys::index_close(rel, pg_sys::NoLock as i32);
            error!("not a hippo index");
        }
        let meta = {
            let b = Buf::read(rel, META_BLK, Lock::Share);
            read_meta(b.page())
        };
        if meta.magic != MAGIC {
            pg_sys::index_close(rel, pg_sys::NoLock as i32);
            error!("not a hippo index");
        }
        let prof = {
            let b = Buf::read(rel, PROFILE_BLK, Lock::Share);
            read_profile(b.page())
        };
        let geom = Geometry::from_meta(&meta);
        let covered_blocks = if meta.nentries > 0 {
            let (blk, off) = geom.locate(meta.nentries - 1);
            let b = Buf::read(rel, blk, Lock::Share);
            entry_end(b.page(), off) as f64 + 1.0
        } else {
            0.0
        };
        let npages = buf::nblocks(rel);
        pg_sys::index_close(rel, pg_sys::NoLock as i32);
        let n = meta.nentries as f64;
        vec![
            ("format_version".to_string(), meta.version as f64),
            ("buckets".to_string(), meta.nbuckets as f64),
            ("density_threshold_pct".to_string(), meta.density_pct as f64),
            ("max_range_pages".to_string(), meta.max_range_pages as f64),
            ("entries".to_string(), n),
            ("entry_bytes".to_string(), meta.entry_size as f64),
            ("index_pages".to_string(), npages as f64),
            ("heap_pages_covered".to_string(), covered_blocks),
            (
                "heap_pages_at_build".to_string(),
                meta.heap_pages_built as f64,
            ),
            ("tuples_at_build".to_string(), meta.ntuples_built as f64),
            (
                "avg_heap_pages_per_entry".to_string(),
                if n > 0.0 { covered_blocks / n } else { 0.0 },
            ),
            (
                "avg_buckets_per_entry_at_build".to_string(),
                prof.mean_bits as f64,
            ),
            ("overlap_prob_1_bucket".to_string(), prof.frac[0] as f64),
            ("overlap_prob_8_buckets".to_string(), prof.frac[5] as f64),
            ("overlap_prob_32_buckets".to_string(), prof.frac[9] as f64),
        ]
    };
    TableIterator::new(rows.into_iter())
}

extension_sql!(
    r#"
REVOKE ALL ON FUNCTION hippo_index_stats(regclass) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION hippo_index_stats(regclass) TO pg_monitor;

CREATE ACCESS METHOD hippo TYPE INDEX HANDLER hippo_handler;
COMMENT ON ACCESS METHOD hippo IS 'sparse histogram-based page-range index (small, cheap to maintain)';

-- Operator classes. Strategy numbers follow btree: 1 <, 2 <=, 3 =, 4 >=, 5 >.
-- Cross-type operators are included for the integer and float families so that e.g.
-- "bigint_col > 5" (an int8/int4 comparison) can use the index.

CREATE OPERATOR CLASS int2_hippo_ops DEFAULT FOR TYPE int2 USING hippo AS
    OPERATOR 1 < (int2, int2), OPERATOR 2 <= (int2, int2), OPERATOR 3 = (int2, int2),
    OPERATOR 4 >= (int2, int2), OPERATOR 5 > (int2, int2),
    OPERATOR 1 < (int2, int4), OPERATOR 2 <= (int2, int4), OPERATOR 3 = (int2, int4),
    OPERATOR 4 >= (int2, int4), OPERATOR 5 > (int2, int4),
    OPERATOR 1 < (int2, int8), OPERATOR 2 <= (int2, int8), OPERATOR 3 = (int2, int8),
    OPERATOR 4 >= (int2, int8), OPERATOR 5 > (int2, int8);

CREATE OPERATOR CLASS int4_hippo_ops DEFAULT FOR TYPE int4 USING hippo AS
    OPERATOR 1 < (int4, int4), OPERATOR 2 <= (int4, int4), OPERATOR 3 = (int4, int4),
    OPERATOR 4 >= (int4, int4), OPERATOR 5 > (int4, int4),
    OPERATOR 1 < (int4, int2), OPERATOR 2 <= (int4, int2), OPERATOR 3 = (int4, int2),
    OPERATOR 4 >= (int4, int2), OPERATOR 5 > (int4, int2),
    OPERATOR 1 < (int4, int8), OPERATOR 2 <= (int4, int8), OPERATOR 3 = (int4, int8),
    OPERATOR 4 >= (int4, int8), OPERATOR 5 > (int4, int8);

CREATE OPERATOR CLASS int8_hippo_ops DEFAULT FOR TYPE int8 USING hippo AS
    OPERATOR 1 < (int8, int8), OPERATOR 2 <= (int8, int8), OPERATOR 3 = (int8, int8),
    OPERATOR 4 >= (int8, int8), OPERATOR 5 > (int8, int8),
    OPERATOR 1 < (int8, int2), OPERATOR 2 <= (int8, int2), OPERATOR 3 = (int8, int2),
    OPERATOR 4 >= (int8, int2), OPERATOR 5 > (int8, int2),
    OPERATOR 1 < (int8, int4), OPERATOR 2 <= (int8, int4), OPERATOR 3 = (int8, int4),
    OPERATOR 4 >= (int8, int4), OPERATOR 5 > (int8, int4);

CREATE OPERATOR CLASS float4_hippo_ops DEFAULT FOR TYPE float4 USING hippo AS
    OPERATOR 1 < (float4, float4), OPERATOR 2 <= (float4, float4), OPERATOR 3 = (float4, float4),
    OPERATOR 4 >= (float4, float4), OPERATOR 5 > (float4, float4),
    OPERATOR 1 < (float4, float8), OPERATOR 2 <= (float4, float8), OPERATOR 3 = (float4, float8),
    OPERATOR 4 >= (float4, float8), OPERATOR 5 > (float4, float8);

CREATE OPERATOR CLASS float8_hippo_ops DEFAULT FOR TYPE float8 USING hippo AS
    OPERATOR 1 < (float8, float8), OPERATOR 2 <= (float8, float8), OPERATOR 3 = (float8, float8),
    OPERATOR 4 >= (float8, float8), OPERATOR 5 > (float8, float8),
    OPERATOR 1 < (float8, float4), OPERATOR 2 <= (float8, float4), OPERATOR 3 = (float8, float4),
    OPERATOR 4 >= (float8, float4), OPERATOR 5 > (float8, float4);

CREATE OPERATOR CLASS date_hippo_ops DEFAULT FOR TYPE date USING hippo AS
    OPERATOR 1 < (date, date), OPERATOR 2 <= (date, date), OPERATOR 3 = (date, date),
    OPERATOR 4 >= (date, date), OPERATOR 5 > (date, date);

CREATE OPERATOR CLASS timestamp_hippo_ops DEFAULT FOR TYPE timestamp USING hippo AS
    OPERATOR 1 < (timestamp, timestamp), OPERATOR 2 <= (timestamp, timestamp),
    OPERATOR 3 = (timestamp, timestamp), OPERATOR 4 >= (timestamp, timestamp),
    OPERATOR 5 > (timestamp, timestamp);

CREATE OPERATOR CLASS timestamptz_hippo_ops DEFAULT FOR TYPE timestamptz USING hippo AS
    OPERATOR 1 < (timestamptz, timestamptz), OPERATOR 2 <= (timestamptz, timestamptz),
    OPERATOR 3 = (timestamptz, timestamptz), OPERATOR 4 >= (timestamptz, timestamptz),
    OPERATOR 5 > (timestamptz, timestamptz);
"#,
    name = "hippo_access_method",
    requires = [hippo_handler, hippo_index_stats]
);
