// SPDX-License-Identifier: Apache-2.0
//! Legalization — `diamondDPL`.
//!
//! Transcribed from OpenROAD `src/dpl/src/Place.cpp`.
//!
//! [`legalize`] is `diamondDPL`, groups and regions included; what it does not build is named in
//! [`NOT_DONE`]. It rests on two pieces that decide whether a legalizer can be correlated at all:
//! the order cells are placed in, and the order the diamond search visits grid points.
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

    fn cell(x: i32, y: i32, w: i32, h: i32) -> Movable {
        Movable {
            name: "c".into(), x, y, w, h, site: "S".into(),
            key: OrderKey { multi_row: false, area: 0, center_dist: 0, name: "c".into() },
        }
    }

    /// Traced on `diamond_regions` (`dpl-diamond-group-trace.py`, `VYGG|nearestPt`): `f1/_284_`
    /// at (7980, 0), 760 × 2800, toward `er1` = (0, 2800, 7000, 5600) → upstream (6240, 2800); and
    /// `f1/_283_` from (7600, 0) → the same point. Not overlapping, so each axis is clamped in.
    #[test]
    fn nearest_pt_matches_the_reference_trace() {
        let er1 = (0, 2800, 7000, 5600);
        assert!(!check_overlap(&cell(7980, 0, 760, 2800), &er1));
        assert_eq!(nearest_pt(&cell(7980, 0, 760, 2800), &er1), (6240, 2800));
        assert_eq!(nearest_pt(&cell(7600, 0, 760, 2800), &er1), (6240, 2800));
    }

    /// Upstream `checkOverlap(cell, rect)` tests the LEFT edge only — `x < rect.xl < x + w` — so a
    /// cell straddling the RIGHT edge is not "overlapping" in `prePlace`'s sense.
    #[test]
    fn check_overlap_is_the_left_edge_only() {
        let r = (1000, 0, 2000, 100);
        assert!(check_overlap(&cell(900, 0, 200, 100), &r), "straddles the left edge");
        assert!(!check_overlap(&cell(1900, 0, 200, 100), &r), "straddles the RIGHT edge: no");
        assert!(!check_overlap(&cell(1200, 0, 200, 100), &r), "wholly inside: no");
    }

    /// Upstream `nearestPt` on an overlapping cell moves along the axis with the SMALLER distance
    /// (`dist_x < dist_y`, so a tie moves in Y), to just outside the rect's left or below it.
    #[test]
    fn nearest_pt_on_an_overlapping_cell_steps_out_on_the_shorter_axis() {
        let r = (1000, 0, 2000, 1000);
        // x: |900+200-1000| = 100 vs |2000-900| = 1100 → left, dist 100, temp_x 800.
        // y: |0+100-0| = 100 vs |1000-0| = 1000 → below, dist 0, temp_y -100.
        assert_eq!(nearest_pt(&cell(900, 0, 200, 100), &r), (900, -100), "dist_y 0 < dist_x 100");
    }

    /// Upstream `distToRect`: Manhattan distance outside the rect, 0 inside.
    #[test]
    fn dist_to_rect_is_zero_inside_and_manhattan_outside() {
        let r = (1000, 1000, 2000, 2000);
        assert_eq!(dist_to_rect(&cell(1200, 1200, 100, 100), &r), 0);
        assert_eq!(dist_to_rect(&cell(900, 2000, 100, 100), &r), 100 + 100);
    }

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
    /// `None` = leave the instance's orientation alone; upstream sets it only when the site
    /// actually offers one (`if (orient.has_value())` in `commitNegotiationPosToDpl`).
    pub orient: Option<String>,
    pub moved: bool,
    /// Where the cell STARTED, in grid units (site column, row).
    ///
    /// 🔑 **Reported because the final position alone cannot be debugged.** A cell that ends at
    /// site 0 is either a legalizer working from a start of 0 or one that dragged the cell across
    /// the die, and those are different bugs. Measured 2026-09-02: `fragmented_row04` ended at
    /// site 0 with `moved: false`, and only the start told us which.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_grid: Option<(i32, i32)>,
    /// The cell's footprint in grid units, `(sites, rows)`.
    ///
    /// 🔑 **Reported because the sweep ORDER is keyed on it** — `sortByNegotiationOrder` is
    /// `(overuse DESC, height ASC, width ASC, index ASC)`. When two runs disagree about which
    /// cell got which slot, the height is the first thing to compare, and it cannot be inferred
    /// from a position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footprint: Option<(i32, i32)>,
}

