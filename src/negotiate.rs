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
//! ⬜ **Status: the legalizer runs end to end.** What it does NOT implement is listed in
//! [`NOT_DONE`] and reported on every run — a legalizer that quietly omits a check reports fewer
//! violations than it earned. The call sequence each function transcribes is stated in that
//! function's own doc comment, at the site, rather than in a document that would drift from it.

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

    /// `Grid::paintPixel(cell, x, y)` — stamp this cell over its footprint.
    ///
    /// ⛔ **Unconditional, so a later painter OVERWRITES an earlier one.** Upstream writes
    /// `pixel->cell = cell` with no test of what was there, which is what makes the slot lossy
    /// under overlap. Refusing to overwrite would make our occupancy MORE complete than the
    /// reference's and change what every `PlacementDRC` check sees.
    ///
    /// ℹ️ Padding reservations (`paintCellPadding`) are not stamped: this engine implements no
    /// padding values, so the reserved band is empty. See `NOT_DONE`.
    pub fn paint_cell(&mut self, idx: usize, x: i32, y: i32, w: i32, h: i32) {
        for dy in 0..h {
            for dx in 0..w {
                let (gx, gy) = (x + dx, y + dy);
                if gx < 0 || gy < 0 || gx as usize >= self.width || gy as usize >= self.height {
                    continue;
                }
                self.pixels[gy as usize * self.width + gx as usize].cell = Some(idx);
            }
        }
    }

    /// `Grid::erasePixel(cell)` — clear this cell's stamp over its footprint.
    ///
    /// ⛔ **`if (pixel->cell == cell)` — a cell clears ONLY its own stamp.** Clearing the square
    /// unconditionally would erase a co-located cell that upstream leaves in place; not testing
    /// it at all would leave a stale stamp behind a cell that has moved. Both are wrong in a
    /// direction the gate cannot see, because the difference only shows on shared squares.
    pub fn erase_cell(&mut self, idx: usize, x: i32, y: i32, w: i32, h: i32) {
        for dy in 0..h {
            for dx in 0..w {
                let (gx, gy) = (x + dx, y + dy);
                if gx < 0 || gy < 0 || gx as usize >= self.width || gy as usize >= self.height {
                    continue;
                }
                let p = &mut self.pixels[gy as usize * self.width + gx as usize];
                if p.cell == Some(idx) {
                    p.cell = None;
                }
            }
        }
    }

    /// `pixel->cell` at a square, `None` off the grid or where nothing is stamped.
    pub fn occupant(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return None;
        }
        self.at(x as usize, y as usize).cell
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
    /// `Pixel::cell` — WHICH cell is stamped here, or `None`.
    ///
    /// ⛔ **This is a different question from `usage > 0` and the two disagree under overlap.**
    /// `usage` counts every cell claiming the square; this holds ONE index, the last painter's.
    /// `PlacementDRC` reads this one — `checkPadding` tests `pixel->cell` and `checkOneSiteGap`
    /// tests it for abutment — so a check driven off `usage` is answering a question upstream
    /// never asks.
    ///
    /// ⚠️ **A ripped-up cell can therefore blank a square another cell still claims**, because
    /// `erasePixel` clears the slot when it holds THIS cell and `usage` is decremented
    /// separately. Upstream says so in `legalize`: *"overlapping cells share a single
    /// pixel->cell slot; when one is ripped up, the other's presence is lost"*, and calls
    /// `syncAllCellsToDplGrid` to repair it before counting violations. That lossiness is
    /// behaviour, not a bug to improve on.
    pub cell: Option<usize>,
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
/// ⛔ **`(overuse DESC, height ASC, width ASC, idx ASC)`** — overuse descending, then the
/// **SMALLEST** cells first. The trailing index is the determinism tie-break, the same role
/// `sequence` plays in `diamondSearch`. Upstream builds this as a decorate-sort and its comment
/// records that the decorated form *"yields identical results to scoring (a, b) directly"*, so
/// the decoration is a speed-up rather than a behaviour change.
///
/// ⚠️ **Only the overuse key is descending, and getting the other two backwards is quiet.** This
/// read `height DESC, width DESC` until 2026-09-02. Nothing could catch it while every cell in
/// the sweep was there for OVERLAP — the first key separated them and the rest never ran. It
/// became visible the moment DRC violations started putting non-overused cells into the sweep,
/// where overuse ties at 0 for everyone and the height key decides.
///
/// 🔑 Measured on `multi_height_one_site_gap_disallow`: a single-height and a double-height cell
/// with a one-site gap between them. Smallest-first moves the SINGLE-height cell one site, which
/// is the reference's answer; tallest-first moves the double-height one instead and both cells
/// end up somewhere upstream never puts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub overuse: i32,
    pub height: i32,
    pub width: i32,
    pub idx: usize,
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Most-overused first, then SHORTEST, then NARROWEST, then lowest index.
        other
            .overuse
            .cmp(&self.overuse)
            .then(self.height.cmp(&other.height))
            .then(self.width.cmp(&other.width))
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
    /// The cell fails one of `PlacementDRC`'s four checks.
    DrcViolation,
    /// ⛔ No DRC checker was supplied. Upstream fails the cell in this case rather than passing
    /// it — a missing checker is "cannot vouch for this", not "nothing wrong".
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
/// ⛔ **Upstream returns FALSE when the DRC engine is unavailable**, logging *"DRC objects not
/// available!"*. It treats a missing checker as "cannot vouch for this cell", not as "no news is
/// good news" — so `drc_ok` here is `Option<bool>` rather than `bool`, and `None` fails the cell
/// exactly as upstream does.
///
/// ✅ **The DRC clause is WIRED as of 2026-09-02** (`crate::drc::check_drc`), so the divergence
/// declared when this was first written is closed. It read: *a cell upstream rejects for edge
/// spacing, blocked layers, padding or a one-site gap is legal here, so this engine considers
/// fewer cells illegal and settles somewhere upstream would not.*
///
/// ⚠️ **Order is upstream's fast path**: `inDie → isValidRow → fence → DRC → footprint`, cheapest
/// first, bailing on the first failure.
pub fn is_cell_legal(
    in_die: bool,
    row_ok: Legality,
    drc_ok: Option<bool>,
    footprint: impl Iterator<Item = (i32, i32)>,
) -> Legality {
    if !in_die {
        return Legality::OffDie;
    }
    if row_ok != Legality::Legal {
        return row_ok;
    }
    // ⛔ `None` = no checker. Upstream fails the cell rather than passing it.
    match drc_ok {
        None => return Legality::DrcUnavailable,
        Some(false) => return Legality::DrcViolation,
        Some(true) => {}
    }
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

/// How a negotiation run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Zero violations, in the given phase and iteration.
    Converged { phase: u8, iter: i32 },
    /// Violations stalled for 3 identical iterations; diamond recovery ran, then the phase broke.
    StalledIntoRecovery { phase: u8, iter: i32, violations: i32 },
    /// Both phases ran out of iterations. ⚠️ Upstream does NOT error here — the caller reports it
    /// through `numViolations()`.
    Exhausted { violations: i32 },
}

/// `runNegotiation`'s two-phase driver.
///
/// ```text
/// phase 1: kMaxIterNeg  = 400  iterations, iter counted from 0
/// phase 2: kMaxIterNeg2 = 1000 iterations, iter CONTINUES from max_iter_neg
/// ```
///
/// ⛔ **Phase 2 continues the iteration counter (`actual_iter = iter + max_iter_neg`), it does not
/// restart it.** Two things ride on that number and both would be wrong if it reset: the isolation
/// point (`iter >= kIsolationPt` — already-legal cells are skipped) and the DRC penalty, which
/// scales as `drc_penalty * (1 + iter)` and is meant to keep climbing.
///
/// ⚠️ **A stall is three CONSECUTIVE identical violation counts**, and any different count resets
/// the counter. Not "no improvement" — a count that rises then falls is progress, not a stall.
///
/// 🔑 **The stall escape is `diamondRecovery`** — the diamond search legalizer, run over the cells
/// still illegal, then the phase BREAKS. So `diamondDPL`'s search is not an alternative to
/// negotiation; it is a component of it.
///
/// ⛔ **A phase-1 stall BREAKS INTO PHASE 2; it does not end the run.** Upstream's `break` leaves
/// the phase-1 loop and falls through to phase 2, which runs its full `kMaxIterNeg2` iterations
/// on the cells `diamondRecovery` could not seat. Only a phase-2 stall ends it. Returning after
/// the first stall — which this function did until 2026-09-02 — throws away the 1000 iterations
/// that are meant to clean up after recovery.
///
/// ⚠️ **`kIsolationPt` is 1, so the isolation applies from phase-1 iteration 1** — not from
/// phase 2. Upstream's own comments say otherwise ("Phase 1 – all active cells rip-up every
/// iteration (isolation point = 0)" and "Phase 2 – isolation point active"), and the code at
/// `NegotiationLegalizerPass.cpp:314` — `if (iter >= kIsolationPt && isCellLegal(idx))` — is what
/// runs. Transcribed from the code.
pub fn run_negotiation(
    max_iter_neg: i32,
    max_iter_neg2: i32,
    mut iterate: impl FnMut(i32) -> i32,
    mut diamond_recovery: impl FnMut(),
) -> Outcome {
    let mut stalled_phase_1 = None;
    let mut final_violations = -1;
    for (phase, (start, count)) in [(0, max_iter_neg), (max_iter_neg, max_iter_neg2)]
        .into_iter()
        .enumerate()
    {
        let mut prev = -1;
        let mut stall = 0;
        let mut last = -1;
        for i in 0..count {
            let actual = start + i;
            let violations = iterate(actual);
            last = violations;
            if violations == 0 {
                return Outcome::Converged { phase: phase as u8 + 1, iter: i };
            }
            if violations == prev {
                stall += 1;
                if stall == 3 {
                    diamond_recovery();
                    // ⛔ BREAK, not return. Phase 1 falls through into phase 2; only a phase-2
                    // stall ends the run.
                    if phase == 1 {
                        return Outcome::StalledIntoRecovery {
                            phase: 2, iter: i, violations,
                        };
                    }
                    stalled_phase_1 = Some((i, violations));
                    break;
                }
            } else {
                stall = 0;
            }
            prev = violations;
        }
        final_violations = last;
    }
    match stalled_phase_1 {
        // Phase 1 stalled, recovery ran, and phase 2 then used up its iterations without
        // converging. Both facts are reported — the stall is why, the exhaustion is what.
        Some((iter, violations)) => Outcome::StalledIntoRecovery { phase: 1, iter, violations },
        None => Outcome::Exhausted { violations: final_violations },
    }
}

