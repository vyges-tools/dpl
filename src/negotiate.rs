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

/// Which instances enter the negotiation model — `initFromDb`'s filter.
///
/// ⛔ Two exclusions, both load-bearing:
///
/// - **`dbPlacementStatus::NONE`** — never placed at all, so there is no position to negotiate
///   from;
/// - **`!isCoreAutoPlaceable()`** — pads and blocks, which are absent from the `Opendp` network,
///   so DRC and legality checks cannot be run on them. ⚠️ They are NOT ignored: `setFixedGridCells`
///   paints them separately, so they still block sites.
pub fn enters_model(placement_status: &str, core_auto_placeable: bool) -> bool {
    placement_status != "NONE" && core_auto_placeable
}

/// A cell's width in SITES — `initFromDb`'s sizing.
///
/// ⛔ **`round(width / site_width)`, floored at 1 — NOT `divCeil`.** `Grid::gridWidth` uses
/// `divCeil`, so the two disagree for a cell whose width is a fraction of a site below a whole
/// number: 1.4 sites rounds to 1 and ceils to 2.
///
/// ⚠️ Transcribed as-is. The negotiation model measures congestion in its own grid, and using the
/// wider `divCeil` here would make cells claim a site they do not occupy — inventing overuse that
/// the algorithm would then work to resolve.
pub fn cell_width_in_sites(master_width: i64, site_width: i64) -> i32 {
    if site_width <= 0 {
        return 1;
    }
    (1).max((master_width as f64 / site_width as f64).round() as i32)
}

/// The starting grid position — `gridX` for x, **`gridRoundY`** for y, then clamped.
///
/// ⚠️ **`x` FLOORS and `y` ROUNDS.** `diamondDPL`'s `legalGridPt` snaps y DOWN instead; this
/// legalizer takes the nearest row. A cell sitting just below a row boundary therefore starts one
/// row higher here than it would there.
///
/// 🔑 The clamp uses `grid_w - width` and `grid_h - height`, not `grid_w`/`grid_h`, so the whole
/// footprint stays on the grid rather than just the origin.
pub fn init_position(
    x_dbu: i64, y_dbu: i64, core_x: i64, core_y: i64, site_width: i64,
    row_y: &[i32], width: i32, height: i32, grid_w: i32, grid_h: i32,
) -> (i32, i32) {
    let gx = ((x_dbu - core_x) as f64 / site_width as f64).floor() as i32;
    let rel_y = (y_dbu - core_y) as i32;
    // `gridRoundY` — the NEAREST row boundary.
    let gy = row_y
        .iter()
        .enumerate()
        .min_by_key(|(_, ry)| (**ry - rel_y).abs())
        .map(|(i, _)| i as i32)
        .unwrap_or(0);
    (gx.clamp(0, (grid_w - width).max(0)), gy.clamp(0, (grid_h - height).max(0)))
}

/// `updateHistoryCosts` — make contested squares more expensive for the next iteration.
///
/// For every square under an active cell, **deduped so a square shared by several cells is bumped
/// once**: `hist_cost += HIST_INCREMENT * overuse`.
///
/// 🔑 **This is what makes negotiation terminate.** Present congestion alone oscillates — two
/// cells swap into each other's square forever. History remembers which squares keep being fought
/// over and prices them out, so the contest resolves instead of repeating.
///
/// ⚠️ **A mutation that does not COMPILE proves nothing.** Deleting the dedupe outright leaves
/// `seen` unused and the build fails — which prints no test results at all, and reads at a glance
/// exactly like a clean run. The honest mutation keeps `seen.insert(pid)` and drops only the skip.
///
/// ⚠️ **Only squares with `overuse > 0` are bumped.** A square that is merely occupied is not
/// contested, and raising its price would penalise a cell for sitting somewhere legal.
pub fn update_history_costs(pixels: &mut [NegPixel], footprints: &[Vec<usize>]) {
    let mut seen = std::collections::HashSet::new();
    for fp in footprints {
        for &pid in fp {
            if !seen.insert(pid) {
                continue; // already bumped this iteration
            }
            let ov = pixels[pid].overuse();
            if ov > 0 {
                pixels[pid].hist_cost += consts::HIST_INCREMENT * ov as f64;
            }
        }
    }
}

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

