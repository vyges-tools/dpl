// SPDX-License-Identifier: Apache-2.0
//! Placement regions (fences) as the checker reads them — `setUpPlacementGroups`,
//! `groupAssignCellRegions` and `checkRegionPlacement` / `checkRegionOverlap`.
//!
//! A `dbGroup` with a `dbRegion` becomes a group: its boundaries clipped to the core and made
//! core-relative, each also entered in an R-tree SHRUNK by one DBU on its high edges ("to prevent
//! imaginary overlaps where a region ends and another starts"). A group's instances that are in
//! the network are its cells; each cell's region is the LAST of the group's rects its initial
//! location lies inside, else the first.

use vyges_opendb::Db;

use crate::grid::Grid;

pub type Box4 = (i32, i32, i32, i32);

/// One placement group — `setUpPlacementGroups`' `Group`: a `dbGroup` that HAS a region.
#[derive(Debug, Clone, Default)]
pub struct Group {
    pub name: String,
    /// The region's boundaries, each `intersect(core)` and made core-relative, in odb order.
    pub rects: Vec<Box4>,
    /// The group's instances that are in the network, in `getInsts` order.
    pub cells: Vec<String>,
}

impl Group {
    /// `Group::getBBox` — the merge of its rects (`mergeInit` then `merge`), core-relative.
    pub fn bbox(&self) -> Option<Box4> {
        self.rects.iter().copied().reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
    }
}

/// `Grid::gridWithin` — the grid squares a DBU rect (core-relative) wholly covers:
/// `dbuToGridCeil`, `gridEndY`, `dbuToGridFloor`, `gridSnapDownY`. Half-open.
pub fn grid_within(grid: &Grid, r: &Box4) -> (i64, i64, i64, i64) {
    let sw = grid.site_width;
    ((r.0 + sw - 1).div_euclid(sw) as i64, grid.grid_end_y_dbu(r.1) as i64,
     r.2.div_euclid(sw) as i64, grid.grid_snap_down_y(r.3).unwrap_or(0) as i64)
}

/// `setUpPlacementGroups` — every `dbGroup` with a region, in `getGroups` order; its index here is
/// upstream's `Group::id`. `network` is the network's cells (`network_->getNode` non-null).
pub fn placement_groups(db: &Db, core: Box4, network: &[String]) -> Vec<Group> {
    let in_network: std::collections::HashSet<&str> = network.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for group in db.block_get_groups() {
        let region = db.group_get_region(&group);
        if region.is_empty() {
            continue;
        }
        let rects = db
            .region_boundaries(&region)
            .unwrap_or_default()
            .into_iter()
            .map(|b| {
                // `box.intersect(core_)`, then `moveDelta(-core.xMin, -core.yMin)`.
                let r = (b.0.max(core.0), b.1.max(core.1), b.2.min(core.2), b.3.min(core.3));
                (r.0 - core.0, r.1 - core.1, r.2 - core.0, r.3 - core.1)
            })
            .collect();
        let cells = db
            .group_get_insts(&group)
            .into_iter()
            .filter(|i| in_network.contains(i.as_str()))
            .collect();
        out.push(Group { name: group, rects, cells });
    }
    out
}

/// The checker's view of regions: the R-tree of every group rect, and each grouped cell's region.
#[derive(Debug, Clone, Default)]
pub struct Regions {
    /// Every region rect in the R-tree, `(x_lo, y_lo, x_hi - 1, y_hi - 1)`, core-relative.
    rtree: Vec<Box4>,
    /// Each grouped cell's assigned region rect, core-relative.
    assigned: std::collections::HashMap<String, Box4>,
}

impl Regions {
    /// `setUpPlacementGroups` + `groupAssignCellRegions`. `network` is the checker's cells;
    /// `location(inst)` a cell's `(x, y, w, h)`, core-relative (`initialLocation`).
    pub fn build(db: &Db, core: Box4, network: &[String], location: &dyn Fn(&str) -> Option<Box4>) -> Regions {
        Regions::from_groups(&placement_groups(db, core, network), location)
    }