/// The result of a legalization run.
#[derive(serde::Serialize, Debug, Default)]
pub struct Legalized {
    pub placed: Vec<Placed>,
    pub failures: Vec<String>,
    pub not_done: Vec<String>,
    /// Instances the MODEL FILTER excluded, counted by `"<master type>/<placement status>"`.
    ///
    /// ⛔ **Reported on every run, because a filter that drops instances silently is
    /// indistinguishable from a design that has none.** `dbMaster::isCoreAutoPlaceable` and the
    /// placement status decide what enters the model at all; anything they reject has no cell, no
    /// blockade, no occupancy and no capacity, and every number the engine goes on to report is
    /// computed as though it were not there.
    ///
    /// ⚠️ **Measured on `gcd`**: its 255 `CORE WELLTAP` tap cells matched no arm of the filter —
    /// the master types arrive from odb in LEF spelling, `"CORE WELLTAP"`, where the C++ enum
    /// reads `CORE_WELLTAP` — so the engine saw a design with no fixed cells whatsoever. Three
    /// correct fixes in a row failed to move the case before this line was printed; it named the
    /// cause immediately.
    ///
    /// 🔑 This is the [`vacuous`] guard for the model itself: a pass must never come from a run
    /// that quietly legalized a smaller design than it was given.
    pub filtered_out: std::collections::BTreeMap<String, usize>,
}

/// Families of behaviour this legalizer does NOT implement.
///
/// ⛔ Named on every run. A legalizer that silently skips `ripUpAndReplace` reports fewer failures
/// than it earned — the cells it could not seat would have been retried upstream.
pub const NOT_DONE: &[&str] = &[
    "rip_up_and_replace", "padding", "one_site_gaps",
    "legalPt hopeless/block-edge refinement",
    // ⚠️ `placeGroups2`'s fallback when a group's cells do not all seat: its cells are reported as
    // failures instead. Never reached upstream on `diamond_regions` (traced: no `brick`).
    "groups: brickPlace1/brickPlace2",
    // ⚠️ `groupRefine` and `anneal` (random swaps, `mt19937` through boost's
    // `uniform_int_distribution`). Traced on `diamond_regions`: both RUN and move NOTHING
    // (`refine 0`, `anneal 0` for both groups), so skipping them is exact there and nowhere else
    // is known.
    "groups: groupRefine/anneal",
];

/// `Opendp::diamondDPL` — legalize every movable cell.
///
/// **The call sequence, and each step is upstream's** — one function per stage below:
///
/// 1. `initGrid`, then `setFixedGridCells` — **paint the FIXED cells in first**, so the search sees
///    them as occupied. ⛔ Skip this and every cell legalizes into a macro;
/// 2. `groupInitPixels2` + `groupInitPixels` — region boundaries blocked, group squares marked;
/// 3. `placeGroups`, when any group has a region: `groupAssignCellRegions`, [`pre_place_groups`],
///    [`pre_place`], [`place_groups2`] (then `groupRefine`/`anneal`, not built — see [`NOT_DONE`]);
/// 4. `place` — the cells not in a group and not yet placed, sorted with [`place_before`], each
///    `diamondMove`d from its `legalGridPt`.
///
/// ✅ **CORRELATED 2026-09-02 on `fragmented_row04`** — one cell in a row cutout. Our result is
/// **byte-identical to upstream's `.defok`**: `_277_ BUF_X4 + PLACED ( 8360 2800 ) FS`.
///
/// `diamondMove` is a diamond search from a grid point for the nearest square where `checkPixels`
/// passes, then `placeCell`, which paints the pixels and ⚠️ **takes the ORIENTATION from the row
/// it landed in**.
pub fn legalize(db: &Db) -> Result<Legalized, String> {
    let mut d = Diamond::init(db)?;
    d.group_init_pixels();
    if !d.groups.is_empty() {
        d.place_groups();
    }
    d.place();
    Ok(d.out)
}

