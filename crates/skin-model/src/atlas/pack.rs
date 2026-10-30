//! Packing cells into same-sized pages with `etagere`, and the choice of
//! the page side.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use etagere::{AllocatorOptions, AtlasAllocator, size2};

use super::{ALIGN, GUTTER, MIN_PAGE_SIDE};

/// The most cells one page's allocator takes.
///
/// `etagere` 0.3.0 numbers its items with `u16`, `u16::MAX` meaning none.
/// With one column and no deallocation, an allocator starts with one item
/// and each allocation adds at most two: one when it splits the free shelf
/// it lands on, one when it splits the free item it takes. So `N`
/// allocations make at most `1 + 2N` items, and `N = 32 767` keeps that
/// within 65 535.
pub(crate) const MAX_CELLS_PER_PAGE: usize = 32_767;

/// A cell's place: its page and its allocation (left, top, width, height
/// in texels), the cell's content `GUTTER` texels inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub page: u32,
    pub alloc: [u32; 4],
}

/// Cells packed at one side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Packing {
    pub side: u32,
    pub pages: u32,
    /// Per cell, in the order given.
    pub places: Vec<Place>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PackError {
    /// No side fits the cells in the pages allowed.
    TooManyPages,
    /// The allocator returned a place off the alignment.
    Misaligned,
}

fn round_up(n: u32, to: u32) -> Option<u32> {
    n.checked_add(to - 1).map(|n| n / to * to)
}

/// A cell's allocation size: its content with a gutter on every side,
/// rounded up to the alignment.
pub(crate) fn padded([w, h]: [u32; 2]) -> Option<[u32; 2]> {
    let pad = |n: u32| n.checked_add(2 * GUTTER).and_then(|n| round_up(n, ALIGN));
    Some([pad(w)?, pad(h)?])
}

/// The largest page side for `max_side`: the largest power of two not
/// above it (0 below 1).
pub(crate) fn side_cap(max_side: u32) -> u32 {
    if max_side == 0 {
        0
    } else {
        1 << (31 - max_side.leading_zeros())
    }
}

/// The candidate sides for cells of `padded` sizes: the powers of two from
/// [`MIN_PAGE_SIDE`] (or the largest cell rounded up) to `cap`.
pub(crate) fn candidates(padded: &[[u32; 2]], cap: u32) -> impl Iterator<Item = u32> {
    let largest = padded.iter().flatten().copied().max().unwrap_or(0);
    let start = largest.max(MIN_PAGE_SIDE).checked_next_power_of_two();
    std::iter::successors(start, |s| s.checked_mul(2)).take_while(move |&s| s <= cap)
}

/// The order cells are placed in: taller first, then wider, then by index.
pub(crate) fn order(padded: &[[u32; 2]]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..padded.len()).collect();
    order.sort_by_key(|&i| {
        let [w, h] = padded.get(i).copied().unwrap_or([0, 0]);
        (std::cmp::Reverse(h), std::cmp::Reverse(w), i)
    });
    order
}

fn allocator(side: i32) -> AtlasAllocator {
    let a = i32::try_from(ALIGN).unwrap_or(1);
    AtlasAllocator::with_options(
        size2(side, side),
        &AllocatorOptions {
            alignment: size2(a, a),
            vertical_shelves: false,
            num_columns: 1,
        },
    )
}