    pub fn from_groups(groups: &[Group], location: &dyn Fn(&str) -> Option<Box4>) -> Regions {
        let mut out = Regions::default();
        for g in groups {
            // "the -1 is to prevent imaginary overlaps where a region ends and another starts".
            out.rtree.extend(g.rects.iter().map(|r| (r.0, r.1, r.2 - 1, r.3 - 1)));
            if g.rects.is_empty() {
                continue;
            }
            for inst in &g.cells {
                let Some((x, y, w, h)) = location(inst) else { continue };
                // `isInside(cell, rect)` over every rect — the LAST that holds it wins, else the
                // first.
                let mut chosen = None;
                for r in &g.rects {
                    if x >= r.0 && x + w <= r.2 && y >= r.1 && y + h <= r.3 {
                        chosen = Some(*r);
                    }
                }
                out.assigned.insert(inst.clone(), chosen.unwrap_or(g.rects[0]));
            }
        }
        out
    }

    /// `checkRegionOverlap` for a cell with NO region (`getRegion() == nullptr`): the query box
    /// must hit no region at all. Grid coordinates; `row_y` every row boundary.
    ///
    /// ⛔ **This is every cell on the negotiation path**, grouped ones included: upstream calls
    /// `groupAssignCellRegions` from `checkPlacement` and the diamond legalizer's `placeGroups`,
    /// never from the negotiation legalizer, so `canBePlaced` meets no region anywhere. Traced:
    /// every `checkRegionOverlap` call with a region on the region designs comes from the
    /// `check_placement` that follows, none from `detailed_placement`. ⬜ No case's negotiation
    /// reaches `diamondRecovery`, so this branch has no golden.
    pub fn no_region_overlap(&self, gx: i32, gy: i32, gx_end: i32, gy_end: i32, site_width: i32,
                             row_y: &[i32], core_y_max: i32) -> bool {
        let row = |i: i32| match i.max(0) as usize {
            n if n < row_y.len() => Some(row_y[n]),
            n if n == row_y.len() => Some(core_y_max),
            _ => None,
        };
        let (Some(y0), Some(y1)) = (row(gy), row(gy_end)) else { return false };
        let q = (gx * site_width, y0, gx_end * site_width - 1, y1 - 1);
        !self.rtree.iter().any(|b| b.0 <= q.2 && q.0 <= b.2 && b.1 <= q.3 && q.1 <= b.3)
    }

    pub fn is_empty(&self) -> bool {
        self.rtree.is_empty()
    }

    /// `checkRegionOverlap(cell, x, y, x_end, y_end)` in GRID coordinates, for whichever case the
    /// cell is in: with a region (assigned by `groupAssignCellRegions`) the query must hit exactly
    /// one R-tree box and be covered by it; without one it must hit none.
    pub fn overlap_ok(&self, inst: &str, gx: i32, gy: i32, gx_end: i32, gy_end: i32,
                      site_width: i32, row_y: &[i32], core_y_max: i32) -> bool {
        let row = |i: i32| match i.max(0) as usize {
            n if n < row_y.len() => Some(row_y[n]),
            n if n == row_y.len() => Some(core_y_max),
            _ => None,
        };
        let (Some(y0), Some(y1)) = (row(gy), row(gy_end)) else { return false };
        let q = (gx * site_width, y0, gx_end * site_width - 1, y1 - 1);
        let hits: Vec<&Box4> = self.rtree.iter()
            .filter(|b| b.0 <= q.2 && q.0 <= b.2 && b.1 <= q.3 && q.1 <= b.3).collect();
        if self.assigned.contains_key(inst) {
            hits.len() == 1 && q.0 >= hits[0].0 && q.1 >= hits[0].1 && q.2 <= hits[0].2 && q.3 <= hits[0].3
        } else {
            hits.is_empty()
        }
    }