/// The diamond legalizer's state: the grid, the movable cells, and the groups.
struct Diamond {
    grid: Grid,
    out: Legalized,
    /// Every movable CORE cell, grouped or not, in DB order.
    cells: Vec<Movable>,
    index: std::collections::HashMap<String, usize>,
    /// `Node::getGroup` — the cell's group index, for grouped cells.
    group_of: Vec<Option<usize>>,
    placed: Vec<bool>,
    groups: Vec<crate::regions::Group>,
    regions: crate::regions::Regions,
}

impl Diamond {
    /// `initGrid` + `setFixedGridCells`, and the movable cells with their place-order keys.
    fn init(db: &Db) -> Result<Diamond, String> {
        let mut grid = Grid::build(db)?;
        let core = grid.core;
        let out = Legalized {
            not_done: NOT_DONE.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let center = ((core.0 + core.2) / 2, (core.1 + core.3) / 2);
        let mut cells: Vec<Movable> = Vec::new();
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
                // ⛔ Fixed cells occupy pixels before anything is searched.
                grid.paint(x - core.0, y - core.1, w, h, false);
                continue;
            }
            // ⚠️ `master->isCore()` — pads, endcaps and cover cells are not the legalizer's
            // business.
            if !mtype.contains("CORE") {
                continue;
            }
            let site = db.master_get_site(&master);
            let area = w as i64 * h as i64;
            // ⛔ **Transcribed exactly, including the frame mismatch.** `CellPlaceOrderLess`
            // computes `abs(cell->getLeft() - center_x_)` where `getLeft()` is CORE-RELATIVE while
            // `center_x_` is the ABSOLUTE core centre. So it ranks by distance from a point that
            // is not the core centre in either frame.
            //
            // ⚠️ **Do not "correct" this.** It decides the order cells claim sites in. Measured on
            // `gcd`: the absolute position on both sides moved 265 of 549 cells.
            let dist = ((x - core.0 - center.0).abs() + (y - core.1 - center.1).abs()) as i64;
            // A cell taller than one grid row is multi-row.
            let multi_row = grid.rows_spanned(y - core.1, h) > 1;
            cells.push(Movable {
                key: OrderKey { multi_row, area, center_dist: dist, name: name.clone() },
                name, x: x - core.0, y: y - core.1, w, h, site,
            });
        }
        let index = cells.iter().enumerate().map(|(i, m)| (m.name.clone(), i)).collect();
        // `setUpPlacementGroups`, over the network's cells.
        let groups = crate::regions::placement_groups(db, core, &crate::network::network_insts(db));
        let mut d = Diamond {
            grid, out, index, group_of: vec![None; cells.len()], placed: vec![false; cells.len()],
            cells, groups, regions: Default::default(),
        };
        for (gi, g) in d.groups.iter().enumerate() {
            for n in &g.cells {
                if let Some(&i) = d.index.get(n) {
                    d.group_of[i] = Some(gi);
                }
            }
        }
        Ok(d)
    }

    /// `groupInitPixels2` + `groupInitPixels` — called unconditionally by `diamondDPL`; with no
    /// group they change nothing a later stage reads.
    fn group_init_pixels(&mut self) {
        crate::regions::group_init_pixels(&mut self.grid, &self.groups);
    }

    /// `placeGroups`.
    fn place_groups(&mut self) {
        self.group_assign_cell_regions();
        self.pre_place_groups();
        self.pre_place();
        self.place_groups2();
        // `groupRefine` / `anneal` — not built; see `NOT_DONE`.
    }

    /// `groupAssignCellRegions` — each grouped cell's region, from its INITIAL location.
    fn group_assign_cell_regions(&mut self) {
        let loc: std::collections::HashMap<&str, crate::regions::Box4> =
            self.cells.iter().map(|m| (m.name.as_str(), (m.x, m.y, m.w, m.h))).collect();
        self.regions = crate::regions::Regions::from_groups(&self.groups, &|n| loc.get(n).copied());
    }

    /// `prePlaceGroups` — a grouped cell whose initial location is inside NONE of its group's rects
    /// is moved toward the NEAREST rect (`distToRect`, first minimum) and held there.
    fn pre_place_groups(&mut self) {
        for gi in 0..self.groups.len() {
            for n in self.groups[gi].cells.clone() {
                let Some(&i) = self.index.get(&n) else { continue }; // fixed
                if self.placed[i] {
                    continue;
                }
                let m = &self.cells[i];
                let rects = &self.groups[gi].rects;
                let in_group = rects.iter().any(|r| is_inside(m, r));
                let mut dist = i64::MAX;
                let mut nearest_rect = None;
                for r in rects {
                    let d = dist_to_rect(m, r);
                    if d < dist {
                        dist = d;
                        nearest_rect = Some(*r);
                    }
                }
                let Some(rect) = nearest_rect else { continue };
                if !in_group {
                    let (nx, ny) = nearest_pt(m, &rect);
                    let start = self.grid.legal_start(nx, ny, m.w, m.h);
                    self.diamond_move(i, start);
                }
            }
        }
    }

    /// `prePlace` — an UNGROUPED cell whose initial location straddles a rect's LEFT edge
    /// (`checkOverlap(cell, rect)`, as written: `x < rect.xl < x + w`) is moved off the LAST such
    /// rect over every group. Visited in network order.
    fn pre_place(&mut self) {
        let mut order: Vec<usize> = (0..self.cells.len()).collect();
        order.sort_by(|&a, &b| self.cells[a].name.cmp(&self.cells[b].name));
        for i in order {
            if self.group_of[i].is_some() || self.placed[i] {
                continue;
            }
            let m = &self.cells[i];
            let mut group_rect = None;
            for g in &self.groups {
                for r in &g.rects {
                    if check_overlap(m, r) {
                        group_rect = Some(*r);
                    }
                }
            }
            if let Some(rect) = group_rect {
                let (nx, ny) = nearest_pt(m, &rect);
                let start = self.grid.legal_start(nx, ny, m.w, m.h);
                self.diamond_move(i, start);
            }
        }
    }

    /// `placeGroups2` — per group, its unplaced cells in `CellPlaceOrderLess` order, each
    /// `diamondMove`d; the first failure stops the group.
    fn place_groups2(&mut self) {
        for gi in 0..self.groups.len() {
            let mut group_cells: Vec<usize> = self.groups[gi].cells.iter()
                .filter_map(|n| self.index.get(n).copied())
                .filter(|&i| !self.placed[i])
                .collect();
            sort_indices(&mut group_cells, &self.cells);
            let mut pass = true;
            for &i in &group_cells {
                if !self.placed[i] {
                    let m = &self.cells[i];
                    let start = self.grid.legal_start(m.x, m.y, m.w, m.h);
                    pass = self.diamond_move(i, start);
                    if !pass {
                        break;
                    }
                }
            }
            if !pass {
                // ⬜ `brickPlace1/2` not built: the group's unseated cells are failures.
                for n in &self.groups[gi].cells {
                    if let Some(&i) = self.index.get(n) {
                        if !self.placed[i] {
                            self.out.failures.push(n.clone());
                        }
                    }
                }
            }
        }
    }

    /// `place` — every cell not in a group and not yet placed, sorted, each `diamondMove`d from
    /// its `legalGridPt`.
    fn place(&mut self) {
        let mut order: Vec<usize> = (0..self.cells.len())
            .filter(|&i| self.group_of[i].is_none() && !self.placed[i])
            .collect();
        sort_indices(&mut order, &self.cells);
        for i in order {
            let m = &self.cells[i];
            // `legalGridPt(cell, padded)` — clamp into the core and round, THEN search from there.
            let start = self.grid.legal_start(m.x, m.y, m.w, m.h);
            if !self.diamond_move(i, start) {
                self.out.failures.push(self.cells[i].name.clone());
            }
        }
    }

    /// `diamondMove(cell, grid_pt)` — `diamondSearch`, then `placeCell` on a hit.
    fn diamond_move(&mut self, i: usize, start: (i64, i64)) -> bool {
        match self.diamond_search(i, start) {
            Some((px, py)) => {
                self.place_cell(i, px, py);
                true
            }
            None => false,
        }
    }

    /// `diamondSearch` — best-first from `start` over the displacement budget, restricted to the
    /// group's bounding box for a grouped cell, accepting the first square `canBePlaced` passes.
    fn diamond_search(&self, i: usize, (gx, gy): (i64, i64)) -> Option<(i64, i64)> {
        // `max_displacement_x_ = 500, max_displacement_y_ = 100` when the command passes 0.
        let (max_dx, max_dy) = (500i64, 100i64);
        let (mut x_min, mut y_min, mut x_max, mut y_max) =
            (gx - max_dx, gy - max_dy, gx + max_dx, gy + max_dy);
        // "Restrict search to group boundary": `gridWithin(group->getBBox())`, and each corner
        // moved to `closestPtInside` it — an INCLUSIVE clamp, `min(max(v, lo), hi)`.
        if let Some(bb) = self.group_of[i].and_then(|g| self.groups[g].bbox()) {
            let (wx0, wy0, wx1, wy1) = crate::regions::grid_within(&self.grid, &bb);
            let inside = |x: i64, y: i64| (x.max(wx0).min(wx1), y.max(wy0).min(wy1));
            (x_min, y_min) = inside(x_min, y_min);
            (x_max, y_max) = inside(x_max, y_max);
        }
        let bounds = (
            x_min.max(0),
            y_min.max(0),
            x_max.min(self.grid.row_site_count as i64 - 1),
            y_max.min(self.grid.row_count as i64 - 1),
        );
        let sw = self.grid.site_width as i64;
        let row_y = &self.grid.row_y;
        let dist_dbu = |a: (i64, i64), b: (i64, i64)| -> i64 {
            let ya = *row_y.get(a.1.max(0) as usize).unwrap_or(&0) as i64;
            let yb = *row_y.get(b.1.max(0) as usize).unwrap_or(&0) as i64;
            (a.0 - b.0).abs() * sw + (ya - yb).abs()
        };
        diamond_points(gx, gy, bounds, &dist_dbu, 200_000)
            .into_iter()
            .find(|&(px, py)| self.can_be_placed(i, px, py))
    }

    /// `canBePlaced` → `checkPixels`: `checkRegionOverlap`, then every square valid, empty, and in
    /// the cell's own group (or in none, for an ungrouped cell), the first row offering its site.
    fn can_be_placed(&self, i: usize, px: i64, py: i64) -> bool {
        let m = &self.cells[i];
        let g = &self.grid;
        let cells_wide = (m.w + g.site_width - 1) / g.site_width;
        if !self.groups.is_empty() {
            let x_end = px + cells_wide as i64;
            let y_end = g.grid_end_y(py, m.h);
            if !self.regions.overlap_ok(&m.name, px as i32, py as i32, x_end as i32, y_end as i32,
                                        g.site_width, &g.row_y, g.core.3) {
                return false;
            }
            for y in py..y_end {
                for x in px..x_end {
                    if g.pixel(x, y).and_then(|p| p.group) != self.group_of[i] {
                        return false;
                    }
                }
            }
        }
        g.can_place(px, py, cells_wide as i64, m.h, &m.site)
    }

    /// `placeCell` — paint, mark placed, take the row's orientation.
    fn place_cell(&mut self, i: usize, px: i64, py: i64) {
        let core = self.grid.core;
        let m = &self.cells[i];
        let nx = px as i32 * self.grid.site_width;
        let ny = self.grid.row_y[py as usize];
        self.grid.paint(nx, ny, m.w, m.h, true);
        let orient = Some(self.grid.site_orient_at(px, py, &m.site).unwrap_or_else(|| "R0".into()));
        self.out.placed.push(Placed {
            name: m.name.clone(),
            x: nx + core.0,
            y: ny + core.1,
            orient,
            moved: nx != m.x || ny != m.y,
            init_grid: None,
            footprint: None,
        });
        self.placed[i] = true;
    }
}