/// Packs cells of `padded` sizes in `order` at `side` into at most
/// `max_pages` pages, each taking at most `per_page` cells: each cell goes
/// to the first page with room. `None` when they do not fit.
pub(crate) fn pack_side(
    padded: &[[u32; 2]],
    order: &[usize],
    side: u32,
    max_pages: u32,
    per_page: usize,
) -> Result<Option<Packing>, PackError> {
    let Ok(side_i) = i32::try_from(side) else {
        return Ok(None);
    };
    let mut pages: Vec<(AtlasAllocator, usize)> = Vec::new();
    let mut places = vec![
        Place {
            page: 0,
            alloc: [0; 4],
        };
        padded.len()
    ];
    for &i in order {
        let Some(&[w, h]) = padded.get(i) else {
            continue;
        };
        let (Ok(w), Ok(h)) = (i32::try_from(w), i32::try_from(h)) else {
            return Ok(None);
        };
        let size = size2(w, h);
        let mut placed = None;
        for (page, (alloc, count)) in (0_u32..).zip(pages.iter_mut()) {
            if *count >= per_page {
                continue;
            }
            if let Some(a) = alloc.allocate(size) {
                *count += 1;
                placed = Some((page, a.rectangle));
                break;
            }
        }
        if placed.is_none() {
            let page = u32::try_from(pages.len()).unwrap_or(u32::MAX);
            if page >= max_pages || per_page == 0 {
                return Ok(None);
            }
            let mut alloc = allocator(side_i);
            let Some(a) = alloc.allocate(size) else {
                return Ok(None);
            };
            pages.push((alloc, 1));
            placed = Some((page, a.rectangle));
        }
        let Some((page, r)) = placed else {
            return Ok(None);
        };
        let rect = [r.min.x, r.min.y, r.width(), r.height()].map(|v| u32::try_from(v).ok());
        let [Some(x), Some(y), Some(rw), Some(rh)] = rect else {
            return Err(PackError::Misaligned);
        };
        let aligned = [x, y, rw, rh].iter().all(|v| v % ALIGN == 0);
        let [pw, ph] = [w, h].map(|v| u32::try_from(v).unwrap_or(u32::MAX));
        if !aligned || rw < pw || rh < ph || x + rw > side || y + rh > side {
            return Err(PackError::Misaligned);
        }
        if let Some(p) = places.get_mut(i) {
            *p = Place {
                page,
                alloc: [x, y, rw, rh],
            };
        }
    }
    Ok(Some(Packing {
        side,
        pages: u32::try_from(pages.len()).unwrap_or(u32::MAX),
        places,
    }))
}

/// The cost of a packing: every layer of the array, the reserved ones
/// included, at the side's texels.
pub(crate) fn cost(reserved: u32, pages: u32, side: u32) -> u64 {
    (u64::from(reserved) + u64::from(pages)) * u64::from(side) * u64::from(side)
}

