// SPDX-License-Identifier: Apache-2.0
//! `NegotiationLegalizer` — the DEFAULT legalizer at the pin.
//!
//! Reference: OpenROAD `src/dpl/src/NegotiationLegalizer{,Pass}.cpp` at
//! `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, read from the tree whose `git rev-parse HEAD`
//! matches that pin.
//!
//! ⛔ **Why this and not `diamondDPL`.** `use_diamond_legalizer_` defaults to **false** and
//! `isUseNegotiationLegalizer()` returns its negation, so negotiation is what a plain
//! `detailed_placement` runs. Only **4 of 67** upstream cases pass `-use_diamond_legalizer`.
//!
//! 🔑 **PathFinder-style negotiated congestion.** Cells are allowed to overlap; contested sites
//! accumulate a *history cost*; each iteration rips up and re-places until nothing overlaps. That
//! is a different shape from `diamondDPL`, which seats each cell once at the nearest legal site.
//!
//! ⚠️ **Only ILLEGAL cells are negotiated** — a cell that is already legal is left where it is.
//!
//! ⬜ **Status: the model and the ordering are built; the iteration core is not.** `findBestLocation`
//! (the cost function), `ripUp`/`place`, the history update and phase 2 remain. The full call
//! sequence is recorded in `vyges-tools-internal/docs/openroad/dpl/negotiation-legalizer.md` so
//! the next pass builds from a read reference rather than re-reading 2,315 lines.

/// One cell in the negotiation model — upstream's `NegCell`.
///
/// ⚠️ Coordinates are in GRID units (site columns, row indices), not DBU. The negotiation grid is
/// its own, separate from `dpl`'s `Grid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegCell {
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// Width in SITES and height in ROWS.
    pub width: i32,
    pub height: i32,
    pub fixed: bool,
    pub legal: bool,
}

/// The negotiation grid — `NegotiationLegalizer::buildGrid`.
///
/// Built by resetting every square, then blockading the fixed cells:
///
/// ```text
/// capacity  = is_valid ? 1 : 0      usage = 0      hist_cost = 1.0
/// row_has_sites[y] = any square in row y has capacity > 0
/// for each FIXED cell footprint:  capacity = 0,  usage = 1
/// ```
///
/// ⛔ **A fixed cell sets `capacity = 0`, not merely `usage = 1`.** It is a hard blockage, so
/// `negotiation_cost` returns `INF_COST` there — a movable cell can never be considered for that
/// square, however cheap the displacement. Recording only the usage would make a fixed cell look
/// like a single overlap that negotiation could bargain away.
///
/// ⚠️ **`hist_cost` starts at 1.0, not 0.** The cost is `hist_cost * (usage + 1) / capacity`, so a
/// zero start would make every square free on the first iteration and the whole congestion term
/// inert until something bumped it.
///
/// ℹ️ **Footprint only — padding is left to `PlacementDRC`**, which knows both masters and can
/// apply class-pair rules that a plain capacity cannot express.
pub struct NegGrid {
    pub width: usize,
    pub height: usize,
    pixels: Vec<NegPixel>,
    row_has_sites: Vec<bool>,
}

impl NegGrid {
    /// `valid(x, y)` is the `dpl` Grid's `is_valid` for that square.
    pub fn build(width: usize, height: usize, valid: &dyn Fn(usize, usize) -> bool) -> NegGrid {
        let mut pixels = vec![NegPixel::default(); width * height];
        for y in 0..height {
            for x in 0..width {
                let p = &mut pixels[y * width + x];
                p.capacity = if valid(x, y) { 1 } else { 0 };
                p.usage = 0;
                p.hist_cost = 1.0;
            }
        }
        let mut g = NegGrid { width, height, pixels, row_has_sites: vec![false; height] };
        g.recompute_rows();
        g
    }

    fn recompute_rows(&mut self) {
        for y in 0..self.height {
            self.row_has_sites[y] = (0..self.width).any(|x| self.at(x, y).capacity > 0);
        }
    }

    /// Blockade a fixed cell's footprint: `capacity = 0`, `usage = 1`.
    pub fn blockade(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for dy in 0..h {
            for dx in 0..w {
                let (gx, gy) = (x + dx, y + dy);
                if gx < 0 || gy < 0 || gx as usize >= self.width || gy as usize >= self.height {
                    continue;
                }
                let p = &mut self.pixels[gy as usize * self.width + gx as usize];
                p.capacity = 0;
                p.usage = 1;
            }
        }
        self.recompute_rows();
    }