/// The algorithm's constants, whose "defaults match the NBLG paper" per upstream's own comment.
///
/// ⚠️ Transcribed rather than chosen. `INF_COST` in particular is **`INT_MAX / 2`**, not an
/// arbitrary large number — costs are compared against it directly, and a different magnitude
/// changes which candidates the branch-and-bound prunes.
pub mod consts {
    /// `kInfCost` — an unusable location. ⚠️ `INT_MAX / 2`, ~1.07e9, NOT 1e18.
    pub const INF_COST: f64 = (i32::MAX / 2) as f64;
    /// `kSiteSearchWindow` — base search width along the row, in sites.
    pub const SITE_SEARCH_WINDOW: i32 = 20;
    /// `kRowSearchWindow` — base search width across rows.
    pub const ROW_SEARCH_WINDOW: i32 = 5;
    /// `kDrcPenalty` — base DRC penalty, scaled by `(1 + iter)`.
    pub const DRC_PENALTY: f64 = 5.0;
    /// `kMaxIterNeg` / `kMaxIterNeg2` — the phase-1 and phase-2 iteration limits.
    pub const MAX_ITER_NEG: i32 = 400;
    pub const MAX_ITER_NEG2: i32 = 1000;
    /// `kIsolationPt` — from this iteration on, a cell that is already legal is skipped.
    /// ⚠️ It is **1**, so the isolation applies from the SECOND iteration: only the first pass
    /// rips up every active cell regardless of legality.
    pub const ISOLATION_PT: i32 = 1;
    /// `kMfDefault` / `kThDefault` — the displacement penalty past the threshold, in sites.
    pub const MAX_DISP_MULTIPLIER: f64 = 1.5;
    pub const MAX_DISP_THRESHOLD: i64 = 30;
    /// `kHfDefault` — how much history a unit of overuse adds.
    pub const HIST_INCREMENT: f64 = 1.0;
}

/// The cost a candidate location is judged by, as `kInfCost` — a location that cannot be used.
pub use consts::INF_COST;

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