    /// `checkRegionPlacement` — true for a cell with no region. Otherwise its region must CONTAIN
    /// its box, and `checkRegionOverlap` must find exactly one R-tree box, covering the query.
    ///
    /// ⚠️ Transcribed as written: the query's Y indices are `y / cell height`, not a row lookup,
    /// read back through `gridYToDbu` (`row_y`, every row boundary); X is `x / site width`. The
    /// query box is `[x·site, row_y[y]] .. [x_end·site − 1, row_y[y_end] − 1]`, and R-tree
    /// `intersects` and `covered_by` are both inclusive.
    ///
    /// ⛔ `gridYToDbu` of the index ONE PAST the last boundary is the core's ABSOLUTE `yMax`,
    /// not a core-relative Y — upstream mixes the two frames there, so it is `core_y_max` here.
    /// Any index beyond that throws upstream (`.at`); it fails the cell here.
    pub fn check(&self, inst: &str, x: i32, y: i32, w: i32, h: i32, site_width: i32, row_y: &[i32],
                 core_y_max: i32) -> bool {
        let Some(region) = self.assigned.get(inst) else { return true };
        let (x_end, y_end) = (x + w, y + h);
        if !(x >= region.0 && y >= region.1 && x_end <= region.2 && y_end <= region.3) {
            return false;
        }
        if site_width <= 0 || h <= 0 {
            return false;
        }
        self.overlap_ok(inst, x / site_width, y / h, x_end / site_width, y_end / h, site_width,
                        row_y, core_y_max)
    }
}

/// `groupInitPixels2` then `groupInitPixels`, as `detailedPlacement` calls them before the
/// negotiation legalizer when any group has a region. Writes `is_valid` and `group`; the dummy cell
/// upstream also stamps is cleared again by `buildGrid` before anything reads it, so it is not kept.
///
/// ⛔ **Transcribed with its quirks, which the reference trace shows are live:**
/// - a group's fractional LEFT edge is subtracted from `gridWithin`'s CEILED first column — a
///   square wholly inside the region — so that square ends at util 0.5 and is made INVALID
///   (`fence01`: square 16, inside `er1`, is invalid in both rows);
/// - the fractional RIGHT edge subtracts `((site − xh) % site) / site`, and C++ `%` of that
///   NEGATIVE operand is negative, so util RISES to 1.5 and the square is left as it was: valid,
///   and in no group (`fence01`: square 14 is in neither);
/// - a square at util exactly 1 is made VALID, whatever made it invalid before.
///
/// Traced on the region designs: every square this reports matches `VYGR|pix` (`fence01` 4
/// invalid, 28 + 28 grouped; `regions3` 5 invalid, 9 + 22).
pub fn group_init_pixels(grid: &mut Grid, groups: &[Group]) {
    let (sw, rows, sites) = (grid.site_width, grid.row_count, grid.row_site_count);
    // `groupInitPixels2`: a square that OVERLAPS a rect (strictly) without lying INSIDE it
    // (inclusively) straddles a region boundary, and is blocked.
    for x in 0..sites {
        for y in 0..rows {
            let sub = (x as i32 * sw, grid.row_y[y], (x as i32 + 1) * sw, grid.row_y[y + 1]);
            let straddles = groups.iter().flat_map(|g| g.rects.iter()).any(|r| {
                let inside = sub.0 >= r.0 && sub.2 <= r.2 && sub.1 >= r.1 && sub.3 <= r.3;
                let overlap = r.0 < sub.2 && r.2 > sub.0 && r.1 < sub.3 && r.3 > sub.1;
                !inside && overlap
            });
            if straddles {
                let p = grid.pixel_mut(x as i64, y as i64).unwrap();
                p.util = 0.0;
                p.is_valid = false;
            }
        }
    }
    // `groupInitPixels`.
    for x in 0..sites {
        for y in 0..rows {
            grid.pixel_mut(x as i64, y as i64).unwrap().util = 0.0;
        }
    }
    for (gid, g) in groups.iter().enumerate() {
        if g.cells.is_empty() {
            continue;
        }
        let within = grid_within;
        for r in &g.rects {
            let (xlo, ylo, xhi, yhi) = within(grid, r);
            for k in ylo..yhi {
                for l in xlo..xhi {
                    if let Some(p) = grid.pixel_mut(l, k) {
                        p.util += 1.0;
                    }
                }
                if r.0 % sw != 0 {
                    if let Some(p) = grid.pixel_mut(xlo, k) {
                        p.util -= (r.0 % sw) as f64 / sw as f64;
                    }
                }
                if r.2 % sw != 0 {
                    if let Some(p) = grid.pixel_mut(xhi - 1, k) {
                        // ⛔ C++ `%`: truncating, so NEGATIVE here — Rust's `%` agrees.
                        p.util -= ((sw - r.2) % sw) as f64 / sw as f64;
                    }
                }
            }
        }
        for r in &g.rects {
            let (xlo, ylo, xhi, yhi) = within(grid, r);
            for k in ylo..yhi {
                for l in xlo..xhi {
                    let Some(p) = grid.pixel_mut(l, k) else { continue };
                    if p.util == 1.0 {
                        p.group = Some(gid);
                        p.is_valid = true;
                    } else if p.util > 0.0 && p.util < 1.0 {
                        p.util = 0.0;
                        p.is_valid = false;
                    }
                }
            }
        }
    }
}

