// SPDX-License-Identifier: Apache-2.0
//! Legalization — `diamondDPL`.
//!
//! Transcribed from OpenROAD `src/dpl/src/Place.cpp`.
//!
//! ⬜ **The placer itself is NOT built yet.** What is here are the two pieces that decide whether
//! a legalizer can be correlated at all, and that are testable without one: the order cells are
//! placed in, and the order the diamond search visits grid points.
//!
//! 🔑 **Both exist because upstream added an explicit tie-break to make them deterministic**, and
//! both would otherwise be unreproducible:
//!
//! - `CellPlaceOrderLess` ends in `strcmp` on the instance name;
//! - `diamondSearch`'s priority queue is keyed on `(manhattan_distance, sequence)`, where
//!   `sequence` is an insertion counter.
//!
//! ⛔ Drop either and equal-ranked items come out in unspecified container order — the result stays
//! *legal* and stops being *comparable*. That is the difference between an engine we can score and
//! one we can only eyeball, so they are transcribed first and pinned by test.

/// What the placement order needs to know about a cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    pub multi_row: bool,
    pub area: i64,
    /// Manhattan distance from the core centre to the cell's lower-left, in DBU.
    pub center_dist: i64,
    pub name: String,
}

/// `CellPlaceOrderLess::operator()` — is `a` placed before `b`?
///
/// The order, and every clause is load-bearing:
///
/// 1. **multi-row cells first** — they are the hardest to fit, so they choose before the grid
///    fills up around them;
/// 2. then **larger area first**, for the same reason;
/// 3. then **nearer the core centre first**;
/// 4. then **instance name**, which is the determinism tie-break.
pub fn place_before(a: &OrderKey, b: &OrderKey) -> bool {
    if a.multi_row != b.multi_row {
        return a.multi_row;
    }
    a.area > b.area
        || (a.area == b.area
            && (a.center_dist < b.center_dist
                || (a.center_dist == b.center_dist && a.name < b.name)))
}