/// `verticalWindowRows` — which rows a cell's search window covers.
///
/// Two sides walk outward from `seed_y`: `below` (+1) and `above` (-1). Each has its own **quota**
/// (`count_per_side`) of usable rows and its own **distance cap** (`max_scan`).
///
/// A side closes when it fills its quota, exhausts its cap, or meets a **hard wall** — the die edge
/// or a band of rows with no placement sites at all.
///
/// ⛔ **Unfilled quota is DONATED to the other side**, which is reopened and given a longer cap
/// (`min(2 * max_scan, max_displacement_y)`). A side closed at a wall is `walled` and **cannot
/// take** a donation; a side that merely filled its quota can be reopened by one. If both are
/// walled short, the unspent quota is never used.
///
/// ⚠️ **The RETURNED ORDER is `seed, below…, above…` — not sorted by distance.** `findBestLocation`
/// ties on a `ScanRank` whose middle term is the position in this list, so reordering it changes
/// which of two equally-costed rows wins.
///
/// 🔑 **A row counts only if it can host the cell SOMEWHERE in the horizontal span.** Probing a
/// single column would call a row beside a macro usable (or dead) on the evidence of one square.
#[allow(clippy::too_many_arguments)]
pub fn vertical_window_rows(
    seed_y: i32,
    height: i32,
    grid_h: i32,
    count_per_side: i32,
    max_scan: i32,
    extended_cap: i32,
    allow_extension: bool,
    row_has_sites: &dyn Fn(i32) -> bool,
    row_usable: &dyn Fn(i32) -> bool,
) -> Vec<i32> {
    let hard_wall = |r: i32| -> bool {
        if r < 0 || r + height > grid_h {
            return true;
        }
        (0..height).any(|dy| !row_has_sites(r + dy))
    };

    struct Side {
        dir: i32,
        step: i32,
        quota: i32,
        cap: i32,
        closed: bool,
        walled: bool,
        found: Vec<i32>,
    }
    let mk = |dir| Side { dir, step: 0, quota: count_per_side, cap: max_scan,
                          closed: false, walled: false, found: Vec::new() };
    let (mut below, mut above) = (mk(1), mk(-1));

    // One step of `self`, with `other` available to receive a donation.
    fn step_side(
        s: &mut Side, o: &mut Side, seed_y: i32, extended_cap: i32, allow_extension: bool,
        hard_wall: &dyn Fn(i32) -> bool, row_usable: &dyn Fn(i32) -> bool,
    ) {
        if s.closed {
            return;
        }
        if s.quota == 0 {
            s.closed = true; // quota filled — a later donation may reopen us
            return;
        }
        if s.step >= s.cap || hard_wall(seed_y + s.dir * (s.step + 1)) {
            s.closed = true;
            s.walled = true;
            if allow_extension && s.quota > 0 && !o.walled {
                o.quota += s.quota;
                o.cap = extended_cap;
                o.closed = false;
                s.quota = 0;
            }
            return;
        }
        s.step += 1;
        let r = seed_y + s.dir * s.step;
        if row_usable(r) {
            s.found.push(r);
            s.quota -= 1;
        }
    }

    while !below.closed || !above.closed {
        step_side(&mut below, &mut above, seed_y, extended_cap, allow_extension,
                  &hard_wall, row_usable);
        step_side(&mut above, &mut below, seed_y, extended_cap, allow_extension,
                  &hard_wall, row_usable);
    }

    // ⚠️ seed first, then every row found below, then every row found above.
    let mut rows = Vec::with_capacity(below.found.len() + above.found.len() + 1);
    if row_usable(seed_y) {
        rows.push(seed_y);
    }
    rows.extend(below.found);
    rows.extend(above.found);
    rows
}

/// `horizontalWindowBounds` — how far left and right the window reaches, as `(dx_lo, dx_hi)`.
///
/// The base window is `±site_window`. With extension enabled both sides walk outward from
/// `base_x`, and the result is clipped to the furthest **open** position each side reached —
/// **never below the base window** — then capped at `±max_displacement_x`.
///
/// ⛔ **The budget of `2 * site_window` is SHARED between the two sides**, not one each. Budget a
/// side cannot spend is simply left for the other, which is how a cell hemmed in on one side
/// still gets a wide search on the other.
///
/// ⛔ **A blocked position costs NOTHING.** Only an open one spends budget, so a side keeps
/// looking straight past a fixed instance instead of stopping at it. A side stops only at the die
/// edge or after `step_cap = min(4 * site_window, max_displacement_x)` steps.
///
/// ⚠️ **`reach` is the furthest OPEN position, not the furthest step.** A walk that dies in the
/// middle of a macro must not drag the window out over blocked sites it can never use.
#[allow(clippy::too_many_arguments)]
pub fn horizontal_window_bounds(
    base_x: i32,
    site_window: i32,
    max_displacement_x: i32,
    allow_extension: bool,
    off_die: &dyn Fn(i32) -> bool,
    open_at: &dyn Fn(i32) -> bool,
) -> (i32, i32) {
    let (mut dx_lo, mut dx_hi) = (-site_window, site_window);

    if allow_extension {
        let step_cap = (4 * site_window).min(max_displacement_x);
        let mut budget = 2 * site_window;
        let (mut left, mut right) = (0, 0);
        let (mut left_reach, mut right_reach) = (0, 0);
        let (mut left_open, mut right_open) = (true, true);

        while budget > 0 && (left_open || right_open) {
            if left_open {
                if left >= step_cap || off_die(base_x - (left + 1)) {
                    left_open = false;
                } else {
                    left += 1;
                    if open_at(base_x - left) {
                        left_reach = left;
                        budget -= 1;
                    }
                }
            }
            if right_open && budget > 0 {
                if right >= step_cap || off_die(base_x + (right + 1)) {
                    right_open = false;
                } else {
                    right += 1;
                    if open_at(base_x + right) {
                        right_reach = right;
                        budget -= 1;
                    }
                }
            }
        }
        dx_lo = -site_window.max(left_reach);
        dx_hi = site_window.max(right_reach);
    }

    (dx_lo.max(-max_displacement_x), dx_hi.min(max_displacement_x))
}

