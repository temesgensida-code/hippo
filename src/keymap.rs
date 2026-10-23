//! Mapping of key datums to an order-preserving `i64`, plus bucket lookup.
//!
//! Every supported key type is pass-by-value on 64-bit platforms and totally ordered, so the
//! complete histogram can be stored as plain sorted `i64` split points and a bucket lookup is a
//! binary search over a slice: no catalog access, no comparison-function calls.

use crate::layout::{DOMAIN_FLOAT, DOMAIN_INT};
use pgrx::pg_sys;

/// Domain (integer-like or float-like) for an index key type, `None` if unsupported.
pub fn domain_for_type(oid: pg_sys::Oid) -> Option<u32> {
    if oid == pg_sys::INT2OID
        || oid == pg_sys::INT4OID
        || oid == pg_sys::INT8OID
        || oid == pg_sys::DATEOID
        || oid == pg_sys::TIMESTAMPOID
        || oid == pg_sys::TIMESTAMPTZOID
    {
        Some(DOMAIN_INT)
    } else if oid == pg_sys::FLOAT4OID || oid == pg_sys::FLOAT8OID {
        Some(DOMAIN_FLOAT)
    } else {
        None
    }
}

/// Order-preserving map of an f64 to i64. NaN sorts above everything (as in PostgreSQL),
/// -0.0 and +0.0 compare equal (as in PostgreSQL).
#[inline]
pub fn f64_key(x: f64) -> i64 {
    if x.is_nan() {
        return i64::MAX;
    }
    let x = if x == 0.0 { 0.0 } else { x };
    let bits = x.to_bits() as i64;
    if bits < 0 {
        bits ^ i64::MAX
    } else {
        bits
    }
}

/// Convert a datum of type `typ` to the key domain.
///
/// # Safety
/// `d` must be a valid, non-null datum of type `typ`.
#[inline]
pub unsafe fn datum_to_key(typ: pg_sys::Oid, d: pg_sys::Datum) -> i64 {
    let v = d.value();
    if typ == pg_sys::INT4OID || typ == pg_sys::DATEOID {
        (v as u32 as i32) as i64
    } else if typ == pg_sys::INT8OID || typ == pg_sys::TIMESTAMPOID || typ == pg_sys::TIMESTAMPTZOID
    {
        v as u64 as i64
    } else if typ == pg_sys::INT2OID {
        (v as u16 as i16) as i64
    } else if typ == pg_sys::FLOAT8OID {
        f64_key(f64::from_bits(v as u64))
    } else if typ == pg_sys::FLOAT4OID {
        f64_key(f32::from_bits(v as u32) as f64)
    } else {
        pgrx::error!("hippo: unsupported key type {:?}", typ)
    }
}

/// Bucket of value `v`: the number of split points strictly smaller than `v`.
/// This is monotone in `v`, which is all correctness needs.
#[inline]
pub fn bucket_of(bounds: &[i64], v: i64) -> usize {
    bounds.partition_point(|&b| b < v)
}

/// Inclusive bucket interval that can contain values satisfying `value <op> v`.
/// `strategy` uses the btree numbering: 1 `<`, 2 `<=`, 3 `=`, 4 `>=`, 5 `>`.
#[inline]
pub fn interval_for(bounds: &[i64], nbuckets: usize, strategy: u16, v: i64) -> (usize, usize) {
    let b = bucket_of(bounds, v);
    match strategy {
        1 | 2 => (0, b),
        3 => (b, b),
        _ => (b, nbuckets - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_order() {
        let xs = [
            f64::NEG_INFINITY,
            -1e300,
            -2.5,
            -0.0,
            0.0,
            1e-300,
            3.0,
            f64::INFINITY,
            f64::NAN,
        ];
        for w in xs.windows(2) {
            assert!(f64_key(w[0]) <= f64_key(w[1]), "{:?}", w);
        }
        assert_eq!(f64_key(-0.0), f64_key(0.0));
    }

    #[test]
    fn buckets_monotone() {
        let bounds = [10, 20, 30];
        assert_eq!(bucket_of(&bounds, 5), 0);
        assert_eq!(bucket_of(&bounds, 10), 0);
        assert_eq!(bucket_of(&bounds, 11), 1);
        assert_eq!(bucket_of(&bounds, 30), 2);
        assert_eq!(bucket_of(&bounds, 31), 3);
        assert_eq!(interval_for(&bounds, 4, 5, 20), (1, 3));
        assert_eq!(interval_for(&bounds, 4, 1, 20), (0, 1));
    }
}