    pub fn at(&self, x: usize, y: usize) -> &NegPixel {
        &self.pixels[y * self.width + x]
    }

    /// ⚠️ A row with no usable square at all — `isValidRow` rejects any cell spanning it.
    pub fn row_has_sites(&self, y: i32) -> bool {
        y >= 0 && (y as usize) < self.height && self.row_has_sites[y as usize]
    }

    /// `addUsage(cell, delta)` over a footprint.
    pub fn add_usage(&mut self, x: i32, y: i32, w: i32, h: i32, delta: i32) {
        for dy in 0..h {
            for dx in 0..w {
                let (gx, gy) = (x + dx, y + dy);
                if gx < 0 || gy < 0 || gx as usize >= self.width || gy as usize >= self.height {
                    continue;
                }
                self.pixels[gy as usize * self.width + gx as usize].usage += delta;
            }
        }
    }

    /// Total overuse across the grid — zero is what the negotiation converges to.
    pub fn total_overuse(&self) -> i32 {
        self.pixels.iter().map(|p| p.overuse()).sum()
    }
}

/// One square of the negotiation grid.
///
/// 🔑 `usage` is what makes this "negotiated": a site may be claimed by more than one cell, and the
/// excess is the pressure the algorithm works to remove. `hist_cost` remembers contested sites
/// across iterations so a site that keeps being fought over becomes expensive.
#[derive(Debug, Clone, Default)]
pub struct NegPixel {
    pub usage: i32,
    pub hist_cost: f64,
    /// 1 for a usable square, 0 for a blockage or a fixed cell's footprint.
    pub capacity: i32,
}

impl NegPixel {
    /// Sites claimed beyond the one the square can serve.
    ///
    /// ⚠️ **One cell per square is not overuse** — `max(0, usage - 1)`, not `usage > 0`. A fixed
    /// cell's square carries `usage = 1` (and `capacity = 0`), and a legally placed movable cell
    /// carries 1 too; treating any usage as overuse would report a legal design as fully
    /// congested and the negotiation would never terminate.
    pub fn overuse(&self) -> i32 {
        (self.usage - 1).max(0)
    }
}

/// The key `sortByNegotiationOrder` sorts on.
///
/// ⛔ **`(overuse DESC, height DESC, width DESC, idx ASC)`** — and the trailing index is the
/// determinism tie-break, the same role `sequence` plays in `diamondSearch`. Upstream builds this
/// as a decorate-sort and its comment records that the decorated form *"yields identical results
/// to scoring (a, b) directly"*, so the decoration is a speed-up rather than a behaviour change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub overuse: i32,
    pub height: i32,
    pub width: i32,
    pub idx: usize,
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Most-overused first, then tallest, then widest, then lowest index.
        other
            .overuse
            .cmp(&self.overuse)
            .then(other.height.cmp(&self.height))
            .then(other.width.cmp(&self.width))
            .then(self.idx.cmp(&other.idx))
    }
}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Order the active cells for one negotiation sweep.
pub fn sort_by_negotiation_order(keys: &mut [SortKey]) {
    keys.sort();
}

/// The cost a candidate location is judged by, as `kInfCost` — a location that cannot be used.
pub const INF_COST: f64 = 1e18;

/// `targetCostFromDisp` — the displacement term.
///
/// `disp + multiplier * max(0, disp - threshold)`: linear in displacement, then steeper once it
/// passes the threshold.
///
/// 🔑 **Monotone in `disp`, and `findBestLocation` depends on that.** The wavefront search prunes
/// as soon as a wavefront's displacement cost plus the congestion floor exceeds the incumbent —
/// which is only sound because this never decreases as displacement grows.
pub fn target_cost_from_disp(disp: i64, multiplier: f64, threshold: i64) -> f64 {
    disp as f64 + multiplier * (disp - threshold).max(0) as f64
}

/// `targetCost` — displacement measured from the cell's INIT position.
///
/// ⛔ **From `init_x`/`init_y`, NOT from where the cell currently sits.** The init position is
/// where global placement put it, and it does not move as the cell is ripped up and re-placed
/// across iterations. Measuring from the current position instead would let a cell drift
/// arbitrarily far over many iterations, one cheap step at a time.
pub fn target_cost(x: i32, y: i32, init_x: i32, init_y: i32, multiplier: f64, threshold: i64) -> f64 {
    let disp = (x - init_x).abs() as i64 + (y - init_y).abs() as i64;
    target_cost_from_disp(disp, multiplier, threshold)
}

