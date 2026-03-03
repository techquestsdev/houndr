use roaring::RoaringBitmap;
use std::io::Cursor;

/// Intersect a list of posting bitmaps using smallest-first ordering
/// with early termination.
pub fn intersect_postings(mut bitmaps: Vec<&RoaringBitmap>) -> RoaringBitmap {
    if bitmaps.is_empty() {
        return RoaringBitmap::new();
    }
    if bitmaps.len() == 1 {
        return bitmaps[0].clone();
    }

    // Sort by cardinality — smallest first for fastest intersection
    bitmaps.sort_by_key(|b| b.len());

    let mut result = bitmaps[0].clone();
    for bitmap in &bitmaps[1..] {
        result &= *bitmap;
        if result.is_empty() {
            break; // Early termination
        }
    }
    result
}

/// Intersect a base bitmap with a set of serialized Roaring bitmaps.
///
/// Uses `intersection_with_serialized_unchecked` to avoid fully deserializing
/// each serialized bitmap — only internal containers that overlap with the
/// base bitmap's containers are deserialized.
///
/// Slices are sorted by length ascending (proxy for cardinality) with
/// early termination if the base becomes empty.
///
/// # Safety
/// Uses unchecked deserialization — callers must ensure the serialized
/// data has been validated (e.g., via whole-file checksum).
pub fn intersect_with_serialized(mut base: RoaringBitmap, serialized: &[&[u8]]) -> RoaringBitmap {
    if base.is_empty() {
        return base;
    }

    let mut sorted: Vec<&[u8]> = serialized.to_vec();
    sorted.sort_by_key(|s| s.len());

    for slice in sorted {
        base = match base.intersection_with_serialized_unchecked(Cursor::new(slice)) {
            Ok(result) => result,
            Err(_) => return RoaringBitmap::new(),
        };
        if base.is_empty() {
            break;
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_empty() {
        let result = intersect_postings(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn intersect_single() {
        let mut a = RoaringBitmap::new();
        a.insert(1);
        a.insert(2);
        let result = intersect_postings(vec![&a]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn intersect_two() {
        let mut a = RoaringBitmap::new();
        a.extend([1, 2, 3, 4]);
        let mut b = RoaringBitmap::new();
        b.extend([2, 4, 6]);
        let result = intersect_postings(vec![&a, &b]);
        assert_eq!(result.len(), 2);
        assert!(result.contains(2));
        assert!(result.contains(4));
    }

    #[test]
    fn intersect_disjoint() {
        let mut a = RoaringBitmap::new();
        a.extend([1, 2]);
        let mut b = RoaringBitmap::new();
        b.extend([3, 4]);
        let mut c = RoaringBitmap::new();
        c.extend([1, 3, 5]);
        // a ∩ b = empty, should terminate early
        let result = intersect_postings(vec![&a, &b, &c]);
        assert!(result.is_empty());
    }

    #[test]
    fn smallest_first_ordering() {
        // Large bitmap intersected with small should still work correctly
        let mut large = RoaringBitmap::new();
        large.extend(0..10000);
        let mut small = RoaringBitmap::new();
        small.extend([5, 50, 500]);
        let result = intersect_postings(vec![&large, &small]);
        assert_eq!(result.len(), 3);
    }

    fn serialize_bitmap(bm: &RoaringBitmap) -> Vec<u8> {
        let mut buf = Vec::new();
        bm.serialize_into(&mut buf).unwrap();
        buf
    }

    #[test]
    fn intersect_with_serialized_basic() {
        let mut base = RoaringBitmap::new();
        base.extend([1, 2, 3, 4, 5]);

        let mut b = RoaringBitmap::new();
        b.extend([2, 4, 6]);
        let buf_b = serialize_bitmap(&b);

        let mut c = RoaringBitmap::new();
        c.extend([1, 2, 4, 8]);
        let buf_c = serialize_bitmap(&c);

        let slices: Vec<&[u8]> = vec![&buf_b, &buf_c];
        let result = intersect_with_serialized(base, &slices);
        // {1,2,3,4,5} ∩ {2,4,6} ∩ {1,2,4,8} = {2,4}
        assert_eq!(result.len(), 2);
        assert!(result.contains(2));
        assert!(result.contains(4));
    }

    #[test]
    fn intersect_with_serialized_empty_base() {
        let base = RoaringBitmap::new();
        let mut b = RoaringBitmap::new();
        b.extend([1, 2]);
        let buf = serialize_bitmap(&b);
        let result = intersect_with_serialized(base, &[&buf]);
        assert!(result.is_empty());
    }

    #[test]
    fn intersect_with_serialized_early_termination() {
        let mut base = RoaringBitmap::new();
        base.extend([10, 20]);

        let mut b = RoaringBitmap::new();
        b.extend([30, 40]); // disjoint from base
        let buf_b = serialize_bitmap(&b);

        let mut c = RoaringBitmap::new();
        c.extend([10, 30]);
        let buf_c = serialize_bitmap(&c);

        // After intersecting with b, base is empty; c shouldn't matter
        let result = intersect_with_serialized(base, &[&buf_b, &buf_c]);
        assert!(result.is_empty());
    }

    #[test]
    fn intersect_with_serialized_matches_intersect_postings() {
        // Verify that intersect_with_serialized produces the same result
        // as intersect_postings for the same inputs.
        let mut a = RoaringBitmap::new();
        a.extend(0..1000);
        let mut b = RoaringBitmap::new();
        b.extend((500..1500).step_by(2));
        let mut c = RoaringBitmap::new();
        c.extend([500, 502, 600, 700, 998, 999]);

        let expected = intersect_postings(vec![&a, &b, &c]);

        let buf_b = serialize_bitmap(&b);
        let buf_c = serialize_bitmap(&c);
        let actual = intersect_with_serialized(a, &[&buf_b, &buf_c]);

        assert_eq!(expected, actual);
    }
}