/// A candidate location and the rank that breaks cost ties.
///
/// ⛔ **`ScanRank = (window, row_pos, dx)`, compared lexicographically**, and the FIRST element is
/// the window index: **0 = the init window, 1 = the current-position window**. So on an exact cost
/// tie a candidate found around the cell's original position beats one found around where it
/// currently sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScanRank {
    pub window: u8,
    pub row_pos: usize,
    pub dx: i32,
}

/// One candidate the search offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub x: i32,
    pub y: i32,
    pub rank: ScanRank,
}

/// `findBestLocation`'s enumeration — candidates in the order upstream visits them.
///
/// **Pass 1 — wavefronts of increasing displacement around the INIT position.**
/// `d` runs 0..=`max_dy + max(-dx_lo, dx_hi)`; each row spends `|ty - init_y|` of the budget and
/// the remainder `rem` is the horizontal reach. ⚠️ At `rem == 0` only the LEFT branch fires (the
/// right needs `rem > 0`), so the centre column is offered **once**, not twice.
///
/// ⛔ **`prune(d)` breaks the whole loop, it does not skip.** `targetCostFromDisp` is monotone in
/// `d`, so once a wavefront's floor exceeds the incumbent no later wavefront can win — that is the
/// entire reason the search is cheap in an uncontended region.
///
/// **Pass 2 — a FULL scan around the CURRENT position, only when the cell has been displaced.**
/// ⚠️ Not wavefronts: upstream's comment is explicit that `targetCost` is anchored at init, so
/// ordering around the current position "gives no usable bound here". Every candidate is offered
/// and the per-candidate prune does the work.
pub fn enumerate_candidates(
    init_x: i32, init_y: i32, cur_x: i32, cur_y: i32,
    init_rows: &[i32], init_dx_lo: i32, init_dx_hi: i32,
    curr_rows: &[i32], curr_dx_lo: i32, curr_dx_hi: i32,
    prune: &dyn Fn(i64) -> bool,
    out: &mut Vec<Candidate>,
) {
    let max_dy = init_rows.iter().map(|ty| (ty - init_y).abs()).max().unwrap_or(0);
    let max_d = max_dy + (-init_dx_lo).max(init_dx_hi);

    'waves: for d in 0..=max_d {
        if prune(d as i64) {
            break 'waves; // ⛔ break, not continue — the floor only rises
        }
        for (row_pos, &ty) in init_rows.iter().enumerate() {
            let rem = d - (ty - init_y).abs();
            if rem < 0 {
                continue;
            }
            if -rem >= init_dx_lo {
                out.push(Candidate { x: init_x - rem, y: ty,
                                     rank: ScanRank { window: 0, row_pos, dx: -rem } });
            }
            if rem > 0 && rem <= init_dx_hi {
                out.push(Candidate { x: init_x + rem, y: ty,
                                     rank: ScanRank { window: 0, row_pos, dx: rem } });
            }
        }
    }

    // Only when displaced — an undisplaced cell's two windows are the same one.
    if cur_x == init_x && cur_y == init_y {
        return;
    }
    for (row_pos, &ty) in curr_rows.iter().enumerate() {
        for dx in curr_dx_lo..=curr_dx_hi {
            out.push(Candidate { x: cur_x + dx, y: ty,
                                 rank: ScanRank { window: 1, row_pos, dx } });
        }
    }
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
    fn equal_overuse_breaks_on_height_then_width_smallest_first() {
        // ⛔ **Only `overuse` is descending.** Height and width are ASCENDING — the smallest
        // cells are negotiated first. This test asserted the opposite until 2026-09-02.
        let mut v = [k(2, 1, 9, 0), k(2, 3, 1, 1), k(2, 1, 2, 2)];
        sort_by_negotiation_order(&mut v);
        // Shortest first; among the two height-1 keys, the narrower one.
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [2, 0, 1]);
    }

    #[test]
    fn overuse_still_outranks_a_smaller_cell() {
        // ⚠️ The keys are not independent: a tiny cell with no overuse must NOT jump ahead of a
        // big one that is overlapping. Reversing height without keeping overuse first would do
        // exactly that.
        let mut v = [k(0, 1, 1, 0), k(5, 9, 9, 1)];
        sort_by_negotiation_order(&mut v);
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [1, 0]);
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
        let ok = |v: Vec<(i32, i32)>| is_cell_legal(true, Legality::Legal, Some(true), v.into_iter());
        assert_eq!(ok(vec![(1, 1), (1, 1)]), Legality::Legal, "one cell per square is legal");
        assert_eq!(ok(vec![(1, 1), (2, 1)]), Legality::Overused);
        assert_eq!(ok(vec![(1, 0)]), Legality::Blockage);
    }

    #[test]
    fn a_missing_drc_checker_fails_the_cell_rather_than_passing_it() {
        // ⛔ Upstream logs "DRC objects not available!" and returns false. A missing checker is
        // "cannot vouch for this cell", not "nothing wrong with it" — passing it would let the
        // negotiation declare a design legal that was never checked.
        assert_eq!(is_cell_legal(true, Legality::Legal, None, [].into_iter()),
                   Legality::DrcUnavailable);
        assert_eq!(is_cell_legal(true, Legality::Legal, Some(false), [].into_iter()),
                   Legality::DrcViolation);
        assert_eq!(is_cell_legal(true, Legality::Legal, Some(true), [].into_iter()),
                   Legality::Legal);
    }

    #[test]
    fn drc_is_consulted_before_the_footprint_scan() {
        // ⚠️ Upstream's fast path: a DRC failure short-circuits, so an overused footprint behind
        // a DRC violation reports the DRC one.
        assert_eq!(is_cell_legal(true, Legality::Legal, Some(false), [(9, 1)].into_iter()),
                   Legality::DrcViolation, "not Overused");
    }

    #[test]
    fn the_row_verdict_is_reported_rather_than_collapsed_to_a_bool() {
        // 🔑 A caller that only sees false cannot tell a blockage from a missing row, and the two
        // want different fixes.
        assert_eq!(is_cell_legal(false, Legality::Legal, Some(true), [].into_iter()), Legality::OffDie);
        assert_eq!(is_cell_legal(true, Legality::RowRejectsSite, Some(true), [].into_iter()),
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
    fn a_later_painter_overwrites_the_occupancy_slot() {
        // ⛔ `Grid::paintPixel` writes `pixel->cell = cell` with no test of what was there, so a
        // square claimed by two cells remembers only the LAST one. Upstream depends on the
        // lossiness: `legalize`'s own comment is "overlapping cells share a single pixel->cell
        // slot; when one is ripped up, the other's presence is lost".
        let mut g = NegGrid::build(4, 1, &|_, _| true);
        g.paint_cell(0, 1, 0, 2, 1);
        assert_eq!(g.occupant(1, 0), Some(0));
        g.paint_cell(1, 1, 0, 2, 1);
        assert_eq!(g.occupant(1, 0), Some(1), "the second painter wins the slot");
    }

    #[test]
    fn a_cell_erases_only_its_own_stamp() {
        // ⛔ `Grid::erasePixel` clears the slot under `if (pixel->cell == cell)`. Cell 0 painted
        // first and cell 1 overwrote it, so cell 0's erase must leave the square alone — and
        // cell 1's erase blanks it even though cell 0 is still there. Both halves are upstream.
        let mut g = NegGrid::build(4, 1, &|_, _| true);
        g.paint_cell(0, 1, 0, 1, 1);
        g.paint_cell(1, 1, 0, 1, 1);
        g.erase_cell(0, 1, 0, 1, 1);
        assert_eq!(g.occupant(1, 0), Some(1), "cell 0 does not own the slot, so it clears nothing");
        g.erase_cell(1, 1, 0, 1, 1);
        assert_eq!(g.occupant(1, 0), None, "and the owner's erase blanks a square cell 0 claims");
    }

    #[test]
    fn occupancy_and_usage_answer_different_questions() {
        // 🔑 The reason `checkOneSiteGap` and `checkPadding` may not be driven off `usage`.
        // After the owner is ripped up the square still carries a usage from the co-located
        // cell, while `pixel->cell` reads empty — which is what upstream's DRC sees.
        let mut g = NegGrid::build(4, 1, &|_, _| true);
        for idx in [0, 1] {
            g.add_usage(1, 0, 1, 1, 1);
            g.paint_cell(idx, 1, 0, 1, 1);
        }
        g.add_usage(1, 0, 1, 1, -1);
        g.erase_cell(1, 1, 0, 1, 1);
        assert_eq!(g.at(1, 0).usage, 1, "cell 0 still claims the square");
        assert_eq!(g.occupant(1, 0), None, "but the occupancy slot reads empty");
    }

    #[test]
    fn the_re_sync_repairs_the_holes_and_repaints_fixed_last() {
        // ⛔ `syncAllCellsToDplGrid` is three passes, not two: erase all, repaint all, then
        // repaint the FIXED cells. Upstream's reason for the third is in the source — a movable
        // cell painted over an endcap overwrites the slot and its next `erasePixel` clears the
        // endcap, "making checkOneSiteGap blind to it".
        let mut g = NegGrid::build(4, 1, &|_, _| true);
        let cells = vec![sweep_cell("a", 1, 1), sweep_cell("b", 1, 1)];
        let fixed: Vec<FixedPaint> = vec![(9, 1, 0, 1, 1)];
        g.paint_cell(0, 1, 0, 1, 1);
        g.erase_cell(0, 1, 0, 1, 1); // the hole a rip-up punches
        assert_eq!(g.occupant(1, 0), None);
        sync_all_cells_to_grid(&cells, &fixed, &mut g);
        assert_eq!(g.occupant(1, 0), Some(9), "the fixed cell is repainted last and wins");
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
        NegPixel { usage, hist_cost: 1.0, capacity: 1, cell: None }
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
    fn converging_in_phase_one_stops_immediately() {
        let mut seen = Vec::new();
        let out = run_negotiation(400, 1000, |i| { seen.push(i); if i < 3 { 5 - i } else { 0 } },
                                  || panic!("recovery must not run on a converging design"));
        assert_eq!(out, Outcome::Converged { phase: 1, iter: 3 });
        assert_eq!(seen, vec![0, 1, 2, 3], "no iteration past convergence");
    }

    #[test]
    fn three_identical_counts_are_a_stall_and_call_recovery() {
        let mut recovered = 0;
        let out = run_negotiation(400, 1000, |_| 7, || recovered += 1);
        // ⛔ A count that never moves stalls TWICE: once in phase 1, which breaks into phase 2,
        // and once in phase 2, which ends the run. Recovery therefore runs once per phase.
        assert_eq!(out, Outcome::StalledIntoRecovery { phase: 2, iter: 3, violations: 7 });
        assert_eq!(recovered, 2, "recovery ran in both phases, not once");
    }

    #[test]
    fn a_phase_one_stall_falls_through_into_phase_two() {
        // 🔑 The property the `break` exists for: after recovery, phase 2 gets its full run.
        // ⚠️ This fails if phase 1 RETURNS on a stall — `iters` would stop at 3.
        let mut iters = Vec::new();
        let out = run_negotiation(400, 5, |i| {
            iters.push(i);
            // Constant through the phase-1 stall, then a sequence that never repeats so phase 2
            // exhausts rather than stalling too.
            if iters.len() <= 4 { 7 } else { 100 - iters.len() as i32 }
        }, || {});
        assert_eq!(&iters[..4], &[0, 1, 2, 3], "phase 1 stalled at its 4th iteration");
        assert_eq!(&iters[4..], &[400, 401, 402, 403, 404],
                   "phase 2 ran all 5 of its iterations, counter continuing from 400");
        assert_eq!(out, Outcome::StalledIntoRecovery { phase: 1, iter: 3, violations: 7 },
                   "the phase-1 stall is still reported — it is why phase 2 had work left");
    }

    #[test]
    fn a_changed_count_resets_the_stall() {
        // ⚠️ Not "no improvement": a count that rises then falls is progress.
        let seq = [4, 4, 5, 5, 5, 5];
        let mut n = 0;
        let out = run_negotiation(400, 1000, |_| { let v = seq[n.min(seq.len() - 1)]; n += 1; v },
                                  || {});
        // 4,4 -> stall 1; 5 resets; 5,5 -> stall 2; 5 -> stall 3 at index 5. Phase 1 then breaks
        // into phase 2, where the count is still a constant 5 and it stalls again.
        assert_eq!(out, Outcome::StalledIntoRecovery { phase: 2, iter: 3, violations: 5 });
    }

    #[test]
    fn phase_two_continues_the_iteration_counter() {
        // ⛔ The isolation point and the DRC penalty both read this number.
        let mut iters = Vec::new();
        run_negotiation(3, 2, |i| { iters.push(i); 1 + (iters.len() as i32 % 2) }, || {});
        assert_eq!(&iters[..3], &[0, 1, 2], "phase 1");
        assert_eq!(&iters[3..], &[3, 4], "phase 2 continues from 3, it does not restart at 0");
    }

    #[test]
    fn exhausting_both_phases_is_not_an_error_here() {
        // ⚠️ Upstream reports non-convergence through the caller's numViolations().
        let mut n = 0;
        let out = run_negotiation(2, 2, |_| { n += 1; n }, || {});
        assert!(matches!(out, Outcome::Exhausted { .. }));
        assert_eq!(n, 4, "both phases ran to their limits");
    }

    fn enumerate(init_rows: &[i32], lo: i32, hi: i32) -> Vec<Candidate> {
        let mut v = Vec::new();
        enumerate_candidates(10, 5, 10, 5, init_rows, lo, hi, &[], 0, 0, &|_| false, &mut v);
        v
    }

    #[test]
    fn the_centre_is_offered_once_not_twice() {
        // ⚠️ At rem == 0 only the left branch fires, so (init_x, init_y) appears a single time.
        let v = enumerate(&[5], -2, 2);
        let centre = v.iter().filter(|c| (c.x, c.y) == (10, 5)).count();
        assert_eq!(centre, 1, "got {centre} copies of the centre");
    }

    #[test]
    fn candidates_arrive_in_wavefronts_of_increasing_displacement() {
        let v = enumerate(&[5], -3, 3);
        let disp: Vec<i32> = v.iter().map(|c| (c.x - 10).abs() + (c.y - 5).abs()).collect();
        assert!(disp.windows(2).all(|w| w[0] <= w[1]), "not monotone: {disp:?}");
        assert_eq!(disp[0], 0, "the cell's own position comes first");
    }

    #[test]
    fn the_prune_breaks_the_search_rather_than_skipping_a_wavefront() {
        // ⛔ **A MONOTONE prune cannot tell `break` from `continue`** — both drop everything past
        // the threshold. Written that way first, and a `break`→`continue` mutation survived.
        //
        // A NON-monotone prune separates them: reject only d == 2. `break` stops there and never
        // offers d >= 3; `continue` skips 2 and resumes at 3.
        //
        // ⚠️ Upstream is only *entitled* to `break` because `targetCostFromDisp` is monotone — but
        // the test for which spelling was written needs the case that distinguishes them.
        let mut v = Vec::new();
        enumerate_candidates(10, 5, 10, 5, &[5], -5, 5, &[], 0, 0, &|d| d == 2, &mut v);
        let reach = v.iter().map(|c| (c.x - 10).abs()).max().unwrap_or(0);
        assert_eq!(reach, 1, "must BREAK at d == 2, so d >= 3 is never offered; got reach {reach}");
        assert!(!v.is_empty(), "wavefronts 0 and 1 were still offered");
    }

    #[test]
    fn the_second_window_runs_only_when_the_cell_is_displaced() {
        let mut same = Vec::new();
        enumerate_candidates(10, 5, 10, 5, &[5], 0, 0, &[9], -1, 1, &|_| false, &mut same);
        assert!(same.iter().all(|c| c.rank.window == 0), "undisplaced: no second window");

        let mut moved = Vec::new();
        enumerate_candidates(10, 5, 30, 9, &[5], 0, 0, &[9], -1, 1, &|_| false, &mut moved);
        assert!(moved.iter().any(|c| c.rank.window == 1), "displaced: the second window runs");
    }

    #[test]
    fn the_init_window_outranks_the_current_window_on_a_tie() {
        // ⛔ ScanRank compares the window index first.
        let a = ScanRank { window: 0, row_pos: 99, dx: 99 };
        let b = ScanRank { window: 1, row_pos: 0, dx: 0 };
        assert!(a < b, "an init-window candidate wins however deep in its own scan it was");
    }

    fn sweep_cell(name: &str, x: i32, w: i32) -> SweepCell {
        SweepCell { name: name.into(), x, y: 0, init_x: x, init_y: 0,
                    width: w, height: 1, fixed: false }
    }

    /// A one-row grid of `width` sites, all valid.
    fn sweep_ctx(grid: &mut NegGrid) -> SweepCtx<'_> {
        SweepCtx {
            grid,
            window: &|_c, x, y| (-4, 4, vec![y; 1].into_iter().map(|_| y).collect()),
            placeable: &|c, x, _y| x >= 0 && x + c.width <= 8,
            drc_violations: &|_, _, _, _| 0,
            fixed_paint: &[],
            max_disp_multiplier: consts::MAX_DISP_MULTIPLIER,
            max_disp_threshold: consts::MAX_DISP_THRESHOLD,
            drc_penalty: consts::DRC_PENALTY,
        }
    }

    #[test]
    fn a_drc_violation_alone_still_counts_as_a_violation() {
        // ⛔ The bug this pins: the count was `grid.total_overuse()`, so a design whose ONLY
        // fault was a DRC violation reported ZERO and the driver declared convergence on the
        // first iteration. Measured on `multi_height_one_site_gap_disallow`.
        let mut grid = NegGrid::build(6, 1, &|_, _| true);
        let mut cells = vec![sweep_cell("a", 0, 1)];
        grid.add_usage(0, 0, 1, 1, 1);
        let mut ctx = SweepCtx {
            grid: &mut grid,
            window: &|_c, x, y| (-x, 0, vec![y]),   // pinned in place: no candidate but its own
            placeable: &|_, x, _| (0..6).contains(&x),
            // Always dirty, wherever it sits.
            drc_violations: &|_, _, _, _| 1,
            fixed_paint: &[],
            max_disp_multiplier: consts::MAX_DISP_MULTIPLIER,
            max_disp_threshold: consts::MAX_DISP_THRESHOLD,
            drc_penalty: consts::DRC_PENALTY,
        };
        let v = negotiation_iter(&mut cells, &mut vec![0], 0, &mut ctx);
        assert_eq!(grid.total_overuse(), 0, "no square is overused");
        assert!(v > 0, "and yet the iteration must report a violation, not converge");
    }

    #[test]
    fn a_bystander_made_illegal_is_pulled_into_the_active_set() {
        // ⛔ A cell outside the active set can be made illegal by a neighbour's move. Without
        // this it is never revisited and the run converges with the violation still there.
        let mut grid = NegGrid::build(6, 1, &|_, _| true);
        let mut cells = vec![sweep_cell("mover", 0, 1), sweep_cell("bystander", 4, 1)];
        for c in &cells {
            grid.add_usage(c.x, c.y, c.width, c.height, 1);
        }
        let mut ctx = SweepCtx {
            grid: &mut grid,
            window: &|_c, x, y| (-x, 0, vec![y]),
            placeable: &|_, x, _| (0..6).contains(&x),
            // Only the bystander is dirty — the active cell is fine.
            drc_violations: &|c, _, _, _| i32::from(c.name == "bystander"),
            fixed_paint: &[],
            max_disp_multiplier: consts::MAX_DISP_MULTIPLIER,
            max_disp_threshold: consts::MAX_DISP_THRESHOLD,
            drc_penalty: consts::DRC_PENALTY,
        };
        let mut active = vec![0];
        let v = negotiation_iter(&mut cells, &mut active, 0, &mut ctx);
        assert_eq!(active, vec![0, 1], "the bystander joined the active set");
        assert!(v > 0, "and was counted");
    }

    #[test]
    fn history_is_not_bumped_on_a_clean_iteration() {
        // ⚠️ `updateHistoryCosts` is guarded by `totalViolations > 0`. Bumping it on a converged
        // iteration prices up squares nobody is contesting.
        let mut grid = NegGrid::build(4, 1, &|_, _| true);
        let mut cells = vec![sweep_cell("a", 1, 1)];
        grid.add_usage(1, 0, 1, 1, 1);
        let before = grid.at(1, 0).hist_cost;
        let mut ctx = SweepCtx {
            grid: &mut grid,
            window: &|_c, x, y| (-x, 3 - x, vec![y]),
            placeable: &|_, x, _| (0..4).contains(&x),
            drc_violations: &|_, _, _, _| 0,
            fixed_paint: &[],
            max_disp_multiplier: consts::MAX_DISP_MULTIPLIER,
            max_disp_threshold: consts::MAX_DISP_THRESHOLD,
            drc_penalty: consts::DRC_PENALTY,
        };
        let v = negotiation_iter(&mut cells, &mut vec![0], 0, &mut ctx);
        assert_eq!(v, 0, "nothing is wrong");
        assert_eq!(grid.at(1, 0).hist_cost, before, "so history did not move");
    }

    #[test]
    fn a_cell_on_a_zero_capacity_square_is_illegal() {
        // ⛔ `blockage01`: a hard placement blockage reaches the legality test ONLY through
        // capacity. A scan that tests overuse alone calls this cell legal and never moves it.
        let mut grid = NegGrid::build(4, 1, &|x, _| x >= 2); // sites 0-1 blocked
        let mut cells = vec![sweep_cell("a", 0, 1)];
        grid.add_usage(0, 0, 1, 1, 1);
        assert!(!footprint_is_legal(&cells[0], 0, 0, &grid), "site 0 has capacity 0");
        assert!(footprint_is_legal(&cells[0], 2, 0, &grid), "site 2 does not");

        // ⚠️ And the sweep moves it out, rather than leaving it where it started.
        let mut ctx = SweepCtx {
            grid: &mut grid,
            window: &|_c, x, y| (-x, 3 - x, vec![y]),
            placeable: &|c, x, _| x >= 0 && x + c.width <= 4,
            drc_violations: &|_, _, _, _| 0,
            fixed_paint: &[],
            max_disp_multiplier: consts::MAX_DISP_MULTIPLIER,
            max_disp_threshold: consts::MAX_DISP_THRESHOLD,
            drc_penalty: consts::DRC_PENALTY,
        };
        negotiation_iter(&mut cells, &mut vec![0], 0, &mut ctx);
        assert!(cells[0].x >= 2, "moved off the blockage, to {}", cells[0].x);
    }

    #[test]
    fn a_sweep_separates_two_overlapping_cells() {
        // 🔑 The end-to-end property the whole engine exists for: two cells on the same site,
        // one sweep, no overlap left.
        let mut grid = NegGrid::build(8, 1, &|_, _| true);
        let mut cells = vec![sweep_cell("a", 2, 2), sweep_cell("b", 2, 2)];
        for c in &cells {
            grid.add_usage(c.x, c.y, c.width, c.height, 1);
        }
        assert!(grid.total_overuse() > 0, "the fixture must start overlapping");

        let mut ctx = sweep_ctx(&mut grid);
        let overuse = negotiation_iter(&mut cells, &mut vec![0, 1], 0, &mut ctx);
        assert_eq!(overuse, 0, "one sweep resolved it: {:?}",
                   cells.iter().map(|c| (c.name.clone(), c.x)).collect::<Vec<_>>());
        assert_ne!(cells[0].x, cells[1].x, "the two cells no longer share a site");
    }

    #[test]
    fn a_cell_can_stay_where_it_is() {
        // ⛔ Ripping up before searching must not stop a cell choosing its own square back.
        let mut grid = NegGrid::build(8, 1, &|_, _| true);
        let mut cells = vec![sweep_cell("solo", 3, 2)];
        grid.add_usage(3, 0, 2, 1, 1);
        let mut ctx = sweep_ctx(&mut grid);
        negotiation_iter(&mut cells, &mut vec![0], 0, &mut ctx);
        assert_eq!(cells[0].x, 3, "an uncontested cell stays at its init position");
        assert_eq!(grid.total_overuse(), 0);
    }

    #[test]
    fn a_fixed_cell_is_never_moved() {
        let mut grid = NegGrid::build(8, 1, &|_, _| true);
        let mut cells = vec![SweepCell { fixed: true, ..sweep_cell("fix", 2, 2) }];
        grid.blockade(2, 0, 2, 1);
        let mut ctx = sweep_ctx(&mut grid);
        negotiation_iter(&mut cells, &mut vec![0], 0, &mut ctx);
        assert_eq!(cells[0].x, 2);
    }

    #[test]
    fn from_the_isolation_point_a_legal_cell_is_left_alone() {
        // ⚠️ iter 0 sweeps everything; iter >= 1 skips cells that are already legal.
        let mut grid = NegGrid::build(8, 1, &|_, _| true);
        let mut cells = vec![sweep_cell("a", 3, 2)];
        grid.add_usage(3, 0, 2, 1, 1);
        let mut ctx = sweep_ctx(&mut grid);
        negotiation_iter(&mut cells, &mut vec![0], consts::ISOLATION_PT, &mut ctx);
        assert_eq!(cells[0].x, 3, "untouched, and its usage was never ripped up");
        assert_eq!(grid.at(3, 0).usage, 1, "usage is intact — the skip happened before rip-up");
    }

    #[test]
    fn the_clamp_is_in_grid_units_not_dbu() {
        // ⛔ **The units contract of `init_position`, pinned.** `width`/`height` are SITES and
        // ROWS; the clamp is `grid_w - width`, `grid_h - height`. Passing DBU makes both bounds
        // negative, `.max(0)` collapses them to `clamp(0, 0)`, and every cell reports a start of
        // (0, 0) — after which `targetCost` measures displacement from the core's bottom-left
        // corner and drags the design there.
        //
        // ⚠️ Measured on `fragmented_row04` (FreePDK45, site 380, grid 32x4): the DEF places
        // `_277_` at (8360, 5600) and we reported `init_grid [0, 0]`.
        let row_y = [0, 2800, 5600, 8400, 11200];
        let grid_units = init_position(8360, 5600, 3800, 2800, 380, &row_y, 4, 1, 32, 4);
        assert_eq!(grid_units, (12, 1), "site 12 of row 1, which is where the DEF puts it");

        // The same call with the master's DBU footprint — the bug, kept as the witness.
        let dbu = init_position(8360, 5600, 3800, 2800, 380, &row_y, 1520, 2800, 32, 4);
        assert_eq!(dbu, (0, 0), "DBU collapses both clamps: this is what the bug looked like");
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

// ── the assembled sweep ──────────────────────────────────────────────────────────────────────

/// A cell as the sweep sees it. Grid units throughout.
#[derive(Debug, Clone)]
pub struct SweepCell {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub init_x: i32,
    pub init_y: i32,
    pub width: i32,
    pub height: i32,
    pub fixed: bool,
}

/// What one `negotiationIter` sweep needs from its surroundings.
pub struct SweepCtx<'a> {
    pub grid: &'a mut NegGrid,
    /// `(dx_lo, dx_hi, rows)` for a cell anchored at `(x, y)`.
    pub window: &'a dyn Fn(&SweepCell, i32, i32) -> (i32, i32, Vec<i32>),
    /// Can this cell's footprint legally sit here, ignoring congestion?
    pub placeable: &'a dyn Fn(&SweepCell, i32, i32) -> bool,
    /// How many of `PlacementDRC`'s four checks a position fails.
    ///
    /// ⚠️ **Takes the grid**, because three of the four rules read neighbouring squares. It is
    /// passed in rather than captured so the sweep can still hold the grid mutably; every call
    /// site reborrows it immutably for the duration of the check.
    ///
    /// 🔑 Called AFTER the cell has been ripped up, so the grid the rules see does not contain
    /// the cell being placed — which is what upstream's `ripUp` (an `erasePixel` on the DPL
    /// grid) arranges before `findBestLocation` runs.
    pub drc_violations: &'a dyn Fn(&SweepCell, i32, i32, &NegGrid) -> i32,
    /// Fixed-cell footprints, repainted into the occupancy map after every re-sync.
    pub fixed_paint: &'a [FixedPaint],
    pub max_disp_multiplier: f64,
    pub max_disp_threshold: i64,
    pub drc_penalty: f64,
}

/// `syncAllCellsToDplGrid` — clear every movable cell from the occupancy map, repaint them all,
/// then repaint the FIXED cells last.
///
/// ⛔ **The three passes are not interchangeable and the order is load-bearing.** Erasing and
/// repainting in one pass would let a cell erased later blank a square an earlier cell had just
/// claimed. Repainting fixed cells last is upstream's own fix, with its reason in the source: a
/// movable cell painted over an endcap overwrites the slot, and *that cell's* next `erasePixel`
/// then clears the endcap from the grid, "making `checkOneSiteGap` blind to it".
///
/// 🔑 **Why it is needed at all**: during a sweep, a ripped-up cell blanks squares a co-located
/// cell still occupies (see [`NegPixel::cell`]). Upstream calls this before counting violations
/// so the scan sees the true placement rather than the holes the sweep punched.
pub fn sync_all_cells_to_grid(cells: &[SweepCell], fixed: &[FixedPaint], grid: &mut NegGrid) {
    for (i, c) in cells.iter().enumerate() {
        grid.erase_cell(i, c.x, c.y, c.width, c.height);
    }
    for (i, c) in cells.iter().enumerate() {
        grid.paint_cell(i, c.x, c.y, c.width, c.height);
    }
    for &(idx, x, y, w, h) in fixed {
        grid.paint_cell(idx, x, y, w, h);
    }
}

/// A fixed cell in the occupancy map: `(index, x, y, width, height)` in grid units.
///
/// ⛔ **Fixed cells are carried separately because they are not in the sweep.** They never move,
/// so nothing in `negotiation_iter` would place them — but `PlacementDRC` reads them through
/// `pixel->cell` like any other occupant, and a checker blind to a macro or an endcap approves
/// positions upstream refuses. Upstream keeps them in the same `cells_` vector behind a `fixed`
/// flag; this engine's sweep vector holds movable cells only, so their footprints ride alongside.
pub type FixedPaint = (usize, i32, i32, i32, i32);

/// `negotiationIter` — one rip-up / re-place sweep over the active cells.
///
/// ⛔ **The order is upstream's and each step matters:**
///
/// 1. `sortByNegotiationOrder` — most-overused first;
/// 2. skip fixed cells; **from `ISOLATION_PT` on, skip cells that are already legal**;
/// 3. `ripUp` — remove the cell's usage BEFORE searching, or it blocks itself;
/// 4. `findBestLocation` — wavefronts, cost, `ScanRank` tie-break;
/// 5. `place` — restore usage at the chosen position.
///
/// Returns the grid's total overuse afterwards.
pub fn negotiation_iter(cells: &mut [SweepCell], active: &mut Vec<usize>, iter: i32,
                        ctx: &mut SweepCtx) -> i32
{
    // 1. Order the sweep.
    let mut keys: Vec<SortKey> = active
        .iter()
        .map(|&i| {
            let c = &cells[i];
            let mut ov = 0;
            for dy in 0..c.height {
                for dx in 0..c.width {
                    if let (Ok(gx), Ok(gy)) = (usize::try_from(c.x + dx), usize::try_from(c.y + dy))
                    {
                        if gx < ctx.grid.width && gy < ctx.grid.height {
                            ov += ctx.grid.at(gx, gy).overuse();
                        }
                    }
                }
            }
            SortKey { overuse: ov, height: c.height, width: c.width, idx: i }
        })
        .collect();
    sort_by_negotiation_order(&mut keys);

    let drc_penalty = ctx.drc_penalty * (1.0 + iter as f64);
    // ⚠️ **The sweep TRACE, and it is the only instrument that can settle an assignment.** Which
    // cell ends up in which slot is decided by the sweep ORDER and by what `findBestLocation`
    // returns for each cell in turn — neither of which any output artefact records. Upstream
    // prints exactly this line from `negotiationIter` under
    // `set_debug_level DPL negotiation 2`:
    //
    //     Negotiation iter {iter}, cell {name}, moves {n}, best location {x}, {y}
    //
    // ⛔ **The format is upstream's, character for character, so the two traces diff directly.**
    // Reformatting it would mean writing a comparison script that can itself be wrong.
    // `moves` counts cells actually swept (fixed and isolation-skipped cells do not increment
    // it), which is upstream's `moves_count`.
    let tracing = std::env::var_os("DPL_TRACE_NEGOTIATION").is_some();
    let mut moves_count = 0;
    // ⚠️ Hoisted and reused across cells. It held one allocation per cell per iteration, which on
    // a design with thousands of active cells and hundreds of iterations is millions of them for
    // a buffer whose contents are discarded each time.
    let mut cands: Vec<Candidate> = Vec::new();

    for key in &keys {
        let i = key.idx;
        if cells[i].fixed {
            continue;
        }
        // 2. ⚠️ The isolation point — from the SECOND iteration a legal cell is left alone.
        if iter >= consts::ISOLATION_PT && cell_is_legal(&cells[i], ctx) {
            continue;
        }
        // 3. ⛔ Rip up FIRST. A cell still counted in `usage` competes with itself for its own
        //    square and the search will refuse to stay put.
        let c = cells[i].clone();
        // ⛔ `ripUp` is `eraseCellFromDplGrid` THEN `addUsage(-1)` — both halves. The occupancy
        // stamp has to go too, or `findBestLocation`'s DRC term sees the cell as still sitting
        // where it was and refuses to put it back there.
        ctx.grid.erase_cell(i, c.x, c.y, c.width, c.height);
        ctx.grid.add_usage(c.x, c.y, c.width, c.height, -1);

        // 4. Enumerate and score.
        let (ilo, ihi, irows) = (ctx.window)(&c, c.init_x, c.init_y);
        let (clo, chi, crows) = if (c.x, c.y) != (c.init_x, c.init_y) {
            (ctx.window)(&c, c.x, c.y)
        } else {
            (0, 0, Vec::new())
        };
        let congestion_floor = c.width as f64 * c.height as f64;
        let mut best = (INF_COST, ScanRank { window: 0, row_pos: usize::MAX, dx: i32::MAX },
                        c.x, c.y);
        cands.clear();
        enumerate_candidates(
            c.init_x, c.init_y, c.x, c.y, &irows, ilo, ihi, &crows, clo, chi,
            &|d| {
                target_cost_from_disp(d, ctx.max_disp_multiplier, ctx.max_disp_threshold)
                    + congestion_floor
                    > best.0
            },
            &mut cands,
        );
        for &cand in &cands {
            if !(ctx.placeable)(&c, cand.x, cand.y) {
                continue;
            }
            let target = target_cost(cand.x, cand.y, c.init_x, c.init_y,
                                     ctx.max_disp_multiplier, ctx.max_disp_threshold);
            let mut cost = negotiation_cost(
                footprint_of(&c, cand.x, cand.y, ctx.grid), target, best.0);
            if cost > best.0 || (cost == best.0 && cand.rank >= best.1) {
                continue; // loses, or ties with a losing rank — skip the costly DRC term
            }
            cost += drc_penalty * (ctx.drc_violations)(&c, cand.x, cand.y, ctx.grid) as f64;
            if cost < best.0 || (cost == best.0 && cand.rank < best.1) {
                best = (cost, cand.rank, cand.x, cand.y);
            }
        }

        // 5. Place — even if nothing better was found, which restores the cell where it was.
        cells[i].x = best.2;
        cells[i].y = best.3;
        // `place` is `addUsage(+1)` then `syncCellToDplGrid`, which is a `paintPixel`.
        ctx.grid.add_usage(best.2, best.3, c.width, c.height, 1);
        ctx.grid.paint_cell(i, best.2, best.3, c.width, c.height);
        moves_count += 1;
        if tracing {
            eprintln!("Negotiation iter {}, cell {}, moves {}, best location {}, {}",
                      iter, c.name, moves_count, best.2, best.3);
        }
    }

    // 5b. ⛔ **Re-sync before counting.** The sweep above punches holes in the occupancy map:
    // ripping up a cell blanks squares a co-located cell still claims, and every DRC-driven
    // count below reads that map. Upstream puts `syncAllCellsToDplGrid()` exactly here, with
    // the reason in its own comment — "leaving bystander cells invisible to DRC checks".
    sync_all_cells_to_grid(cells, ctx.fixed_paint, ctx.grid);

    // 6. Count what is left. ⛔ **NOT `grid.total_overuse()`.**
    //
    // Upstream sums per-square overuse over the ACTIVE cells' footprints — so a square shared by
    // two active cells is counted twice — and then adds **one per illegal active cell**, where
    // illegal is the full `isCellLegal` including the DRC checks.
    //
    // ⚠️ Returning grid overuse alone made a design whose only fault was a DRC violation report
    // ZERO and converge on the first iteration. Measured on
    // `multi_height_one_site_gap_disallow`: a genuine one-site gap between two cells, one sweep,
    // "converged", gap still there.
    let mut violations = 0;
    for &i in active.iter() {
        if cells[i].fixed {
            continue;
        }
        let c = &cells[i];
        for dy in 0..c.height {
            for dx in 0..c.width {
                if let (Ok(gx), Ok(gy)) = (usize::try_from(c.x + dx), usize::try_from(c.y + dy)) {
                    if gx < ctx.grid.width && gy < ctx.grid.height {
                        violations += ctx.grid.at(gx, gy).overuse();
                    }
                }
            }
        }
        if !cell_is_legal(c, ctx) {
            violations += 1;
        }
    }

    // 7. ⛔ **Pull in bystanders that a move has just made illegal.** A cell outside the active
    // set can acquire a one-site gap because its neighbour moved; without this it is never
    // revisited and the run converges with the violation in place.
    let in_active: std::collections::HashSet<usize> = active.iter().copied().collect();
    for i in 0..cells.len() {
        if cells[i].fixed || in_active.contains(&i) {
            continue;
        }
        if !cell_is_legal(&cells[i], ctx) {
            active.push(i);
            violations += 1;
        }
    }

    // 8. History, and ⚠️ **only when something is still wrong** — upstream guards the update with
    // `totalViolations > 0`. Bumping history on a converged iteration would price up squares that
    // nobody is contesting.
    //
    // ⬜ `updateDrcHistoryCosts` belongs here and is not built; it is named in `NOT_DONE`.
    if violations > 0 {
        let footprints: Vec<Vec<usize>> = active
            .iter()
            .map(|&i| {
                let c = &cells[i];
                let mut f = Vec::new();
                for dy in 0..c.height {
                    for dx in 0..c.width {
                        let (gx, gy) = (c.x + dx, c.y + dy);
                        if gx >= 0 && gy >= 0
                            && (gx as usize) < ctx.grid.width && (gy as usize) < ctx.grid.height
                        {
                            f.push(gy as usize * ctx.grid.width + gx as usize);
                        }
                    }
                }
                f
            })
            .collect();
        update_history_costs(&mut ctx.grid.pixels, &footprints);
    }
    violations
}

/// The squares a cell at `(x, y)` would cover, as `(usage, capacity, hist_cost)` — `None` for a
/// square off the grid.
///
/// ⛔ **Lazy, and it must stay lazy.** `findBestLocation` evaluates this for every candidate in
/// the window — hundreds per cell per iteration — so a version that collected into a `Vec`
/// allocated once per candidate. Measured 2026-09-02 on `aes` and `ibex`: with the allocating
/// version neither finished inside a 120-second budget. `negotiationCost` also aborts early once
/// the running cost passes the incumbent, and a lazy iterator means the squares past that point
/// are never even read.
fn footprint_of<'a>(c: &SweepCell, x: i32, y: i32, grid: &'a NegGrid)
    -> impl Iterator<Item = Option<(i32, i32, f64)>> + 'a
{
    let (w, h) = (c.width, c.height);
    (0..h).flat_map(move |dy| (0..w).map(move |dx| (x + dx, y + dy))).map(move |(gx, gy)| {
        if gx < 0 || gy < 0 || gx as usize >= grid.width || gy as usize >= grid.height {
            None
        } else {
            let p = grid.at(gx as usize, gy as usize);
            Some((p.usage, p.capacity, p.hist_cost))
        }
    })
}

fn cell_is_legal(c: &SweepCell, ctx: &SweepCtx) -> bool {
    if !(ctx.placeable)(c, c.x, c.y) || (ctx.drc_violations)(c, c.x, c.y, ctx.grid) > 0 {
        return false;
    }
    footprint_is_legal(c, c.x, c.y, ctx.grid)
}

/// `isCellLegal`'s FINAL loop: every square of the footprint must have capacity and no overuse.
///
/// ⛔ **`capacity == 0` is how a BLOCKAGE reaches this test.** `Grid::markBlocked` only sets
/// `is_valid = false` on the squares a hard placement blockage covers; `buildGrid` then turns that
/// into `capacity = is_valid ? 1 : 0`. Nothing earlier in `isCellLegal` — not `inDie`, not
/// `isValidRow`, not `getSiteOrientation` — looks at validity, so **without this loop a cell
/// sitting inside a hard blockage reports as perfectly legal**.
///
/// ⚠️ Measured on `blockage01`: the design has a hard blockage over sites 0..15 of every row and
/// an instance the DEF leaves at `(0, 0)`. Our seeding scan tested overuse alone, called the cell
/// legal, never negotiated it, and left it inside the blockage at `3800`; the reference moves it
/// to `9880`, the first site past the blockage.
///
/// 🔑 `overuse()` is `max(0, usage - capacity)` and the cell IS counted in `usage` here, so a
/// cell alone on its own square reads 0.
///
/// ⬜ **Untested, and it cannot be tested from here.** `buildGrid` only ever assigns capacity 0
/// or 1, so `usage - capacity` and `usage - 1` agree on every square this engine can build. The
/// expression is upstream's; a test asserting it would pass either way and prove nothing.
fn footprint_is_legal(c: &SweepCell, x: i32, y: i32, grid: &NegGrid) -> bool {
    footprint_of(c, x, y, grid)
        .all(|sq| matches!(sq, Some((u, cap, _)) if cap > 0 && (u - cap).max(0) == 0))
}

// ── the driver ───────────────────────────────────────────────────────────────────────────────

use crate::grid::Grid;
use crate::place::{Legalized, Placed};
use vyges_opendb::Db;

/// Families of behaviour the NEGOTIATION legalizer does not implement.
///
/// ⛔ Reported on every run for the same reason `place::NOT_DONE` is: a legalizer that quietly
/// omits a check reports fewer violations than it earned.
pub const NOT_DONE: &[&str] = &[
    "groups_and_regions (respectsFence / initFenceRegions)",
    "master_symmetry (checkMasterSym)",
    // ⚠️ ONE of `countDRCViolations`' four terms is evaluated (one-site gaps). The other three
    // need the occupant's identity or its master's edge list, so the DRC penalty is an
    // UNDER-count — it never over-reports, but it will prefer a location upstream would not.
    "drc: padding, edge_spacing and blocked_layers are not evaluated by the legalizer",
    "drc_history_costs (updateDrcHistoryCosts)",
    "diamondRecovery on stall",
];

/// `NegotiationLegalizer::legalize` — the DEFAULT detailed-placement path.
///
/// ⚠️ **Default, not the fallback.** `use_diamond_legalizer_` defaults to false and
/// `isUseNegotiationLegalizer()` is its negation, so `diamondDPL` runs only when
/// `-use_diamond_legalizer` is passed — **4 of 67 upstream cases**.
///
/// **The call sequence, upstream's, in order:**
///
/// 1. `initFromDb` — cells enter the model by [`enters_model`]; each gets an
///    [`init_position`] in grid units that never changes afterwards;
/// 2. `buildGrid` — capacity 1 per valid site, `blockade` for fixed cells;
/// 3. seed `addUsage(+1)` for every movable cell where it currently sits;
/// 4. scan for illegal cells; the active set is those plus nothing else — ⚠️ a legal cell is
///    only ever revisited because a neighbour's search moved into it, which shows up as overuse;
/// 5. [`run_negotiation`] — two phases, [`negotiation_iter`] per iteration, history updated
///    after each, diamond recovery on a 3-iteration stall;
/// 6. sync back: grid units → absolute DBU, ⚠️ **orientation from the row landed in**.
pub fn legalize(db: &Db) -> Result<Legalized, String> {
    let grid = Grid::build(db)?;
    let core = grid.core;
    let (gw, gh) = (grid.row_site_count as i32, grid.row_count as i32);
    let sw = grid.site_width;

    let mut out = Legalized {
        not_done: NOT_DONE.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };

    // 2. The grid. A site is valid where the row model says a cell may sit.
    let valid = |x: usize, y: usize| grid.pixel(x as i64, y as i64).is_some_and(|p| p.is_valid);
    let mut ngrid = NegGrid::build(gw.max(0) as usize, gh.max(0) as usize, &valid);

    // 1. + 3. The cells.
    // Routing levels, shared with the power model (which reads pin geometry on ROUTING layers).
    let levels = {
        let layers = db.layers_with_direction().unwrap_or_default();
        let types: Vec<(String, String)> = layers
            .iter()
            .map(|(n, _)| (n.clone(), db.layer_get_type(n).unwrap_or_default()))
            .collect();
        crate::drc::routing_levels(&types)
    };
    let mut cells: Vec<SweepCell> = Vec::new();
    let mut sites: Vec<String> = Vec::new();
    let mut masters: Vec<String> = Vec::new();
    // ⛔ **The occupancy map needs the fixed cells too.** They are not in the sweep — nothing
    // moves them — but `PlacementDRC` reads every occupant through `pixel->cell`, so leaving
    // them out makes each check blind to exactly the obstacles that matter most.
    let mut fixed_boxes: Vec<(i32, i32, i32, i32)> = Vec::new();
    for i in 0..db.num_insts() {
        let name = db.nth_inst_name(i);
        let master = db.inst_master(&name);
        let (x, y) = db.inst_location(&name);
        let (w, h) = (db.master_get_width(&master) as i32, db.master_get_height(&master) as i32);
        let mtype = db.master_get_type(&master).unwrap_or_default();
        let status = db.inst_get_placement_status(&name);
        let fixed = status == "FIRM" || status == "LOCKED" || mtype.contains("BLOCK");

        if fixed {
            let (x0, y0, x1, y1) = grid.covering(x - core.0, y - core.1, w, h);
            ngrid.blockade(x0 as i32, y0 as i32, (x1 - x0 + 1) as i32, (y1 - y0 + 1) as i32);
            fixed_boxes.push((x0 as i32, y0 as i32, (x1 - x0 + 1) as i32, (y1 - y0 + 1) as i32));
            continue;
        }
        if !mtype.contains("CORE") || !enters_model(&status, true) {
            continue;
        }
        // ⛔ **`init_position` wants the footprint in GRID UNITS, not DBU**, because its clamp is
        // `grid_w - width` and `grid_h - height`. Passing the master's DBU width and height makes
        // both clamps negative, `.max(0)` turns them into `clamp(0, 0)`, and EVERY cell starts at
        // site 0 of row 0 — which then makes `targetCost` pull the whole design to the core's
        // bottom-left corner. Measured on `fragmented_row04`: the cell reported `init_grid
        // [0, 0]` for an instance the DEF places at (8360, 5600), and landed at site 0.
        let cw = cell_width_in_sites(w as i64, sw as i64);
        // ⛔ From the MASTER, not from where the cell sits. See `Grid::grid_height`.
        let ch = grid.grid_height(h, db.row_pattern(&db.master_get_site(&master))
                                          .map_or(0, |p| p.len()));
        let (gx, gy) = init_position(
            x as i64, y as i64, core.0 as i64, core.1 as i64, sw as i64,
            &grid.row_y, cw, ch, gw, gh,
        );
        // ⛔ Seeded where the cell IS. `init_x/init_y` are the same point and never move again —
        // `targetCost` measures displacement from them for the whole run.
        ngrid.add_usage(gx, gy, cw, ch, 1);
        cells.push(SweepCell {
            name: name.clone(), x: gx, y: gy, init_x: gx, init_y: gy,
            width: cw, height: ch, fixed: false,
        });
        sites.push(db.master_get_site(&master));
        masters.push(master.clone());
    }

    // Indices past the movable cells, so `pixel->cell` names exactly one occupant either way.
    let fixed_paint: Vec<FixedPaint> = fixed_boxes
        .iter()
        .enumerate()
        .map(|(k, &(x, y, w, h))| (cells.len() + k, x, y, w, h))
        .collect();
    // `syncAllCellsToDplGrid` — the seeding call `legalize` makes right after `addUsage`, before
    // the first legality scan. ⛔ Without it the map is empty and every DRC check passes
    // vacuously on the first pass.
    sync_all_cells_to_grid(&cells, &fixed_paint, &mut ngrid);

    // The closures the sweep needs, bound to this database.
    //
    // ⚠️ `row_has_sites_` is "some square in this row has capacity", which is the NegGrid's
    // question, not "a pixel exists there".
    //
    // 🔑 Snapshotted rather than queried: `buildGrid` fixes capacity and the sweep only ever
    // changes USAGE, so this cannot go stale — and a closure holding the grid would stop the
    // sweep borrowing it mutably.
    let row_has_sites: Vec<bool> = (0..gh).map(|r| ngrid.row_has_sites(r)).collect();
    let row_has = |r: i32| r >= 0 && (r as usize) < row_has_sites.len()
                           && row_has_sites[r as usize];
    // ⚠️ Geometry is copied out so the closures below do not borrow `cells` — the sweep needs it
    // mutably. The dimensions never change during a run, so the copy cannot go stale.
    let dims: Vec<(i32, i32)> = cells.iter().map(|c| (c.width, c.height)).collect();
    let power = PowerModel::build(db, &grid, &levels);

    // `isValidRow`, transcribed. ⛔ **The site map is consulted at the BOTTOM ROW and at the
    // cell's own x — once, not over the footprint.** Upstream calls
    // `getSiteOrientation(gridX, rowIdx, site)` with exactly those two coordinates; the rows
    // above only have to have SOME sites, and the columns to the right are not asked at all.
    //
    // 🔑 That is not a gap in upstream, because full-footprint validity arrives by another
    // route: `buildGrid` sets `capacity = is_valid ? 1 : 0` and `isCellLegal`'s final loop
    // rejects any square with `capacity == 0`. Site identity and square usability are two
    // different questions and upstream asks them in two different places.
    //
    // ⛔ Requiring the SITE at every square — which this did until 2026-09-02 — refuses cells
    // upstream accepts, because a `dbRow` registers its site only at the grid row of its ORIGIN.
    // A double-height row at y = 11200 puts `DoubleHeightSite` on grid row 4 and nothing on row
    // 5, so a 2-row cell seated there failed the second row and was dropped.
    // ⚠️ Measured on `multi_height_one_site_gap_disallow`: 2 of its 3 cells were reported as
    // failures and left untouched at their input positions.
    let site_ok = |i: usize, x: i32, y: i32| -> bool {
        let (cw, ch) = dims[i];
        // `rowIdx < 0 || rowIdx + cell.height > grid_h_`, plus our horizontal bound.
        if x < 0 || y < 0 || y + ch > gh || x + cw > gw {
            return false;
        }
        // Every row the cell spans must have sites.
        if !(0..ch).all(|dy| row_has(y + dy)) {
            return false;
        }
        // The bottom row must OFFER this cell's site, at this x.
        if !grid.site_valid_at(x as i64, y as i64, &sites[i]) {
            return false;
        }
        // ⬜ `checkMasterSym` belongs here and is not built.
        // ⚠️ The power test is last: it is the expensive one, and upstream reaches it only for a
        // multi-row master.
        ch <= 1 || power.compatible(&masters[i], y, ch)
    };

    // `checkDRC`'s one-site-gap term, from OCCUPANCY alone.
    //
    // ⛔ **`disallow_one_site_gaps_` is DERIVED**: `importDb` sets it to
    // `!odb::hasOneSiteMaster(db_)`. Where a one-site-wide master exists the gap is fillable and
    // upstream does not apply the rule at all.
    //
    // 🔑 `checkOneSiteGap` asks only whether a square holds A cell, never WHICH — so
    // `NegGrid.usage > 0` answers it exactly. `ripUp`/`place` keep usage in step through the
    // sweep, and `blockade` gives a fixed cell `usage = 1`, so fixed neighbours count too.
    //
    // ⬜ The other three `PlacementDRC` rules are NOT wired here: padding and edge spacing need
    // the OCCUPANT'S IDENTITY (a class-pair table and a master edge list), which this grid does
    // not carry. They stay in `NOT_DONE`.
    let one_site_gaps_disallowed = !db.has_one_site_master();
    let gap_ok = |i: usize, x: i32, y: i32, g: &NegGrid| -> bool {
        let (cw, ch) = dims[i];
        crate::drc::check_one_site_gap(
            one_site_gaps_disallowed, x, x + cw, y, y + ch,
            crate::drc::EdgeReading::OffGridIsOccupied,
            &|px, py| {
                if px < 0 || py < 0 || px as usize >= g.width || py as usize >= g.height {
                    None // off the grid
                } else {
                    // ⛔ **`pixel->cell`, not `usage > 0`.** `checkOneSiteGap`'s `isAbutted` and
                    // `cellAtSite` both test the occupancy slot, and the two answers diverge on
                    // a shared square: rip-up clears the slot while `usage` still counts the
                    // co-located cell. Reading usage reports an abutment upstream does not see.
                    Some(g.occupant(px, py).is_some())
                }
            })
    };

    // 4. Who starts illegal.
    //
    // ⛔ Upstream's seeding scan is `isCellLegal`, which CALLS `checkDRC`. Leaving the DRC out
    // here would seed a cell that violates a placement rule as already legal, and the sweep would
    // never look at it again.
    let mut illegal: Vec<usize> = Vec::new();
    for i in 0..cells.len() {
        let c = &cells[i];
        // ⛔ The SAME footprint test the sweep's isolation skip uses — capacity AND overuse.
        // Testing overuse alone here let a cell inside a hard blockage seed as legal.
        if !footprint_is_legal(c, c.x, c.y, &ngrid)
            || !site_ok(i, c.x, c.y)
            || !gap_ok(i, c.x, c.y, &ngrid)
        {
            illegal.push(i);
        }
    }

    // ⛔ **The active set is the illegal cells PLUS every movable cell inside their search
    // windows** — upstream's comment: "so the loop can create space organically". A run that
    // negotiates only the illegal cells has nowhere to push their neighbours to, and a cell
    // wedged between two legal ones simply never finds a site.
    //
    // 🔑 Seeded ONCE, before the phases; the set does not grow afterwards.
    //
    // ⚠️ Upstream buckets movable cells by row and binary-searches the x-range so the cost is
    // proportional to what is actually inside the windows, not to the design size. Transcribed,
    // because on a design where most cells are legal a linear scan per seed is the difference
    // between seconds and minutes.
    let active = {
        let mut set: std::collections::BTreeSet<usize> = illegal.iter().copied().collect();
        let mut buckets: Vec<Vec<(i32, usize)>> = vec![Vec::new(); gh.max(0) as usize];
        for (i, c) in cells.iter().enumerate() {
            if c.fixed || c.y < 0 || c.y >= gh || set.contains(&i) {
                continue;
            }
            buckets[c.y as usize].push((c.x, i));
        }
        for b in &mut buckets {
            b.sort_unstable();
        }
        for &idx in &illegal {
            let seed = &cells[idx];
            let site_window = effective_site_window(
                consts::SITE_SEARCH_WINDOW, seed.width, 500, true);
            let row_cap = effective_row_cap(consts::ROW_SEARCH_WINDOW, seed.height, 100, true);
            let (xlo, xhi) = (seed.x - site_window, seed.x + seed.width + site_window);
            let ylo = (seed.y - row_cap).max(0);
            let yhi = (seed.y + seed.height + row_cap).min(gh - 1);
            for yy in ylo..=yhi {
                for &(bx, bi) in &buckets[yy as usize] {
                    if bx > xhi {
                        break; // sorted by x — nothing further can be in range
                    }
                    if bx >= xlo {
                        set.insert(bi);
                    }
                }
            }
        }
        // ⚠️ A `BTreeSet`, where upstream uses an `unordered_set`. The ORDER out of it does not
        // matter — `sortByNegotiationOrder` re-sorts the vector every iteration with a trailing
        // index tie-break — but a deterministic one costs nothing and makes a trace comparable.
        set.into_iter().collect::<Vec<usize>>()
    };

    // 5. Run it. ⚠️ The window is rebuilt per candidate anchor, as upstream does — hoisting it
    //    out of the loop would freeze the reach at the cell's start position.
    let index: Vec<usize> = (0..cells.len()).collect();
    let outcome = if active.is_empty() {
        Outcome::Converged { phase: 1, iter: 0 }
    } else {
        let names: Vec<String> = cells.iter().map(|c| c.name.clone()).collect();
        let by_name: std::collections::HashMap<&str, usize> =
            index.iter().map(|&i| (names[i].as_str(), i)).collect();
        let window = |c: &SweepCell, x: i32, y: i32| -> (i32, i32, Vec<i32>) {
            let i = by_name[c.name.as_str()];
            let w = build_search_window(
                x, y,
                consts::SITE_SEARCH_WINDOW, consts::ROW_SEARCH_WINDOW,
                effective_row_cap(consts::ROW_SEARCH_WINDOW, c.height, 100, true),
                500, 100, true,
                &|px| px < 0 || px >= gw,
                &|px| px >= 0 && px < gw,
                c.height, gh,
                &row_has,
                &|r, lo, hi| (lo.max(0)..=hi.min(gw - 1)).any(|px| site_ok(i, px, r)),
            );
            (w.dx_lo, w.dx_hi, w.rows)
        };
        let placeable = |c: &SweepCell, x: i32, y: i32| site_ok(by_name[c.name.as_str()], x, y);
        // `countDRCViolations`, as far as this engine can evaluate it.
        //
        // ⚠️ **One of upstream's four terms**, so the penalty is an UNDER-count: padding, edge
        // spacing and blocked layers contribute 0 here and are named in `not_done`. A cost
        // function missing a term does not fail — it quietly prefers different locations.
        let idx_of: std::collections::HashMap<&str, usize> =
            index.iter().map(|&i| (names[i].as_str(), i)).collect();
        let drc = |c: &SweepCell, x: i32, y: i32, g: &NegGrid| -> i32 {
            if gap_ok(idx_of[c.name.as_str()], x, y, g) { 0 } else { 1 }
        };

        let mut ctx_cells = cells;
        // ⚠️ The active set GROWS during the run — `negotiationIter` appends bystanders a move
        // has just made illegal — so it is owned by the driver and passed mutably.
        let mut active_set = active;
        let outcome = {
            let mut iterate = |iter: i32| -> i32 {
                let mut ctx = SweepCtx {
                    grid: &mut ngrid, window: &window, placeable: &placeable,
                    drc_violations: &drc,
                    fixed_paint: &fixed_paint,
                    max_disp_multiplier: consts::MAX_DISP_MULTIPLIER,
                    max_disp_threshold: consts::MAX_DISP_THRESHOLD,
                    drc_penalty: consts::DRC_PENALTY,
                };
                // ⛔ The history update lives INSIDE the iteration, gated on the violation count,
                // exactly where upstream puts it.
                negotiation_iter(&mut ctx_cells, &mut active_set, iter, &mut ctx)
            };
            run_negotiation(consts::MAX_ITER_NEG, consts::MAX_ITER_NEG2, &mut iterate, || {})
        };
        cells = ctx_cells;
        outcome
    };

    // 6. Sync back.
    for (i, c) in cells.iter().enumerate() {
        if !site_ok(i, c.x, c.y) {
            // ⛔ **Say WHICH test refused it.** A failure list of bare names cannot be debugged:
            // geometry, the site name and the power rails are three different bugs and they all
            // read as "failed" without this.
            let (cw, ch) = (c.width, c.height);
            let why = if c.x < 0 || c.y < 0 || c.y + ch > gh || c.x + cw > gw {
                format!("a {cw}x{ch} cell at ({}, {}) does not fit the {gw}x{gh} grid", c.x, c.y)
            } else if let Some(r) = (0..ch).map(|dy| c.y + dy).find(|&r| !row_has(r)) {
                format!("row {r} has no sites")
            } else if !grid.site_valid_at(c.x as i64, c.y as i64, &sites[i]) {
                format!("row {} does not offer site `{}` at x {}", c.y, sites[i], c.x)
            } else {
                format!("power rails do not match row {} for a {ch}-row cell", c.y)
            };
            out.failures.push(format!("{}: {why}", c.name));
            continue;
        }
        let nx = c.x * sw;
        let ny = grid.row_y[c.y as usize];
        let orient = grid
            .site_orient_at(c.x as i64, c.y as i64, &sites[i])
            .unwrap_or_else(|| "R0".into());
        out.placed.push(Placed {
            name: c.name.clone(),
            x: nx + core.0,
            y: ny + core.1,
            orient,
            moved: c.x != c.init_x || c.y != c.init_y,
            init_grid: Some((c.init_x, c.init_y)),
            footprint: Some((c.width, c.height)),
        });
    }
    if !matches!(outcome, Outcome::Converged { .. }) {
        out.not_done.push(format!("did not converge: {:?}", outcome));
    }
    Ok(out)
}

// ── power-rail alignment, bound to a database ────────────────────────────────────────────────

/// The design's power model: each master's `(top, bottom)` rails, and each row's `(bottom, top)`.
pub struct PowerModel {
    master: std::collections::HashMap<String, (crate::drc::Power, crate::drc::Power)>,
    /// Per grid row, `(bottom, top)`.
    rows: Vec<(crate::drc::Power, crate::drc::Power)>,
    /// Whether anything was actually determined. ⚠️ When false every check passes — say so.
    pub known: bool,
}

impl PowerModel {
    /// Build it the way `importDb` does: read every master's rails, infer the R0 convention from
    /// the first single-height CORE master that shows one, then assign each row by its orientation.
    ///
    /// ⛔ **Instance order decides the convention**, because `inferR0RowPower` takes the FIRST
    /// master that settles it — so the block's instances are walked in their own order, not the
    /// master table's.
    pub fn build(db: &Db, grid: &Grid, levels: &std::collections::HashMap<String, i32>)
        -> PowerModel
    {
        use crate::drc::Power;
        let mut master: std::collections::HashMap<String, (Power, Power)> =
            std::collections::HashMap::new();
        let mut rails_of = |db: &Db, m: &str| -> (Power, Power) {
            if let Some(&v) = master.get(m) {
                return v;
            }
            let (mut pwr, mut gnd) = (Vec::new(), Vec::new());
            for (term, sig) in db.master_mterms(m).unwrap_or_default() {
                let is_pwr = sig.eq_ignore_ascii_case("POWER");
                let is_gnd = sig.eq_ignore_ascii_case("GROUND");
                if !is_pwr && !is_gnd {
                    continue;
                }
                for (ln, _, y0, _, y1) in db.mterm_pin_boxes(m, &term).unwrap_or_default() {
                    // ⚠️ ROUTING layers only — wells, implants and cuts are skipped, and a well
                    // rectangle spans the whole cell height so counting one would decide every
                    // master's rails wrongly.
                    if !levels.contains_key(&db.layer_name_by_number(ln)) {
                        continue;
                    }
                    let y_centre = (y0 + y1) / 2;
                    if is_pwr { pwr.push(y_centre) } else { gnd.push(y_centre) }
                }
            }
            let v = crate::drc::master_power(&pwr, &gnd);
            master.insert(m.to_string(), v);
            v
        };

        // `inferR0RowPower` — the first single-height CORE master, in INSTANCE order.
        let mut candidates = Vec::new();
        for i in 0..db.num_insts() {
            let m = db.inst_master(&db.nth_inst_name(i));
            let ty = db.master_get_type(&m).unwrap_or_default();
            if !ty.eq_ignore_ascii_case("CORE") {
                continue;
            }
            let h = db.master_get_height(&m) as i32;
            if grid.grid_height(h, db.row_pattern(&db.master_get_site(&m)).map_or(0, |p| p.len()))
                > 1
            {
                continue; // multi-height masters are skipped
            }
            candidates.push(rails_of(db, &m));
        }
        let (r0_top, r0_bot) = crate::drc::infer_r0_row_power(candidates.into_iter());

        let rows: Vec<(Power, Power)> = if r0_bot == Power::Unknown {
            // ⛔ Nothing settled it, so EVERY row stays unknown and the check is a no-op.
            vec![(Power::Unknown, Power::Unknown); grid.row_count]
        } else {
            (0..grid.row_count)
                .map(|r| crate::drc::row_power(r0_top, r0_bot,
                                               grid.row_orient.get(r).map_or("R0", |s| s)))
                .collect()
        };
        let known = r0_bot != Power::Unknown;
        // Make sure every master a caller may ask about is cached, not just the candidates.
        for i in 0..db.num_insts() {
            let m = db.inst_master(&db.nth_inst_name(i));
            rails_of(db, &m);
        }
        PowerModel { master, rows, known }
    }

    /// `Opendp::checkRowPowerCompatible` — may this master start in this grid row?
    ///
    /// ⚠️ Returns `true` when the model is unknown, which is what upstream does: `powerCompatible`
    /// treats `Power_UNK` as matching anything.
    pub fn compatible(&self, master: &str, row: i32, rows_spanned: i32) -> bool {
        if !self.known || row < 0 {
            return true;
        }
        let (top, bot) = *self.master.get(master)
            .unwrap_or(&(crate::drc::Power::Unknown, crate::drc::Power::Unknown));
        crate::drc::power_compatible(
            bot, top, row as usize, rows_spanned.max(0) as usize, self.rows.len(),
            &|r| self.rows.get(r).map_or(crate::drc::Power::Unknown, |p| p.0),
            &|r| self.rows.get(r).map_or(crate::drc::Power::Unknown, |p| p.1),
        ).0
    }
}