/// `negotiationCost` — displacement plus the PathFinder congestion term.
///
/// For each square of the footprint: `cost += hist_cost * (usage + 1) / capacity`. That is the
/// classic `h * p` — history times present congestion (upstream's comment cites "Eq. 10").
///
/// ⚠️ **`usage + 1`** counts the cell being considered, so a square already holding one cell reads
/// as 2/1 rather than 1/1. Without the `+1` an occupied square looks exactly as cheap as an empty
/// one and the algorithm never separates overlaps.
///
/// ⛔ `capacity == 0` is a blockage and returns `INF_COST`; so does a square off the grid.
///
/// `abort_bound` is a branch-and-bound cut: the caller's incumbent cost. Returning early once the
/// running total passes it is safe because every remaining term is non-negative.
pub fn negotiation_cost(
    footprint: impl Iterator<Item = Option<(i32, i32, f64)>>,
    target: f64,
    abort_bound: f64,
) -> f64 {
    let mut cost = target;
    if cost > abort_bound {
        return cost;
    }
    for square in footprint {
        match square {
            // Off the grid, or a blockage: unusable.
            None => return cost + INF_COST,
            Some((usage, capacity, hist)) => {
                if capacity == 0 {
                    return cost + INF_COST;
                }
                cost += hist * ((usage + 1) as f64 / capacity as f64);
                if cost > abort_bound {
                    return cost;
                }
            }
        }
    }
    cost
}

/// Why a candidate position is not legal. `Legal` is the only passing value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legality {
    Legal,
    OffDie,
    RowHasNoSites,
    RowRejectsSite,
    Blockage,
    Overused,
    /// ⬜ Declared, never returned by this engine — see [`is_cell_legal`].
    DrcUnavailable,
}

/// `isValidRow` — may this cell sit in this row, at this column?
///
/// 1. the row span must be inside the grid;
/// 2. **every** row of the span must have sites — a multi-row cell may not straddle a gap;
/// 3. the row must offer the cell's site (`getSiteOrientation`).
///
/// ⬜ Upstream also checks **master symmetry** (`checkMasterSym`) and, for multi-row cells,
/// **power-stack compatibility** (`checkRowPowerCompatible`). Neither is implemented here and both
/// are reported by the caller rather than skipped in silence.
pub fn is_valid_row(
    row: i32,
    height: i32,
    grid_h: i32,
    row_has_sites: &dyn Fn(i32) -> bool,
    row_offers_site: &dyn Fn(i32) -> bool,
) -> Legality {
    if row < 0 || row + height > grid_h {
        return Legality::OffDie;
    }
    for dy in 0..height {
        if !row_has_sites(row + dy) {
            return Legality::RowHasNoSites;
        }
    }
    if !row_offers_site(row) {
        return Legality::RowRejectsSite;
    }
    Legality::Legal
}