/// Sort cells into the order `place()` uses.
pub fn sort_for_placement(cells: &mut [OrderKey]) {
    // ⚠️ `sort_by` with a strict-weak `less` mirrors `std::ranges::sort` with the comparator.
    cells.sort_by(|a, b| {
        if place_before(a, b) {
            std::cmp::Ordering::Less
        } else if place_before(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
}

/// The order `diamondSearch` visits grid points around `(cx, cy)`.
///
/// A best-first walk, not a ring-by-ring spiral: a min-heap keyed on
/// `(manhattan_distance, sequence)` over 4-neighbours, with a visited set.
///
/// ⚠️ **The neighbour list order is behaviour**, because it decides `sequence` and `sequence` is
/// the tie-break. Upstream's order is West, East, South, North —
/// `{-1,0}, {1,0}, {0,-1}, {0,1}` — and permuting it changes which of several equally distant
/// legal sites a cell lands on.
///
/// `bounds` is `(x_min, y_min, x_max, y_max)`, inclusive as upstream compares them.
/// `dist` is upstream's `calcDist`: DBU Manhattan, so a row's height and a site's width both matter
/// rather than raw grid steps.
pub fn diamond_points(
    cx: i64,
    cy: i64,
    bounds: (i64, i64, i64, i64),
    dist: &dyn Fn((i64, i64), (i64, i64)) -> i64,
    limit: usize,
) -> Vec<(i64, i64)> {
    use std::collections::{BinaryHeap, HashSet};
    use std::cmp::Reverse;

    let (x_min, y_min, x_max, y_max) = bounds;
    let mut heap: BinaryHeap<Reverse<(i64, usize, i64, i64)>> = BinaryHeap::new();
    let mut visited: HashSet<(i64, i64)> = HashSet::new();
    let mut seq = 0usize;
    let mut out = Vec::new();

    heap.push(Reverse((0, seq, cx, cy)));
    seq += 1;
    visited.insert((cx, cy));

    // West, East, South, North — upstream's order, and it decides `sequence`.
    const NEIGHBOURS: [(i64, i64); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];

    while let Some(Reverse((_, _, x, y))) = heap.pop() {
        out.push((x, y));
        if out.len() >= limit {
            break;
        }
        for (dx, dy) in NEIGHBOURS {
            let n = (x + dx, y + dy);
            if visited.contains(&n) {
                continue;
            }
            if n.0 < x_min || n.0 > x_max || n.1 < y_min || n.1 > y_max {
                continue;
            }
            visited.insert(n);
            heap.push(Reverse((dist((cx, cy), n), seq, n.0, n.1)));
            seq += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(multi: bool, area: i64, dist: i64, name: &str) -> OrderKey {
        OrderKey { multi_row: multi, area, center_dist: dist, name: name.into() }
    }

    #[test]
    fn multi_row_cells_are_placed_first_whatever_their_area() {
        // 🔑 Clause 1 beats clause 2: a TINY multi-row cell still precedes a huge single-row one.
        let small_multi = key(true, 1, 0, "a");
        let huge_single = key(false, 1_000_000, 0, "a");
        assert!(place_before(&small_multi, &huge_single));
        assert!(!place_before(&huge_single, &small_multi));
    }

    #[test]
    fn larger_area_precedes_smaller() {
        assert!(place_before(&key(false, 10, 0, "a"), &key(false, 5, 0, "a")));
    }

    #[test]
    fn equal_area_breaks_on_distance_then_on_name() {
        assert!(place_before(&key(false, 10, 1, "z"), &key(false, 10, 2, "a")),
                "nearer the core centre wins before the name is consulted");
        // ⛔ The determinism tie-break. Without it these two are indistinguishable and the order
        // is whatever the sort happens to do.
        assert!(place_before(&key(false, 10, 5, "aaa"), &key(false, 10, 5, "aab")));
        assert!(!place_before(&key(false, 10, 5, "aab"), &key(false, 10, 5, "aaa")));
    }

    #[test]
    fn the_sort_is_stable_against_input_order() {
        // Same cells, shuffled in: the output must be identical, which is what the name tie-break
        // buys and what makes a correlation possible at all.
        let mk = || vec![key(false, 10, 5, "b"), key(true, 1, 9, "m"), key(false, 10, 5, "a"),
                         key(false, 20, 0, "c")];
        let mut one = mk();
        let mut two = mk();
        two.reverse();
        sort_for_placement(&mut one);
        sort_for_placement(&mut two);
        assert_eq!(one, two);
        assert_eq!(one.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
                   ["m", "c", "a", "b"]);
    }

    fn manhattan(a: (i64, i64), b: (i64, i64)) -> i64 {
        (a.0 - b.0).abs() + (a.1 - b.1).abs()
    }

    #[test]
    fn the_search_starts_at_the_centre_and_grows_by_distance() {
        let pts = diamond_points(0, 0, (-5, -5, 5, 5), &manhattan, 9);
        assert_eq!(pts[0], (0, 0), "the cell's own position is tried first");
        // Distances must be non-decreasing — that is what "best-first" means here.
        let d: Vec<i64> = pts.iter().map(|p| manhattan((0, 0), *p)).collect();
        assert!(d.windows(2).all(|w| w[0] <= w[1]), "distances not monotonic: {d:?}");
    }

    #[test]
    fn equally_distant_points_come_out_in_insertion_order() {
        // ⛔ The four neighbours of the centre are all at distance 1, so their order is decided
        // ENTIRELY by `sequence`, i.e. by the neighbour list. West, East, South, North.
        let pts = diamond_points(0, 0, (-5, -5, 5, 5), &manhattan, 5);
        assert_eq!(&pts[1..5], &[(-1, 0), (1, 0), (0, -1), (0, 1)]);
    }

    #[test]
    fn the_search_stays_inside_its_bounds() {
        let pts = diamond_points(0, 0, (0, 0, 1, 1), &manhattan, 99);
        assert_eq!(pts.len(), 4, "a 2x2 box has four points and the search must not leave it");
        for p in &pts {
            assert!((0..=1).contains(&p.0) && (0..=1).contains(&p.1), "{p:?} is out of bounds");
        }
    }

    #[test]
    fn a_visited_point_is_never_queued_twice() {
        // Every point of the box appears exactly once.
        let pts = diamond_points(2, 2, (0, 0, 4, 4), &manhattan, 999);
        let uniq: std::collections::HashSet<_> = pts.iter().collect();
        assert_eq!(uniq.len(), pts.len());
        assert_eq!(pts.len(), 25);
    }
}

// ── the legalizer ────────────────────────────────────────────────────────────────────────────

use crate::grid::Grid;
use vyges_opendb::Db;

/// One cell the legalizer may move.
#[derive(Debug, Clone)]
pub struct Movable {
    pub name: String,
    pub key: OrderKey,
    /// Core-relative starting position, in DBU.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub site: String,
}

/// Where a cell ended up.
#[derive(serde::Serialize, Debug, Clone)]
pub struct Placed {
    pub name: String,
    /// ABSOLUTE DBU, ready to write back — core offset already added.
    pub x: i32,
    pub y: i32,
    pub orient: String,
    pub moved: bool,
}

/// The result of a legalization run.
#[derive(serde::Serialize, Debug, Default)]
pub struct Legalized {
    pub placed: Vec<Placed>,
    pub failures: Vec<String>,
    pub not_done: Vec<String>,
}

/// Families of behaviour this legalizer does NOT implement.
///
/// ⛔ Named on every run. A legalizer that silently skips `ripUpAndReplace` reports fewer failures
/// than it earned — the cells it could not seat would have been retried upstream.
pub const NOT_DONE: &[&str] = &[
    "rip_up_and_replace", "groups_and_regions", "padding", "one_site_gaps",
    "legalPt hopeless/block-edge refinement",
];

/// `Opendp::diamondDPL` — legalize every movable cell.
///
/// **The call sequence, and each step is upstream's:**
///
/// 1. `initGrid` — the row/pixel model;
/// 2. `setFixedGridCells` — **paint the FIXED cells in first**, so the search sees them as
///    occupied. ⛔ Skip this and every cell legalizes into a macro;
/// 3. collect the movable cells — `Node::CELL`, `master->isCore()`, and not
///    (fixed ∨ in a group ∨ already placed);
/// 4. **sort** with [`place_before`];
/// ✅ **CORRELATED 2026-09-02 on `fragmented_row04`** — one cell in a row cutout. Our result is
/// **byte-identical to upstream's `.defok`**: `_277_ BUF_X4 + PLACED ( 8360 2800 ) FS`, same
/// coordinates and same orientation, and the REFERENCE's own `check_placement` accepts it.
///
/// 5. for each: `diamondMove` — a diamond search from its current grid point for the nearest
///    square where `checkPixels` passes — then `placeCell`, which paints the pixels and
///    ⚠️ **takes the ORIENTATION from the row it landed in**.
pub fn legalize(db: &Db) -> Result<Legalized, String> {
    let mut grid = Grid::build(db)?;
    let core = grid.core;
    let mut out = Legalized {
        not_done: NOT_DONE.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };

    let center = ((core.0 + core.2) / 2, (core.1 + core.3) / 2);
    let mut movable: Vec<Movable> = Vec::new();

    for i in 0..db.num_insts() {
        let name = db.nth_inst_name(i);
        let master = db.inst_master(&name);
        let (x, y) = db.inst_location(&name);
        let (w, h) = (db.master_get_width(&master) as i32, db.master_get_height(&master) as i32);
        let mtype = db.master_get_type(&master).unwrap_or_default();
        let fixed = db.inst_get_placement_status(&name) == "FIRM"
            || db.inst_get_placement_status(&name) == "LOCKED"
            || mtype.contains("BLOCK");

        if fixed {
            // ⛔ Step 2: fixed cells occupy pixels before anything is searched.
            grid.paint(x - core.0, y - core.1, w, h, false);
            continue;
        }
        // ⚠️ `master->isCore()` — pads, endcaps and cover cells are not the legalizer's business.
        if !mtype.contains("CORE") {
            continue;
        }
        let site = db.master_get_site(&master);
        let area = w as i64 * h as i64;
        // ⛔ **Transcribed exactly, including the frame mismatch.** `CellPlaceOrderLess` computes
        // `abs(cell->getLeft() - center_x_)` where `getLeft()` is CORE-RELATIVE (see
        // `updateDbInstLocations`, which adds `core_.xMin()` back) while `center_x_` is the
        // ABSOLUTE core centre. So it ranks by distance from a point that is not the core centre
        // in either frame.
        //
        // ⚠️ **Do not "correct" this.** It is deterministic and it decides the order cells claim
        // sites in, so the ranking IS the behaviour. Using the absolute position on both sides —
        // which is what this line did first — gives a different order and cascades: measured on
        // `gcd`, 265 of 549 cells landed elsewhere, 114 of them with a flipped orientation
        // because they had moved to a row of the opposite parity.
        let dist = ((x - core.0 - center.0).abs() + (y - core.1 - center.1).abs()) as i64;
        // A cell taller than one grid row is multi-row.
        let multi_row = grid.rows_spanned(y - core.1, h) > 1;
        movable.push(Movable {
            key: OrderKey { multi_row, area, center_dist: dist, name: name.clone() },
            name, x: x - core.0, y: y - core.1, w, h, site,
        });
    }

    movable.sort_by(|a, b| {
        if place_before(&a.key, &b.key) { std::cmp::Ordering::Less }
        else if place_before(&b.key, &a.key) { std::cmp::Ordering::Greater }
        else { std::cmp::Ordering::Equal }
    });

    // `max_displacement_x_ = 500, max_displacement_y_ = 100` when the command passes 0.
    let (max_dx, max_dy) = (500i64, 100i64);
    let sw = grid.site_width as i64;
    let row_y = grid.row_y.clone();
    let dist_dbu = move |a: (i64, i64), b: (i64, i64)| -> i64 {
        let ya = *row_y.get(a.1.max(0) as usize).unwrap_or(&0) as i64;
        let yb = *row_y.get(b.1.max(0) as usize).unwrap_or(&0) as i64;
        (a.0 - b.0).abs() * sw + (ya - yb).abs()
    };

    for m in &movable {
        // `legalGridPt(cell, padded)` — clamp into the core and round, THEN search from there.
        let (gx, gy) = grid.legal_start(m.x, m.y, m.w, m.h);
        let bounds = (
            (gx - max_dx).max(0),
            (gy - max_dy).max(0),
            (gx + max_dx).min(grid.row_site_count as i64 - 1),
            (gy + max_dy).min(grid.row_count as i64 - 1),
        );
        let cells_wide = (m.w + grid.site_width - 1) / grid.site_width;

        let mut seated = None;
        for (px, py) in diamond_points(gx, gy, bounds, &dist_dbu, 200_000) {
            if grid.can_place(px, py, cells_wide as i64, m.h, &m.site) {
                seated = Some((px, py));
                break;
            }
        }
        match seated {
            Some((px, py)) => {
                let nx = px as i32 * grid.site_width;
                let ny = grid.row_y[py as usize];
                grid.paint(nx, ny, m.w, m.h, true);
                let orient = grid.site_orient_at(px, py, &m.site).unwrap_or_else(|| "R0".into());
                out.placed.push(Placed {
                    name: m.name.clone(),
                    x: nx + core.0,
                    y: ny + core.1,
                    orient,
                    moved: nx != m.x || ny != m.y,
                });
            }
            None => out.failures.push(m.name.clone()),
        }
    }
    Ok(out)
}