/// `effectiveSiteWindow` — the horizontal window, scaled by the cell and capped.
///
/// ⚠️ **`max(base, cell.width)`**: a wide cell gets at least its own width of search, because a
/// window narrower than the cell can only ever offer positions that overlap where it already is.
pub fn effective_site_window(base: i32, cell_width: i32, max_disp_x: i32,
                             allow_extension: bool) -> i32 {
    if !allow_extension || base == 0 {
        return base.min(max_disp_x);
    }
    base.max(cell_width).min(max_disp_x)
}

/// `effectiveRowCap` — the vertical distance cap, scaled by the cell's height and capped.
///
/// ⚠️ **`cell.height * base`**, not `max(base, height)`: a 4-row cell searches four times as far
/// vertically, because the rows it can legally start on are that much sparser.
pub fn effective_row_cap(base: i32, cell_height: i32, max_disp_y: i32,
                         allow_extension: bool) -> i32 {
    if !allow_extension {
        return base.min(max_disp_y);
    }
    (cell_height * base).min(max_disp_y)
}

/// One cell's search window: a horizontal reach and the rows to try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchWindow {
    pub dx_lo: i32,
    pub dx_hi: i32,
    pub rows: Vec<i32>,
}

/// `buildSearchWindow` — compose the two reaches around an anchor.
///
/// ⛔ **Horizontal FIRST, computed once at the anchor row; the vertical walk then probes only
/// within that span.** The order is not cosmetic: `verticalWindowRows` judges a row usable by
/// looking for a hostable column inside `[anchor_x + dx_lo, anchor_x + dx_hi]`, so a horizontal
/// reach computed afterwards — or a wider one — would change which rows qualify.
pub fn build_search_window(
    anchor_x: i32,
    anchor_y: i32,
    site_window: i32,
    row_window: i32,
    row_cap: i32,
    max_disp_x: i32,
    max_disp_y: i32,
    allow_extension: bool,
    off_die: &dyn Fn(i32) -> bool,
    open_at: &dyn Fn(i32) -> bool,
    cell_height: i32,
    grid_h: i32,
    row_has_sites: &dyn Fn(i32) -> bool,
    row_usable_in: &dyn Fn(i32, i32, i32) -> bool,
) -> SearchWindow {
    let (dx_lo, dx_hi) =
        horizontal_window_bounds(anchor_x, site_window, max_disp_x, allow_extension,
                                 off_die, open_at);
    let (x_lo, x_hi) = (anchor_x + dx_lo, anchor_x + dx_hi);
    let rows = vertical_window_rows(
        anchor_y, cell_height, grid_h, row_window, row_cap,
        (2 * row_cap).min(max_disp_y), allow_extension,
        row_has_sites,
        &|r| row_usable_in(r, x_lo, x_hi),
    );
    SearchWindow { dx_lo, dx_hi, rows }
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

    fn window(seed: i32, quota: i32, scan: i32, grid_h: i32,
              usable: &dyn Fn(i32) -> bool) -> Vec<i32> {
        vertical_window_rows(seed, 1, grid_h, quota, scan, 2 * scan, true,
                             &|r| r >= 0 && r < grid_h && usable(r), usable)
    }

    #[test]
    fn the_window_returns_the_seed_first_then_below_then_above() {
        // ⚠️ Not distance-sorted — the order is what ScanRank ties on.
        let rows = window(5, 2, 5, 20, &|_| true);
        assert_eq!(rows, vec![5, 6, 7, 4, 3]);
    }

    #[test]
    fn each_side_stops_at_its_own_quota() {
        let rows = window(10, 1, 9, 40, &|_| true);
        assert_eq!(rows, vec![10, 11, 9], "one row per side");
    }

    #[test]
    fn a_walled_side_donates_its_quota_to_the_other() {
        // ⛔ The seed sits on row 1; row 0 exists, row -1 is the die edge, so `above` walls
        // almost immediately and hands its quota down.
        let rows = window(1, 3, 20, 30, &|_| true);
        assert_eq!(rows[0], 1);
        assert!(rows.contains(&0), "the one row above is taken");
        let below: Vec<i32> = rows.iter().copied().filter(|r| *r > 1).collect();
        assert!(below.len() > 3, "the walled side's unfilled quota was spent below: {rows:?}");
    }

    #[test]
    fn extension_can_be_disabled() {
        let plain = vertical_window_rows(1, 1, 30, 3, 20, 40, false,
                                         &|r| (0..30).contains(&r), &|r| (0..30).contains(&r));
        let below: Vec<i32> = plain.iter().copied().filter(|r| *r > 1).collect();
        assert_eq!(below.len(), 3, "without extension each side keeps its own quota: {plain:?}");
    }

    #[test]
    fn a_row_band_with_no_sites_is_a_hard_wall() {
        // Rows 8..12 have no sites: the walk below stops there rather than jumping the gap.
        let usable = |r: i32| !(8..=12).contains(&r);
        let rows = vertical_window_rows(6, 1, 40, 5, 20, 40, false, &usable, &usable);
        assert!(rows.iter().all(|r| *r < 8 || *r > 12), "must not cross the gap: {rows:?}");
        assert!(rows.contains(&7), "it may reach the row just before the wall");
    }

    #[test]
    fn an_unusable_row_costs_a_step_but_not_quota() {
        // 🔑 Rows that exist but cannot host the cell are stepped over without spending quota.
        let has = |_: i32| true;
        let usable = |r: i32| r % 2 == 0; // odd rows exist but cannot host
        let rows = vertical_window_rows(4, 1, 40, 2, 10, 20, false, &has, &usable);
        assert!(rows.iter().all(|r| r % 2 == 0), "only usable rows are returned: {rows:?}");
        assert!(rows.len() >= 3, "quota was spent on usable rows, not on the skipped ones");
    }

    #[test]
    fn without_extension_the_window_is_just_the_base() {
        let (lo, hi) = horizontal_window_bounds(50, 4, 500, false, &|_| false, &|_| true);
        assert_eq!((lo, hi), (-4, 4));
    }

    #[test]
    fn the_extended_window_never_shrinks_below_the_base() {
        // Everything blocked: no reach is recorded, so the base window stands.
        let (lo, hi) = horizontal_window_bounds(50, 3, 500, true, &|_| false, &|_| false);
        assert_eq!((lo, hi), (-3, 3));
    }

    #[test]
    fn an_open_side_can_spend_the_shared_budget_before_a_blocked_side_uses_any() {
        // ⛔ **The budget is SHARED, and that has a consequence worth stating.** With a macro at
        // 51..56 on the right and open space on the left, the left side spends all
        // `2 * site_window` of it while the right is still stepping (free) through the macro — so
        // the right records NO reach and keeps only its base window.
        //
        // ⚠️ I asserted the opposite twice before tracing it. "Blocked steps are free" is about
        // BUDGET, not about winning the race for it.
        let open = |x: i32| !(51..=56).contains(&x);
        let (lo, hi) = horizontal_window_bounds(50, 3, 500, true, &|_| false, &open);
        assert_eq!(lo, -6, "the open side extended to its shared-budget reach");
        assert_eq!(hi, 3, "the blocked side never spent budget, so its base window stands");
    }

    #[test]
    fn a_blocked_stretch_is_stepped_over_when_the_other_side_cannot_compete() {
        // The same macro, but the left is walled at the die edge, so the right owns the budget
        // and walks through the macro to the open sites beyond it.
        let open = |x: i32| !(51..=56).contains(&x);
        let (_, hi) = horizontal_window_bounds(50, 3, 500, true, &|x| x < 50, &open);
        assert!(hi >= 7, "with the budget to itself the right passes the macro: hi={hi}");
    }

    #[test]
    fn the_step_cap_bounds_how_far_a_free_walk_can_go() {
        // ⚠️ Blocked steps are free of BUDGET but not of STEPS: `step_cap = 4 * site_window`
        // stops the walk, so a macro wider than the cap is never crossed. Learned by writing the
        // previous test wrong — a 20-wide macro at site_window 2 (cap 8) is impassable.
        let open = |x: i32| !(51..=70).contains(&x);
        let (_, hi) = horizontal_window_bounds(50, 2, 500, true, &|_| false, &open);
        assert_eq!(hi, 2, "cap 8 cannot cross a 20-wide macro, so the base window stands");
    }

    #[test]
    fn the_reach_is_the_furthest_open_position_not_the_furthest_step() {
        // Open only at +1; everything beyond is blocked to the step cap.
        let open = |x: i32| x <= 51;
        let (_, hi) = horizontal_window_bounds(50, 1, 500, true, &|_| false, &open);
        assert_eq!(hi, 1, "must not drag the window over sites it can never use");
    }

    #[test]
    fn a_side_at_the_die_edge_leaves_its_budget_to_the_other() {
        // 🔑 The budget is shared: hemmed in on the left, the right still searches widely.
        let off = |x: i32| x < 0;
        let (lo, hi) = horizontal_window_bounds(1, 4, 500, true, &off, &|_| true);
        assert!(lo >= -4, "the left is walled at the die edge: lo={lo}");
        assert!(hi > 4, "the right spent the shared budget: hi={hi}");
    }

    #[test]
    fn the_hard_cap_always_wins() {
        let (lo, hi) = horizontal_window_bounds(500, 50, 6, true, &|_| false, &|_| true);
        assert_eq!((lo, hi), (-6, 6), "max_displacement_x caps everything");
    }

    #[test]
    fn a_wide_cell_gets_at_least_its_own_width_of_search() {
        // ⚠️ A window narrower than the cell can only offer positions overlapping where it is.
        assert_eq!(effective_site_window(4, 30, 500, true), 30);
        assert_eq!(effective_site_window(40, 30, 500, true), 40, "the base wins when it is wider");
        assert_eq!(effective_site_window(4, 30, 10, true), 10, "the hard cap still wins");
        assert_eq!(effective_site_window(4, 30, 500, false), 4, "no extension: the plain base");
    }

    #[test]
    fn a_tall_cell_searches_proportionally_further_vertically() {
        // ⚠️ MULTIPLIED by height, not max()'d — the legal starting rows are that much sparser.
        assert_eq!(effective_row_cap(3, 4, 500, true), 12);
        assert_eq!(effective_row_cap(3, 1, 500, true), 3);
        assert_eq!(effective_row_cap(3, 4, 5, true), 5, "capped by max displacement");
        assert_eq!(effective_row_cap(3, 4, 500, false), 3, "no extension: the plain base");
    }

    #[test]
    fn the_window_probes_rows_only_within_its_horizontal_span() {
        // 🔑 The horizontal reach is computed first and bounds the vertical probe. Here only
        // columns 48..52 can host, so a narrow window finds rows and a shifted one does not.
        let hostable = |_r: i32, x_lo: i32, x_hi: i32| x_lo <= 52 && x_hi >= 48;
        let w = build_search_window(50, 5, 2, 2, 4, 500, 500, false,
                                    &|_| false, &|_| true, 1, 20,
                                    &|_| true, &hostable);
        assert_eq!((w.dx_lo, w.dx_hi), (-2, 2));
        assert!(!w.rows.is_empty(), "rows inside the span are found");

        let away = build_search_window(200, 5, 2, 2, 4, 500, 500, false,
                                       &|_| false, &|_| true, 1, 20,
                                       &|_| true, &hostable);
        assert!(away.rows.is_empty(), "no column in the span can host, so no row qualifies");
    }

    fn px(usage: i32) -> NegPixel {
        NegPixel { usage, hist_cost: 1.0, capacity: 1 }
    }

    #[test]
    fn history_rises_only_on_contested_squares() {
        // ⚠️ A merely occupied square is not contested; pricing it would penalise a legal cell.
        let mut p = vec![px(1), px(3), px(0)];
        update_history_costs(&mut p, &[vec![0, 1, 2]]);
        assert_eq!(p[0].hist_cost, 1.0, "usage 1 is not overuse");
        assert_eq!(p[1].hist_cost, 3.0, "overuse 2 adds 2.0");
        assert_eq!(p[2].hist_cost, 1.0);
    }

    #[test]
    fn a_shared_square_is_bumped_once_per_iteration() {
        // ⛔ Two cells cover square 0. Without the dedupe it would be bumped twice, so history
        // would grow with the number of overlapping cells rather than with the overuse.
        let mut p = vec![px(2)];
        update_history_costs(&mut p, &[vec![0], vec![0]]);
        assert_eq!(p[0].hist_cost, 2.0, "one bump of +1, not two");
    }

    #[test]
    fn history_accumulates_across_iterations() {
        // 🔑 This is what makes the negotiation terminate: a square fought over repeatedly keeps
        // getting dearer until the contest resolves rather than oscillating.
        let mut p = vec![px(2)];
        for _ in 0..4 {
            update_history_costs(&mut p, &[vec![0]]);
        }
        assert_eq!(p[0].hist_cost, 5.0);
    }

    #[test]
    fn the_constants_are_the_papers_defaults() {
        // Pinned because they are transcribed, not chosen — a changed value is a changed algorithm.
        assert_eq!(consts::SITE_SEARCH_WINDOW, 20);
        assert_eq!(consts::ROW_SEARCH_WINDOW, 5);
        assert_eq!(consts::MAX_ITER_NEG, 400);
        assert_eq!(consts::MAX_ITER_NEG2, 1000);
        assert_eq!(consts::ISOLATION_PT, 1);
        assert_eq!(consts::MAX_DISP_MULTIPLIER, 1.5);
        assert_eq!(consts::MAX_DISP_THRESHOLD, 30);
        assert_eq!(consts::HIST_INCREMENT, 1.0);
        assert_eq!(consts::DRC_PENALTY, 5.0);
        // ⚠️ INT_MAX/2, not an arbitrary large number: costs are compared against it.
        assert_eq!(consts::INF_COST, 1_073_741_823.0);
    }

    #[test]
    fn unplaced_and_non_core_instances_stay_out_of_the_model() {
        assert!(enters_model("PLACED", true));
        assert!(enters_model("FIRM", true));
        assert!(!enters_model("NONE", true), "never placed: nothing to negotiate from");
        assert!(!enters_model("PLACED", false), "a pad or block is painted, not negotiated");
    }

    #[test]
    fn cell_width_rounds_rather_than_ceils() {
        // ⛔ The difference from Grid::gridWidth, which uses divCeil.
        assert_eq!(cell_width_in_sites(140, 100), 1, "1.4 sites rounds DOWN to 1");
        assert_eq!(cell_width_in_sites(160, 100), 2, "1.6 rounds up");
        assert_eq!(cell_width_in_sites(200, 100), 2, "exactly 2");
        assert_eq!(cell_width_in_sites(10, 100), 1, "never below one site");
    }

    #[test]
    fn the_start_position_floors_x_and_rounds_y() {
        // Rows every 10 units; the cell sits at y=17, nearer row 2 (y=20) than row 1 (y=10).
        let rows = [0, 10, 20, 30];
        let (gx, gy) = init_position(255, 17, 0, 0, 100, &rows, 1, 1, 100, 4);
        assert_eq!(gx, 2, "x floors: 2.55 sites -> 2");
        assert_eq!(gy, 2, "y takes the NEAREST row, not the one below");
    }

    #[test]
    fn the_clamp_keeps_the_whole_footprint_on_the_grid() {
        // 🔑 grid_w - width, not grid_w: a 3-site cell may not start on the last column.
        let rows = [0, 10];
        let (gx, _) = init_position(10_000, 0, 0, 0, 100, &rows, 3, 1, 10, 2);
        assert_eq!(gx, 7, "10 - 3, so the footprint ends exactly at the edge");
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