/// `isCellLegal` — is the cell legally placed where it is?
///
/// ⛔ **Upstream's version returns FALSE when the DRC engine is unavailable**, logging
/// *"DRC objects not available!"*. Transcribed literally without `PlacementDRC`, every cell would
/// be illegal and the negotiation would never converge.
///
/// ⚠️ **So this omits the DRC clause, and that is a DIVERGENCE, not a simplification.** A cell that
/// upstream rejects for edge spacing, blocked layers, padding or a one-site gap is legal here — so
/// this engine will consider fewer cells illegal, negotiate fewer of them, and settle somewhere
/// upstream would not. It must stay declared until `PlacementDRC` exists.
pub fn is_cell_legal(
    in_die: bool,
    row_ok: Legality,
    footprint: impl Iterator<Item = (i32, i32)>,
) -> Legality {
    if !in_die {
        return Legality::OffDie;
    }
    if row_ok != Legality::Legal {
        return row_ok;
    }
    // ⬜ upstream: `drc_engine_->checkDRC(node, x, y, orient)` here.
    for (usage, capacity) in footprint {
        if capacity == 0 {
            return Legality::Blockage;
        }
        if (usage - 1).max(0) > 0 {
            return Legality::Overused;
        }
    }
    Legality::Legal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(overuse: i32, height: i32, width: i32, idx: usize) -> SortKey {
        SortKey { overuse, height, width, idx }
    }

    #[test]
    fn a_fixed_site_is_used_but_not_overused() {
        // ⛔ The distinction the whole cost function rests on.
        assert_eq!(NegPixel { usage: 0, ..Default::default() }.overuse(), 0);
        assert_eq!(NegPixel { usage: 1, ..Default::default() }.overuse(), 0);
        assert_eq!(NegPixel { usage: 2, ..Default::default() }.overuse(), 1);
        assert_eq!(NegPixel { usage: 5, ..Default::default() }.overuse(), 4);
    }

    #[test]
    fn the_most_overused_cell_is_negotiated_first() {
        let mut v = [k(0, 1, 1, 0), k(7, 1, 1, 1), k(3, 1, 1, 2)];
        sort_by_negotiation_order(&mut v);
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [1, 2, 0]);
    }

    #[test]
    fn equal_overuse_breaks_on_height_then_width() {
        let mut v = [k(2, 1, 9, 0), k(2, 3, 1, 1), k(2, 1, 2, 2)];
        sort_by_negotiation_order(&mut v);
        // Tallest first; then among the height-1 pair the wider one.
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [1, 0, 2]);
    }

    #[test]
    fn the_index_is_the_determinism_tie_break() {
        // ⛔ Without it these are indistinguishable and the sweep order is whatever the sort does.
        let mut v = [k(1, 1, 1, 9), k(1, 1, 1, 2), k(1, 1, 1, 5)];
        sort_by_negotiation_order(&mut v);
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [2, 5, 9]);
    }

    #[test]
    fn displacement_cost_is_monotone_and_steepens_past_the_threshold() {
        // 🔑 The wavefront prune is only sound because this never decreases.
        let c = |d| target_cost_from_disp(d, 2.0, 10);
        let vals: Vec<f64> = (0..20).map(c).collect();
        assert!(vals.windows(2).all(|w| w[0] <= w[1]), "not monotone: {vals:?}");
        assert_eq!(c(10), 10.0, "at the threshold it is still purely linear");
        assert_eq!(c(11), 11.0 + 2.0, "past it, the multiplier applies to the excess only");
    }

    #[test]
    fn displacement_is_measured_from_the_init_position() {
        // ⛔ Not from where the cell currently sits — that would let it drift across iterations.
        assert_eq!(target_cost(5, 0, 5, 0, 1.0, 100), 0.0, "at its init position, no cost");
        assert_eq!(target_cost(7, 0, 5, 0, 1.0, 100), 2.0);
        assert_eq!(target_cost(3, 0, 5, 0, 1.0, 100), 2.0, "symmetric");
    }

    #[test]
    fn an_occupied_square_costs_more_than_an_empty_one() {
        // ⚠️ The `usage + 1` is what separates overlaps; without it these are equal.
        let empty = negotiation_cost([Some((0, 1, 1.0))].into_iter(), 0.0, INF_COST);
        let taken = negotiation_cost([Some((1, 1, 1.0))].into_iter(), 0.0, INF_COST);
        assert!(taken > empty, "empty={empty} taken={taken}");
        assert_eq!(empty, 1.0);
        assert_eq!(taken, 2.0);
    }

    #[test]
    fn history_makes_a_contested_square_expensive() {
        let fresh = negotiation_cost([Some((1, 1, 1.0))].into_iter(), 0.0, INF_COST);
        let fought = negotiation_cost([Some((1, 1, 8.0))].into_iter(), 0.0, INF_COST);
        assert!(fought > fresh, "history must raise the price of a contested site");
    }

    #[test]
    fn a_blockage_or_an_off_grid_square_is_unusable() {
        assert!(negotiation_cost([Some((0, 0, 1.0))].into_iter(), 0.0, INF_COST) >= INF_COST);
        assert!(negotiation_cost([None].into_iter(), 0.0, INF_COST) >= INF_COST);
    }

    #[test]
    fn the_abort_bound_stops_the_scan_early() {
        // Two squares, but the bound is passed on the first: the second is never added.
        let c = negotiation_cost([Some((9, 1, 1.0)), Some((9, 1, 1.0))].into_iter(), 0.0, 5.0);
        assert_eq!(c, 10.0, "returned as soon as it passed the bound, not 20");
    }

    #[test]
    fn a_multi_row_cell_may_not_straddle_a_row_without_sites() {
        // ⛔ EVERY row of the span is checked, not just the bottom one.
        let has = |r: i32| r != 3;               // row 3 is a gap
        let offers = |_: i32| true;
        assert_eq!(is_valid_row(2, 1, 10, &has, &offers), Legality::Legal);
        assert_eq!(is_valid_row(2, 2, 10, &has, &offers), Legality::RowHasNoSites,
                   "a 2-row cell starting at 2 covers the gap at 3");
    }

    #[test]
    fn a_row_span_running_past_the_grid_is_off_die() {
        let (has, offers) = (|_: i32| true, |_: i32| true);
        assert_eq!(is_valid_row(9, 2, 10, &has, &offers), Legality::OffDie);
        assert_eq!(is_valid_row(-1, 1, 10, &has, &offers), Legality::OffDie);
    }

    #[test]
    fn an_overused_or_blocked_square_makes_a_cell_illegal() {
        let ok = |v: Vec<(i32, i32)>| is_cell_legal(true, Legality::Legal, v.into_iter());
        assert_eq!(ok(vec![(1, 1), (1, 1)]), Legality::Legal, "one cell per square is legal");
        assert_eq!(ok(vec![(1, 1), (2, 1)]), Legality::Overused);
        assert_eq!(ok(vec![(1, 0)]), Legality::Blockage);
    }

    #[test]
    fn the_row_verdict_is_reported_rather_than_collapsed_to_a_bool() {
        // 🔑 A caller that only sees false cannot tell a blockage from a missing row, and the two
        // want different fixes.
        assert_eq!(is_cell_legal(false, Legality::Legal, [].into_iter()), Legality::OffDie);
        assert_eq!(is_cell_legal(true, Legality::RowRejectsSite, [].into_iter()),
                   Legality::RowRejectsSite);
    }

    #[test]
    fn a_fixed_cell_is_a_blockage_not_an_overlap() {
        // ⛔ The distinction that decides whether negotiation can bargain a site away.
        let mut g = NegGrid::build(4, 2, &|_, _| true);
        g.blockade(1, 0, 2, 1);
        assert_eq!(g.at(1, 0).capacity, 0, "a fixed footprint has NO capacity");
        assert_eq!(g.at(1, 0).usage, 1);
        assert_eq!(g.at(0, 0).capacity, 1, "its neighbour is untouched");
        // And the cost model refuses it outright rather than pricing it.
        let c = negotiation_cost([Some((g.at(1, 0).usage, g.at(1, 0).capacity, 1.0))].into_iter(),
                                 0.0, INF_COST);
        assert!(c >= INF_COST, "a blockade must be unusable, not merely expensive");
    }

    #[test]
    fn history_starts_at_one_so_the_congestion_term_is_live_immediately() {
        // ⚠️ At 0 the whole term would be inert until something bumped it.
        let g = NegGrid::build(2, 1, &|_, _| true);
        assert_eq!(g.at(0, 0).hist_cost, 1.0);
        assert_eq!(negotiation_cost([Some((0, 1, g.at(0, 0).hist_cost))].into_iter(), 0.0,
                                    INF_COST), 1.0);
    }

    #[test]
    fn a_row_with_no_usable_square_has_no_sites() {
        // Row 1 is entirely invalid; a cell may not span it.
        let g = NegGrid::build(3, 2, &|_, y| y == 0);
        assert!(g.row_has_sites(0));
        assert!(!g.row_has_sites(1));
        assert!(!g.row_has_sites(-1), "off the grid is not a row with sites");
    }

    #[test]
    fn blockading_a_whole_row_removes_it() {
        let mut g = NegGrid::build(2, 1, &|_, _| true);
        assert!(g.row_has_sites(0));
        g.blockade(0, 0, 2, 1);
        assert!(!g.row_has_sites(0), "every square blocked means the row is gone");
    }

    #[test]
    fn usage_tracks_rip_up_and_place() {
        let mut g = NegGrid::build(4, 1, &|_, _| true);
        g.add_usage(0, 0, 2, 1, 1);
        assert_eq!(g.total_overuse(), 0, "one cell over two squares is not overuse");
        g.add_usage(1, 0, 2, 1, 1);
        assert_eq!(g.total_overuse(), 1, "the shared square is overused by one");
        g.add_usage(1, 0, 2, 1, -1); // rip up
        assert_eq!(g.total_overuse(), 0);
    }

    #[test]
    fn the_order_does_not_depend_on_the_input_order() {
        let mk = || vec![k(0, 2, 2, 3), k(4, 1, 1, 1), k(4, 2, 1, 7), k(0, 2, 2, 0)];
        let (mut a, mut b) = (mk(), mk());
        b.reverse();
        sort_by_negotiation_order(&mut a);
        sort_by_negotiation_order(&mut b);
        assert_eq!(a, b, "the sweep order must not depend on how the active set was built");
    }
}