/// `initFenceRegions` — the negotiation legalizer's OWN region model, separate from the groups.
///
/// ⛔ **Every `dbRegion`, not only those a group names, and NOT clipped to the core.** Each boundary
/// becomes grid units from the core origin — `(x − core.xMin) / site` TRUNCATED, `gridEndY` below,
/// `gridSnapDownY` above — so a region reaching past the core has NEGATIVE or past-the-grid bounds.
/// Traced on `regions1`: `er0` is `(-73, 0, 104, 1)` on a 31-site grid.
#[derive(Debug, Clone, Default)]
pub struct Fences {
    rects: Vec<Vec<Box4>>,
    /// Each cell's fence index (`NegCell::fence_id`); a cell absent here is in the default region.
    pub cell_fence: std::collections::HashMap<String, usize>,
}

impl Fences {
    pub fn build(db: &Db, grid: &Grid, cells: &[String]) -> Fences {
        let (core, sw) = (grid.core, grid.site_width);
        let mut out = Fences::default();
        let mut index_of: std::collections::HashMap<String, usize> = Default::default();
        for region in db.block_get_regions() {
            let rects: Vec<Box4> = db
                .region_boundaries(&region)
                .unwrap_or_default()
                .into_iter()
                .map(|b| ((b.0 - core.0) / sw, grid.grid_end_y_dbu(b.1 - core.1) as i32,
                          (b.2 - core.0) / sw, grid.grid_snap_down_y(b.3 - core.1).unwrap_or(0) as i32))
                .collect();
            if !rects.is_empty() {
                index_of.insert(region, out.rects.len());
                out.rects.push(rects);
            }
        }
        // "Try direct region first, then group-based region."
        for c in cells {
            let mut region = db.inst_get_region(c);
            if region.is_empty() {
                let group = db.inst_get_group(c);
                if !group.is_empty() {
                    region = db.group_get_region(&group);
                }
            }
            if let Some(&fi) = index_of.get(&region) {
                out.cell_fence.insert(c.clone(), fi);
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// `FenceRegion::contains` — the footprint `[x, x+w) × [y, y+h)` inside at least ONE rect.
    fn contains(&self, fi: usize, x: i32, y: i32, w: i32, h: i32) -> bool {
        self.rects[fi].iter().any(|r| x >= r.0 && y >= r.1 && x + w <= r.2 && y + h <= r.3)
    }

    /// `respectsFence`. A fenced cell must lie inside its fence. A cell in the default region
    /// must not lie INSIDE any fence — ⚠️ `contains`, as upstream writes it, although its comment
    /// says "must not overlap": a default cell straddling a fence edge passes.
    pub fn respects(&self, cell: &str, x: i32, y: i32, w: i32, h: i32) -> bool {
        match self.cell_fence.get(cell) {
            Some(&fi) => self.contains(fi, x, y, w, h),
            None => !(0..self.rects.len()).any(|fi| self.contains(fi, x, y, w, h)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two abutting regions, one grouped cell assigned to the left one. Site 10, rows 0/100/200.
    fn two_regions() -> Regions {
        let mut r = Regions::default();
        r.rtree = vec![(0, 0, 99, 199), (100, 0, 199, 199)];
        r.assigned.insert("u1".into(), (0, 0, 100, 200));
        r
    }
    const ROWS: &[i32] = &[0, 100, 200];

    /// Upstream: a cell with no region always passes `checkRegionPlacement`.
    #[test]
    fn ungrouped_cell_passes() {
        assert!(two_regions().check("other", 150, 0, 40, 100, 10, ROWS, 200));
    }

    /// Upstream: `region->contains(cell box)` is INCLUSIVE, and the R-tree's `-1` keeps a cell
    /// that ends exactly on the shared edge from also hitting the neighbour region.
    #[test]
    fn cell_flush_with_the_shared_edge_passes() {
        assert!(two_regions().check("u1", 60, 0, 40, 100, 10, ROWS, 200));
    }

    /// Upstream: a cell that pokes past its region fails `contains`.
    #[test]
    fn cell_past_its_region_fails() {
        assert!(!two_regions().check("u1", 70, 0, 40, 100, 10, ROWS, 200));
    }

    /// Upstream: `checkRegionOverlap` needs EXACTLY one R-tree hit; a query touching two fails
    /// even when `contains` passed. Here the assigned region is wide but the rtree is split.
    #[test]
    fn query_hitting_two_rtree_boxes_fails() {
        let mut r = two_regions();
        r.assigned.insert("u2".into(), (0, 0, 200, 200));
        assert!(!r.check("u2", 80, 0, 40, 100, 10, ROWS, 200));
    }

    /// Upstream `gridYToDbu`: the index one past the last boundary is the ABSOLUTE core yMax.
    /// A cell reaching the top row boundary with `y_end / h == row_y.len()` reads it; with the
    /// core at absolute y 1000, the query ends at 999 and falls outside the region box.
    #[test]
    fn one_past_the_last_boundary_reads_absolute_core_top() {
        let mut r = Regions::default();
        r.rtree = vec![(0, 0, 99, 299)];
        r.assigned.insert("u".into(), (0, 0, 100, 300));
        // h = 100, y = 200: y_end / h = 3 == len → core_y_max.
        assert!(r.check("u", 0, 200, 40, 100, 10, &[0, 100, 200], 300));
        assert!(!r.check("u", 0, 200, 40, 100, 10, &[0, 100, 200], 1000));
    }

    // ── groupInitPixels, pinned to the reference's own pixels ─────────────────────────────────
    //
    // The expected sets are `VYGR|pix` from an instrumented reference run (`dpl-region-trace.py`)
    // on the upstream designs: core (28000, 28000)–(39780, 33600), site 380, two 2800 rows.

    fn nangate_grid() -> Grid {
        Grid::uniform_for_test((28000, 28000, 39780, 33600), 380, vec![0, 2800, 5600])
    }

    fn summary(g: &Grid) -> (Vec<(i64, i64)>, Vec<usize>) {
        let mut invalid = Vec::new();
        let mut per_group = vec![0usize; 2];
        for y in 0..g.row_count as i64 {
            for x in 0..g.row_site_count as i64 {
                let p = g.pixel(x, y).unwrap();
                if !p.is_valid {
                    invalid.push((x, y));
                }
                if let Some(id) = p.group {
                    per_group[id] += 1;
                }
            }
        }
        (invalid, per_group)
    }

    fn group(name: &str, rect: Box4) -> Group {
        Group { name: name.into(), rects: vec![rect], cells: vec![format!("{name}/cell")] }
    }

    /// `fence01`: `er0` ends and `er1` starts at 5890, mid-square 15. Upstream: square 15
    /// straddles (`groupInitPixels2`) — invalid; square 16 lies wholly INSIDE `er1` yet is invalid,
    /// because the left fraction is subtracted from `gridWithin`'s ceiled first column (util 0.5);
    /// square 14 gets util 1.5 from the negative C++ `%` and is left valid and in NO group.
    #[test]
    fn fence01_boundary_squares_match_the_reference() {
        let mut g = nangate_grid();
        group_init_pixels(&mut g, &[group("er0", (0, 0, 5890, 5600)), group("er1", (5890, 0, 11780, 5600))]);
        let (invalid, per_group) = summary(&g);
        assert_eq!(invalid, vec![(15, 0), (16, 0), (15, 1), (16, 1)]);
        assert_eq!(per_group, vec![28, 28]);
        assert!(g.pixel(14, 0).unwrap().is_valid && g.pixel(14, 0).unwrap().group.is_none());
    }

    /// `regions3`: `er1` covers row 0 up to 4000; `er2` starts at 7000 (clipped to the core).
    /// Upstream: 5 invalid squares — 10 in row 0, 18 and 19 in both — and 9 + 22 grouped.
    #[test]
    fn regions3_boundary_squares_match_the_reference() {
        let mut g = nangate_grid();
        group_init_pixels(&mut g, &[group("er1", (0, 0, 4000, 2800)), group("er2", (7000, 0, 11780, 5600))]);
        let (invalid, per_group) = summary(&g);
        assert_eq!(invalid, vec![(10, 0), (18, 0), (19, 0), (18, 1), (19, 1)]);
        assert_eq!(per_group, vec![9, 22]);
    }

    /// Upstream `groupInitPixels` skips a group with no cells — its squares stay ungrouped — but
    /// `groupInitPixels2` does not, so its straddling squares are still blocked.
    #[test]
    fn an_empty_group_still_blocks_its_boundary() {
        let mut g = nangate_grid();
        let mut empty = group("er1", (0, 0, 4000, 2800));
        empty.cells.clear();
        group_init_pixels(&mut g, &[empty]);
        let (invalid, per_group) = summary(&g);
        assert_eq!(invalid, vec![(10, 0)]);
        assert_eq!(per_group, vec![0, 0]);
    }

    // ── respectsFence ─────────────────────────────────────────────────────────────────────────

    /// `regions1`'s fences as the reference computed them: `(-73, 0, 104, 1)` and `(-73, 1, 18, 2)`.
    fn regions1_fences() -> Fences {
        let mut f = Fences::default();
        f.rects = vec![vec![(-73, 0, 104, 1)], vec![(-73, 1, 18, 2)]];
        f.cell_fence.insert("f1/_283_".into(), 1);
        f
    }

    /// Upstream: a fenced cell must lie INSIDE its fence — `[x, x+w) × [y, y+h)` in one rect.
    #[test]
    fn a_fenced_cell_must_lie_inside_its_fence() {
        let f = regions1_fences();
        assert!(f.respects("f1/_283_", 10, 1, 4, 1));
        assert!(f.respects("f1/_283_", 14, 1, 4, 1), "ending ON the fence's edge is inside");
        assert!(!f.respects("f1/_283_", 15, 1, 4, 1), "one site past");
        assert!(!f.respects("f1/_283_", 10, 0, 4, 1), "the other fence's row");
    }

    /// Upstream `respectsFence` for the default region tests `fence.contains`, not overlap: a
    /// default cell wholly inside a fence fails, one straddling a fence edge passes.
    #[test]
    fn a_default_cell_fails_only_wholly_inside_a_fence() {
        let mut f = Fences::default();
        f.rects = vec![vec![(0, 0, 10, 1)]];
        assert!(!f.respects("u", 2, 0, 4, 1), "inside");
        assert!(f.respects("u", 8, 0, 4, 1), "straddles the edge — passes, as upstream writes it");
        assert!(f.respects("u", 12, 0, 4, 1), "outside");
    }
}