/// Packs cells of `padded` sizes at the cheapest candidate side under this
/// packing: the fewest texels across all layers, ties going to fewer
/// pages, among the sides that fit `max_pages`.
pub(crate) fn pack(
    padded: &[[u32; 2]],
    cap: u32,
    max_pages: u32,
    reserved: u32,
) -> Result<Packing, PackError> {
    if padded.is_empty() {
        return Ok(Packing {
            side: MIN_PAGE_SIDE,
            pages: 0,
            places: Vec::new(),
        });
    }
    let order = order(padded);
    let mut best: Option<Packing> = None;
    for side in candidates(padded, cap) {
        let Some(p) = pack_side(padded, &order, side, max_pages, MAX_CELLS_PER_PAGE)? else {
            continue;
        };
        let better = best.as_ref().is_none_or(|b| {
            let (new, old) = (
                cost(reserved, p.pages, p.side),
                cost(reserved, b.pages, b.side),
            );
            new < old || (new == old && p.pages < b.pages)
        });
        if better {
            best = Some(p);
        }
    }
    best.ok_or(PackError::TooManyPages)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{MAX_CELLS_PER_PAGE, candidates, order, pack, pack_side, padded, side_cap};

    /// Whether two rectangles (left, top, width, height) overlap.
    fn overlap(a: [u32; 4], b: [u32; 4]) -> bool {
        a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
    }

    #[test]
    fn a_full_count_of_tiny_cells_splits_at_the_page_cap() {
        // 65 536 cells of 1×1, padded to 24×24: a 8 192 page holds 341
        // shelves of 341, far more than the cap, so the cap decides.
        let sizes = vec![padded([1, 1]).unwrap(); 65_536];
        assert_eq!(sizes[0], [24, 24]);
        let order = order(&sizes);
        let p = pack_side(&sizes, &order, 8_192, 64, MAX_CELLS_PER_PAGE)
            .unwrap()
            .unwrap();
        assert_eq!(MAX_CELLS_PER_PAGE, 32_767);
        assert_eq!(p.pages, 3);
        let mut per_page = [0_usize; 3];
        for place in &p.places {
            per_page[place.page as usize] += 1;
        }
        assert_eq!(per_page, [32_767, 32_767, 2]);
        // No two cells of a page overlap: sort by page, then position, and
        // compare neighbours on the same row of 24-texel slots.
        let mut seen = std::collections::HashSet::new();
        for place in &p.places {
            assert!(seen.insert((place.page, place.alloc[0], place.alloc[1])));
        }
    }

    #[test]
    fn the_largest_side_fits_etagere() {
        // etagere asserts each side fits u16 and the area i32.
        assert!(u16::try_from(super::super::MAX_PAGE_SIDE).is_ok());
        let area = i64::from(super::super::MAX_PAGE_SIDE).pow(2);
        assert!(i32::try_from(area).is_ok());
        assert_eq!(side_cap(8_192), 8_192);
        assert_eq!(side_cap(8_191), 4_096);
        assert_eq!(side_cap(300), 256);
    }

    /// Cells from raw values: sizes 1 to 600.
    fn cells() -> impl Strategy<Value = Vec<[u32; 2]>> {
        prop::collection::vec((0_u32..600, 0_u32..600), 1..300)
            .prop_map(|v| v.into_iter().map(|(w, h)| [w + 1, h + 1]).collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn packing_places_every_cell_once_aligned_and_apart(
            contents in cells(),
            side_exp in 9_u32..14,
            layers in 1_u32..20,
            reserved in 0_u32..3,
        ) {
            let cap = 1_u32 << side_exp;
            let sizes: Vec<[u32; 2]> = contents.iter().map(|&c| padded(c).unwrap()).collect();
            let largest = sizes.iter().flatten().copied().max().unwrap();
            prop_assume!(largest <= cap);
            let Ok(p) = pack(&sizes, cap, layers, reserved) else {
                // No candidate fits: every candidate fails the page cap.
                let order = order(&sizes);
                for side in candidates(&sizes, cap) {
                    let fit = pack_side(&sizes, &order, side, layers, MAX_CELLS_PER_PAGE).unwrap();
                    prop_assert!(fit.is_none());
                }
                return Ok(());
            };
            prop_assert!(p.pages >= 1 && p.pages <= layers);
            prop_assert!(p.side.is_power_of_two() && p.side >= 256 && p.side <= cap);
            prop_assert_eq!(p.places.len(), sizes.len());
            for (i, (place, size)) in p.places.iter().zip(&sizes).enumerate() {
                let [x, y, w, h] = place.alloc;
                prop_assert!(place.page < p.pages);
                prop_assert!(x % 8 == 0 && y % 8 == 0 && w % 8 == 0 && h % 8 == 0, "{:?}", place);
                prop_assert!(w >= size[0] && h >= size[1]);
                prop_assert!(x + w <= p.side && y + h <= p.side);
                for other in &p.places[..i] {
                    if other.page == place.page {
                        prop_assert!(!overlap(other.alloc, place.alloc));
                    }
                }
            }
            // The cheapest feasible side: texels over every layer,
            // reserved ones included, ties to fewer pages.
            let order = order(&sizes);
            let want = |pages: u32, side: u32| (u64::from(reserved + pages) * u64::from(side) * u64::from(side), pages);
            for side in candidates(&sizes, cap) {
                if let Some(q) = pack_side(&sizes, &order, side, layers, MAX_CELLS_PER_PAGE).unwrap() {
                    prop_assert!(want(p.pages, p.side) <= want(q.pages, q.side));
                }
            }
            // Deterministic.
            prop_assert_eq!(pack(&sizes, cap, layers, reserved).unwrap(), p);
        }
    }
}
