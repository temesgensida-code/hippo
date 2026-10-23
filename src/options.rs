//! Index storage parameters: `WITH (buckets = N, density = P)`.

use crate::layout::*;
use pgrx::pg_sys;
use std::ffi::c_int;
use std::sync::atomic::{AtomicU32, Ordering};

static RELOPT_KIND: AtomicU32 = AtomicU32::new(0);

#[repr(C)]
pub struct HippoOptions {
    vl_len_: i32,
    pub buckets: c_int,
    pub density: c_int,
    pub max_range_pages: c_int,
}

/// Register the reloption kind and parameters. Called from `_PG_init`.
pub unsafe fn register() {
    let kind = pg_sys::add_reloption_kind();
    RELOPT_KIND.store(kind as u32, Ordering::Relaxed);
    pg_sys::add_int_reloption(
        kind,
        c"buckets".as_ptr(),
        c"Resolution of the complete histogram (number of buckets)".as_ptr(),
        DEFAULT_BUCKETS,
        MIN_BUCKETS,
        MAX_BUCKETS as i32,
        pg_sys::AccessExclusiveLock as i32,
    );
    pg_sys::add_int_reloption(
        kind,
        c"density".as_ptr(),
        c"Partial histogram density threshold in percent (page ranges stop growing above it)"
            .as_ptr(),
        DEFAULT_DENSITY,
        1,
        100,
        pg_sys::AccessExclusiveLock as i32,
    );
    pg_sys::add_int_reloption(
        kind,
        c"max_range_pages".as_ptr(),
        c"Maximum number of heap pages summarised by one index entry".as_ptr(),
        DEFAULT_MAX_RANGE_PAGES,
        1,
        131072,
        pg_sys::AccessExclusiveLock as i32,
    );
}

/// `amoptions` callback.
pub unsafe fn parse(reloptions: pg_sys::Datum, validate: bool) -> *mut pg_sys::bytea {
    let tab = [
        pg_sys::relopt_parse_elt {
            optname: c"buckets".as_ptr(),
            opttype: pg_sys::relopt_type::RELOPT_TYPE_INT,
            offset: std::mem::offset_of!(HippoOptions, buckets) as c_int,
            ..Default::default()
        },
        pg_sys::relopt_parse_elt {
            optname: c"density".as_ptr(),
            opttype: pg_sys::relopt_type::RELOPT_TYPE_INT,
            offset: std::mem::offset_of!(HippoOptions, density) as c_int,
            ..Default::default()
        },
        pg_sys::relopt_parse_elt {
            optname: c"max_range_pages".as_ptr(),
            opttype: pg_sys::relopt_type::RELOPT_TYPE_INT,
            offset: std::mem::offset_of!(HippoOptions, max_range_pages) as c_int,
            ..Default::default()
        },
    ];
    pg_sys::build_reloptions(
        reloptions,
        validate,
        RELOPT_KIND.load(Ordering::Relaxed) as pg_sys::relopt_kind::Type,
        std::mem::size_of::<HippoOptions>(),
        tab.as_ptr(),
        tab.len() as c_int,
    ) as *mut pg_sys::bytea
}

/// (buckets, density_pct, max_range_pages) in effect for `index`.
pub unsafe fn for_index(index: pg_sys::Relation) -> (usize, u32, u32) {
    let o = (*index).rd_options as *const HippoOptions;
    if o.is_null() {
        (
            DEFAULT_BUCKETS as usize,
            DEFAULT_DENSITY as u32,
            DEFAULT_MAX_RANGE_PAGES as u32,
        )
    } else {
        (
            (*o).buckets as usize,
            (*o).density as u32,
            (*o).max_range_pages as u32,
        )
    }
}