/// `CellPlaceOrderLess` over indices into `cells`.
fn sort_indices(idx: &mut [usize], cells: &[Movable]) {
    idx.sort_by(|&a, &b| {
        if place_before(&cells[a].key, &cells[b].key) { std::cmp::Ordering::Less }
        else if place_before(&cells[b].key, &cells[a].key) { std::cmp::Ordering::Greater }
        else { std::cmp::Ordering::Equal }
    });
}

/// `Opendp::isInside(cell, rect)` — the INITIAL footprint wholly inside, inclusive.
fn is_inside(m: &Movable, r: &crate::regions::Box4) -> bool {
    m.x >= r.0 && m.x + m.w <= r.2 && m.y >= r.1 && m.y + m.h <= r.3
}

/// `Opendp::checkOverlap(cell, rect)` as written: the initial footprint straddles the rect's LEFT
/// edge (`x < xl < x + w`) and overlaps it vertically. ⚠️ Not a general overlap test.
fn check_overlap(m: &Movable, r: &crate::regions::Box4) -> bool {
    m.x + m.w > r.0 && m.x < r.0 && m.y + m.h > r.1 && m.y < r.3
}

/// `Opendp::distToRect` — the initial location's Manhattan distance outside the rect.
fn dist_to_rect(m: &Movable, r: &crate::regions::Box4) -> i64 {
    let dx = if m.x < r.0 { r.0 - m.x } else if m.x + m.w > r.2 { m.x + m.w - r.2 } else { 0 };
    let dy = if m.y < r.1 { r.1 - m.y } else if m.y + m.h > r.3 { m.y + m.h - r.3 } else { 0 };
    dx as i64 + dy as i64
}

/// `Opendp::nearestPt` — where to start a cell's search so it lands in `rect`, from its initial
/// location.
fn nearest_pt(m: &Movable, r: &crate::regions::Box4) -> (i32, i32) {
    let (x, y, w, h) = (m.x, m.y, m.w, m.h);
    if check_overlap(m, r) {
        let (dist_x, temp_x) = if (x + w - r.0).abs() > (r.2 - x).abs() {
            ((r.2 - x).abs(), r.2)
        } else {
            ((x - r.0).abs(), r.0 - w)
        };
        let (dist_y, temp_y) = if (y + h - r.1).abs() > (r.3 - y).abs() {
            ((r.3 - y).abs(), r.3)
        } else {
            ((y - r.1).abs(), r.1 - h)
        };
        return if dist_x < dist_y { (temp_x, y) } else { (x, temp_y) };
    }
    let tx = if x < r.0 { r.0 } else if x + w > r.2 { r.2 - w } else { x };
    let ty = if y < r.1 { r.1 } else if y + h > r.3 { r.3 - h } else { y };
    (tx, ty)
}
