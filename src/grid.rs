// SPDX-License-Identifier: Apache-2.0
//! The placement grid — `dpl::Grid`.
//!
//! Transcribed from OpenROAD `src/dpl/src/infrastructure/Grid.cpp`.
//!
//! 🔑 **This is the engine, not a helper.** `checkInRows` walks pixel validity and site
//! orientation; `checkOneSiteGaps` walks boundary pixels; `PlacementDRC` paints padding into them;
//! and `diamondDPL`'s search *is* a search over these pixels. Building it once unblocks the
//! checker's remaining families and the legalizer.
//!
//! ## The three facts that are easy to get wrong
//!
//! ⛔ **Everything is CORE-RELATIVE.** Row origins, cell positions and `getRowCoordinates()` are
//! all measured from `core.xMin()`/`core.yMin()`. Reading any of them absolutely produces answers
//! that look catastrophic rather than subtly wrong — measured elsewhere in this crate, an absolute
//! site-alignment test called all 21,340 cells of a clean design misaligned.
//!
//! ⛔ **PAD-class rows are SKIPPED.** `visitDbRows` filters them with the comment *"dpl doesn't
//! deal with pads"*. An IO ring's rows are not placement rows, and including them would invent
//! grid area that no standard cell may occupy.
//!
//! ⛔ **Grid rows are the distinct Y BOUNDARIES, not the rows.** Each `dbRow` contributes TWO
//! entries to the Y index — its origin, and its origin plus the site height — and `row_count` is
//! the number of distinct boundaries MINUS ONE. That is what supports hybrid rows of differing
//! heights; assuming one grid row per `dbRow` gives the right answer only on a uniform design.
//! ## Correlated 2026-09-02
//!
//! | design | reference | this grid |
//! | --- | --- | --- |
//! | `aes.defok` | clean | 21,340 cells, **0** `in_rows` failures |
//! | `fragmented_row04.def` | *"Placed in rows check failed (1)"* | **1**, naming `_277_` |
//!
//! 🔑 Zero false positives across 21,340 cells is the load-bearing half: a grid whose dimensions,
//! core offset or row-boundary model were wrong would light up immediately rather than subtly.
use std::collections::{BTreeMap, BTreeSet};
use vyges_opendb::Db;

/// One grid square.
#[derive(Debug, Clone, Default)]
pub struct Pixel {
    /// A row covers this square, and no hard blockage has taken it away.
    pub is_valid: bool,
    /// A cell sits here — upstream's `pixel->cell`, as the one bit most callers read.
    pub occupied: bool,
    /// WHICH cell sits here, as an index into the caller's cell table.
    ///
    /// ⛔ **Identity, not just occupancy.** Every `PlacementDRC` rule needs it: `checkPadding`
    /// asks whether the occupant's CLASS conflicts with this cell's, `checkEdgeSpacing` skips
    /// `pixel->cell == cell` and de-duplicates neighbours it has already compared, and
    /// `checkOneSiteGap` distinguishes "occupied" from "occupied by me". A boolean cannot answer
    /// any of the three.
    pub cell: Option<u32>,
    /// `pixel->padding_reserved_by` — the cell whose PADDING claims this square without its
    /// body covering it. ⚠️ A separate field upstream, and separately consulted by
    /// `checkPadding`: a square can be reserved by one cell's padding and occupied by no one.
    pub padding_reserved_by: Option<u32>,
    /// Bitmask of routing levels blocked here, `1 << level`.
    pub blocked_layers: u32,
}

/// The placement grid.
pub struct Grid {
    pub core: (i32, i32, i32, i32),
    pub site_width: i32,
    pub row_count: usize,
    pub row_site_count: usize,
    pixels: Vec<Vec<Pixel>>,
    /// Grid row -> the sites valid there, as `(lo, hi, site, orientation)` over x interval
    /// `[lo, hi)`.
    ///
    /// ⛔ **The orientation belongs to the (interval, SITE) entry, not to the row.** Upstream's
    /// `row_sites_` is an interval map to a `site -> orientation` table for exactly this reason:
    /// several `dbRow`s with DIFFERENT sites and DIFFERENT orientations can start at the same Y,
    /// and a multi-row master's site may be registered there with the opposite orientation to
    /// the single-height one beside it.
    ///
    /// ⚠️ Collapsing this to one orientation per row is silent on a uniform design — every row
    /// there has a single site — and wrong on a hybrid one, where the last row parsed decides
    /// the orientation of every cell in that row regardless of its master.
    row_sites: Vec<Vec<(usize, usize, String, String)>>,
    /// Core-relative Y of each grid row boundary.
    pub row_y: Vec<i32>,
    /// The orientation of the LAST row parsed at each grid row.
    ///
    /// ⚠️ **Only meaningful where a grid row has one site.** Ask
    /// [`Grid::site_orient_at`] for the orientation a particular master would take — that is
    /// keyed by site, as upstream's is.
    pub row_orient: Vec<String>,
    /// Whether `mark_blocked_layers` found any special wire to record.
    blocked_layers_populated: bool,
    /// The SITE height of every row in the block, in the order the rows were read.
    ///
    /// ⛔ Kept because `uniform_row_height_` is derived from SITE heights, not from the spacing
    /// between grid-row boundaries. The two are different questions and give different answers.
    site_heights: Vec<i32>,
}

impl Grid {
    /// `Grid::initGrid` — allocate, mark valid from the rows, then subtract hard blockages.
    ///
    /// ⚠️ **`markHopeless`'s "hopeless" marking is deliberately NOT here.** It exists to prune the
    /// legalizer's diamond search and has no bearing on whether a placement is legal, so building
    /// it now would be untested weight. It is needed when `diamondDPL` lands, not before.
    pub fn build(db: &Db) -> Result<Grid, String> {
        let core = (
            db.block_get_core_area_x_min(),
            db.block_get_core_area_y_min(),
            db.block_get_core_area_x_max(),
            db.block_get_core_area_y_max(),
        );

        // Rows, PAD class excluded, in core-relative coordinates.
        let mut rows = Vec::new();
        let mut site_width = 0i32;
        for i in 0..db.num_rows().unwrap_or(0) {
            let Ok(Some((bbox, site, orient))) = db.nth_row(i) else { continue };
            // ⛔ "dpl doesn't deal with pads" — Grid::visitDbRows.
            if db.site_get_class(&site).unwrap_or_default() == "PAD" {
                continue;
            }
            let w = db.site_get_width(&site);
            if site_width == 0 {
                site_width = w;
            } else if w != site_width {
                // Upstream errors DPL-51 rather than guessing which width the grid uses.
                return Err(format!("site widths are not equal: {site_width} != {w} ({site})"));
            }
            // ⚠️ `row_get_site_count` is keyed by row NAME, not by the index `nth_row` uses.
            let name = db.nth_row_name(i).unwrap_or_default();
            let count = db.row_get_site_count(&name).max(0) as usize;
            let h = db.site_get_height(&site);
            rows.push((bbox[0] - core.0, bbox[1] - core.1, count, site, orient, h));
        }
        if rows.is_empty() {
            return Err("no rows found".into());
        }
        if site_width <= 0 {
            return Err("site width is zero".into());
        }

        // 🔑 Every row contributes its origin AND its top edge, so a design with two row heights
        // gets a boundary at each. `row_count` is boundaries minus one.
        let mut ys: BTreeSet<i32> = BTreeSet::new();
        for (_, y, _, _, _, h) in &rows {
            ys.insert(*y);
            ys.insert(*y + *h as i32);
        }
        let row_y: Vec<i32> = ys.into_iter().collect();
        let index_of: BTreeMap<i32, usize> =
            row_y.iter().enumerate().map(|(i, y)| (*y, i)).collect();
        let row_count = row_y.len().saturating_sub(1);
        // `divFloor(core.dx, site_width)`.
        let row_site_count = ((core.2 - core.0) / site_width).max(0) as usize;

        let mut pixels = vec![vec![Pixel::default(); row_site_count]; row_count];
        let mut row_sites: Vec<Vec<(usize, usize, String, String)>> = vec![Vec::new(); row_count];
        let mut row_orient: Vec<String> = vec!["R0".into(); row_count];

        for (x, y, count, site, _orient, _h) in &rows {
            let Some(&gy) = index_of.get(y) else { continue };
            if gy >= row_count {
                continue;
            }
            let x_start = (*x / site_width).max(0) as usize;
            let x_end = (x_start + *count).min(row_site_count);
            for gx in x_start..x_end {
                pixels[gy][gx].is_valid = true;
            }
            row_sites[gy].push((x_start, x_end, site.clone(), _orient.clone()));
            row_orient[gy] = _orient.clone();
        }

        let mut g = Grid { core, site_width, row_count, row_site_count, pixels, row_sites, row_y,
                           row_orient, blocked_layers_populated: false,
                           site_heights: rows.iter().map(|r| r.5 as i32).collect() };
        g.mark_blocked(db);
        g.mark_blocked_layers(db);
        Ok(g)
    }

    /// `Grid::markBlocked` — HARD blockages invalidate the squares they cover.
    ///
    /// ⚠️ **Soft blockages are skipped**: they discourage placement, they do not forbid it, so a
    /// cell over one is legal. Treating them as hard would fail designs the reference passes.
    fn mark_blocked(&mut self, db: &Db) {
        let boxes = db.blockage_boxes().unwrap_or_default();
        for (i, b) in boxes.iter().enumerate() {
            if db.blockage_is_soft(i) {
                continue;
            }
            let (xlo, ylo, xhi, yhi) = (b.0 - self.core.0, b.1 - self.core.1,
                                        b.2 - self.core.0, b.3 - self.core.1);
            let gx0 = (xlo / self.site_width).max(0) as usize;
            let gx1 = (((xhi + self.site_width - 1) / self.site_width).max(0) as usize)
                .min(self.row_site_count);
            for gy in self.grid_rows_covering(ylo, yhi) {
                for gx in gx0..gx1 {
                    self.pixels[gy][gx].is_valid = false;
                }
            }
        }
    }

    /// `Grid::markBlocked`'s second half — the `blocked_layers` bitmask, from SPECIAL nets.
    ///
    /// ⛔ **A different thing from `mark_blocked` above, despite sharing upstream's function.**
    /// That one invalidates squares under hard blockages; this one records WHICH low routing
    /// levels a power strap crosses, so `checkBlockedLayers` can refuse a cell whose pins need
    /// one of them. A square with `blocked_layers` set is still perfectly valid for a cell that
    /// does not use those levels.
    ///
    /// ⚠️ Silently does nothing if the routing levels cannot be derived — and that is why
    /// [`Grid::blocked_layer_status`] exists: a checker that reports "clean" from a mask that was
    /// never populated is worse than one that says it could not check.
    fn mark_blocked_layers(&mut self, db: &Db) {
        let Ok(layers) = db.layers_with_direction() else { return };
        let types: Vec<(String, String)> = layers
            .iter()
            .map(|(n, _)| (n.clone(), db.layer_get_type(n).unwrap_or_default()))
            .collect();
        let levels = crate::drc::routing_levels(&types);
        if crate::drc::routing_level_sanity(&levels).is_err() {
            return;
        }
        let Ok(boxes) = db.swire_boxes() else { return };
        for (layer_no, x0, y0, x1, y1) in boxes {
            let name = db.layer_name_by_number(layer_no);
            let Some(&level) = levels.get(&name) else { continue };
            if !crate::drc::blocks_layer(level, (x1 - x0) as i64, (y1 - y0) as i64) {
                continue;
            }
            let (xlo, ylo, xhi, yhi) =
                (x0 - self.core.0, y0 - self.core.1, x1 - self.core.0, y1 - self.core.1);
            let gx0 = (xlo / self.site_width).max(0) as usize;
            let gx1 = (((xhi + self.site_width - 1) / self.site_width).max(0) as usize)
                .min(self.row_site_count);
            for gy in self.grid_rows_covering(ylo, yhi) {
                for gx in gx0..gx1 {
                    self.pixels[gy][gx].blocked_layers |= 1 << level;
                }
            }
            self.blocked_layers_populated = true;
        }
    }

    /// Whether `blocked_layers` was actually populated, and from how many squares.
    ///
    /// 🔑 **A `checkBlockedLayers` pass over an empty mask is VACUOUS.** The caller reports the
    /// count so a clean verdict can be told apart from a check that had nothing to look at.
    pub fn blocked_layer_status(&self) -> (bool, usize) {
        let n = self.pixels.iter().flatten().filter(|p| p.blocked_layers != 0).count();
        (self.blocked_layers_populated, n)
    }

    /// Grid rows whose Y band intersects `[ylo, yhi)`, in core-relative DBU.
    fn grid_rows_covering(&self, ylo: i32, yhi: i32) -> Vec<usize> {
        (0..self.row_count)
            .filter(|&i| self.row_y[i] < yhi && self.row_y[i + 1] > ylo)
            .collect()
    }

    /// `Grid::gridPixel` — `None` outside the grid, as upstream returns `nullptr`.
    pub fn pixel(&self, x: i64, y: i64) -> Option<&Pixel> {
        if x < 0 || y < 0 || y as usize >= self.row_count || x as usize >= self.row_site_count {
            return None;
        }
        Some(&self.pixels[y as usize][x as usize])
    }

    /// `Grid::getSiteOrientation` — is `site` one of the sites valid at this square?
    pub fn site_valid_at(&self, x: i64, y: i64, site: &str) -> bool {
        if y < 0 || y as usize >= self.row_count {
            return false;
        }
        self.row_sites[y as usize]
            .iter()
            .any(|(lo, hi, s, _)| (x as usize) >= *lo && (x as usize) < *hi && s == site)
    }

    /// `Opendp::legalPt(cell, pt)` — clamp a wanted position into the core, then ROUND it to the
    /// nearest site and row.
    ///
    /// ⛔ **Not an optional refinement — without it an UNPLACED cell never enters the grid.** A
    /// cell the DEF leaves at `(0,0)` is at core-relative `(-core.x, -core.y)`, so the diamond
    /// search starts outside its own bounds, every neighbour is rejected by the bounds test, and
    /// the cell reports as unplaceable on a design with an empty row waiting for it. Measured on
    /// `simple01`.
    ///
    /// ⚠️ **ROUND, not floor** (`divRound`, `gridRoundY`). Flooring biases every clamped cell one
    /// site to the left, which is invisible on a design where the site happens to be free and a
    /// wrong answer where it is not.
    pub fn legal_start(&self, x: i32, y: i32, w: i32, h: i32) -> (i64, i64) {
        let max_x = self.row_site_count as i32 * self.site_width - w;
        let cx = x.clamp(0, max_x.max(0));
        let gx = ((cx as f64) / self.site_width as f64).round() as i64;
        let core_dy = self.core.3 - self.core.1;
        let cy = y.clamp(0, (core_dy - h).max(0));
        // Nearest row boundary, which is `gridRoundY`.
        let gy = self
            .row_y
            .iter()
            .take(self.row_count.max(1))
            .enumerate()
            .min_by_key(|(_, ry)| (**ry - cy).abs())
            .map(|(i, _)| i as i64)
            .unwrap_or(0);
        (gx.clamp(0, self.row_site_count as i64 - 1), gy)
    }

    /// The grid row index a core-relative Y sits in, if any.
    pub fn grid_y(&self, y: i32) -> Option<usize> {
        (0..self.row_count).find(|&i| self.row_y[i] <= y && y < self.row_y[i + 1])
    }

    /// The grid squares a cell occupies — upstream's `gridX(cell)` / `gridWidth(cell)` pair.
    ///
    /// ⛔ **`x_end` is `floor(x / site_width) + ceil(w / site_width)`, NOT `ceil((x + w) /
    /// site_width)`.** The two agree when `x` is site-aligned and differ by one column when it is
    /// not — and FIXED cells frequently are not.
    ///
    /// ⚠️ **Measured on `gcd`:** the wider form over-paints every unaligned fixed cell by a
    /// column, making sites unavailable that upstream leaves free, so movable cells get pushed one
    /// site right. `+380` — exactly one site — was the single most common disagreement, 62 cells.
    ///
    /// 🔑 `y_end` is likewise measured from the SNAPPED row's Y (`gridEndY(gridYToDbu(grid_y) +
    /// height)`), not from the cell's raw `y`.
    pub fn covering(&self, x: i32, y: i32, w: i32, h: i32) -> (i64, i64, i64, i64) {
        // ⛔ **`x / site_width`, NOT `div_euclid`.** `Grid::gridX` is plain C++ integer division,
        // which TRUNCATES TOWARD ZERO; `div_euclid` FLOORS. They agree for non-negative `x` and
        // differ for every negative one not exactly on a site boundary — measured, `-190 / 380` is
        // `0` in C++ and `-1` under `div_euclid`.
        //
        // ⚠️ Negative core-relative x is reachable: a cell hanging left of the core, or an
        // unplaced instance the DEF left at (0,0) in a design whose core does not start there.
        // ⛔ Upstream's helper is *named* `divFloor` and does NOT floor — its body is
        // `return dividend / divisor;`. A reader who trusts the name writes `div_euclid` and is
        // wrong for exactly the negative coordinates above. Its siblings `divRound` and `divCeil`
        // DO go through `double` and so match Rust's `f64::round`/`ceil` directly.
        let xlo = (x / self.site_width) as i64;
        let xhi = xlo + ((w + self.site_width - 1) / self.site_width).max(1) as i64;
        let ylo = match self.grid_snap_down_y(y) {
            Some(v) => v as i64,
            None => return (xlo, -1, xhi, -1),
        };
        // From the snapped row's own Y, as `gridEndY` does.
        let top = self.row_y[ylo as usize] + h;
        let yhi = (0..=self.row_count)
            .find(|&i| self.row_y.get(i).copied().unwrap_or(i32::MAX) >= top)
            .map(|v| v as i64)
            .unwrap_or(self.row_count as i64)
            .max(ylo + 1);
        (xlo, ylo, xhi, yhi)
    }

    /// `Grid::gridSnapDownY` — the row a core-relative Y falls in, snapping DOWN.
    ///
    /// ⚠️ Below the first row it clamps to 0 rather than answering `None`: upstream's
    /// `gridSnapDownY` returns an index, and a fixed cell hanging below the core still paints.
    pub fn grid_snap_down_y(&self, y: i32) -> Option<usize> {
        if self.row_count == 0 {
            return None;
        }
        if y < self.row_y[0] {
            return Some(0);
        }
        (0..self.row_count).rev().find(|&i| self.row_y[i] <= y)
    }

    /// `Grid::paintPixel` — mark the squares a cell occupies.
    ///
    /// ⚠️ Padding is painted separately, by [`Grid::paint_cell_padding`], because upstream calls
    /// it separately — `checkPlacement` runs `checkPadding`, THEN paints the padding, THEN
    /// checks edge spacing. Folding the two together would let a cell see its own reservation.
    pub fn paint(&mut self, x: i32, y: i32, w: i32, h: i32, _movable: bool) {
        self.paint_cell(x, y, w, h, None)
    }

    /// `Grid::paintPixel` with the occupant recorded.
    pub fn paint_cell(&mut self, x: i32, y: i32, w: i32, h: i32, occupant: Option<u32>) {
        let (xlo, ylo, xhi, yhi) = self.covering(x, y, w, h);
        if ylo < 0 {
            return;
        }
        for gy in ylo.max(0)..yhi.min(self.row_count as i64) {
            for gx in xlo.max(0)..xhi.min(self.row_site_count as i64) {
                let p = &mut self.pixels[gy as usize][gx as usize];
                p.occupied = true;
                // ⚠️ FIRST writer wins, matching `paintPixel`'s assignment order under the
                // overlap check: a square already claimed keeps its claimant, so the second cell
                // is the one reported as overlapping.
                if p.cell.is_none() {
                    p.cell = occupant;
                }
            }
        }
    }

    /// `Grid::paintCellPadding` — reserve the sites a cell's padding claims either side of it.
    ///
    /// ⛔ **The TWO PAD SPANS ONLY — `[x - left_pad, x)` and `[x_end, x_end + right_pad)`. The
    /// cell's own body is NOT painted here**; that is [`Grid::paint_cell`]'s `cell` field.
    /// Upstream keeps them apart because `checkPadding` consults both and a cell must not be
    /// blocked by its own reservation.
    ///
    /// ⚠️ Painting the body too is invisible while padding is zero — the body square already
    /// carries the same cell in `pixel.cell`, so the class-pair verdict is unchanged — and wrong
    /// the moment a design sets padding. Transcribed from `Grid.cpp`'s two separate loops.
    ///
    /// ⚠️ **LAST writer wins**, unlike `paint_cell`: upstream assigns unconditionally here, so a
    /// square claimed by two cells' padding names the later one.
    pub fn paint_cell_padding(&mut self, x: i64, y: i64, cells_wide: i64, h: i32,
                              left_pad: i64, right_pad: i64, occupant: u32) {
        let y_end = self.grid_end_y(y, h).min(self.row_count as i64);
        let x_end = x + cells_wide;
        let spans = [((x - left_pad).max(0), x.min(self.row_site_count as i64)),
                     (x_end.max(0), (x_end + right_pad).min(self.row_site_count as i64))];
        for (lo, hi) in spans {
            for gy in y.max(0)..y_end {
                for gx in lo..hi {
                    self.pixels[gy as usize][gx as usize].padding_reserved_by = Some(occupant);
                }
            }
        }
    }

    /// `Grid::gridEndY(gridYToDbu(y) + height)` — the row PAST the last one a cell of `h` DBU
    /// starting at grid row `y` covers.
    pub fn grid_end_y(&self, y: i64, h: i32) -> i64 {
        let y0 = *self.row_y.get(y.max(0) as usize).unwrap_or(&0);
        let top = y0 + h;
        match self.row_y.iter().position(|&ry| ry >= top) {
            Some(r) => r as i64,
            None => self.row_count as i64,
        }
    }

    /// `Opendp::checkPixels` — may a cell of `cells_wide` sites and `h` DBU sit at this square?
    ///
    /// Every square it would cover must exist, be **valid** and be **unoccupied**, and the FIRST
    /// ROW must offer the cell's site.
    ///
    /// ⬜ Not checked here, and declared by the caller: group/region membership, one-site gaps,
    /// padding reservations, and master symmetry.
    pub fn can_place(&self, x: i64, y: i64, cells_wide: i64, h: i32, site: &str) -> bool {
        if y < 0 || y as usize >= self.row_count || x < 0 {
            return false;
        }
        let y_end = self.rows_spanned(self.row_y[y as usize], h) as i64 + y;
        if x + cells_wide > self.row_site_count as i64 || y_end > self.row_count as i64 {
            return false;
        }
        for gy in y..y_end {
            for gx in x..x + cells_wide {
                match self.pixel(gx, gy) {
                    None => return false,
                    Some(p) if !p.is_valid || p.occupied => return false,
                    _ => {}
                }
            }
            if gy == y && !self.site_valid_at(x, gy, site) {
                return false;
            }
        }
        true
    }

    /// `Grid::gridHeight(master)` — a cell's height in ROWS, from the MASTER alone.
    ///
    /// ⛔ **Not a function of where the cell currently sits**, and that distinction is the whole
    /// point. `rows_spanned` measures the band a cell actually covers, so a cell resting 10 DBU
    /// above its row straddles two of them; this asks how tall the master IS.
    ///
    /// ⚠️ Measured on `simple02`, whose single cell is deliberately placed at y = 2810 in a
    /// design with a 2800 row pitch: `rows_spanned` called it a 2-row cell, and every rule that
    /// treats multi-row cells specially — power-rail alignment first — then refused it.
    ///
    /// Upstream's three cases, transcribed:
    ///
    /// | | |
    /// | --- | --- |
    /// | uniform row height | `max(1, ceil(master_height / row_height))` |
    /// | non-uniform, site has no ROWPATTERN | **1** |
    /// | non-uniform, site has a ROWPATTERN | the pattern's LENGTH — not a division |
    pub fn grid_height(&self, master_height: i32, row_pattern_len: usize) -> i32 {
        match self.uniform_row_height() {
            Some(rh) if rh > 0 => 1.max((master_height + rh - 1) / rh),
            // ⚠️ Hybrid rows: the pattern length IS the answer. Dividing by some row height would
            // give a different number whenever the pattern mixes row heights, which is the only
            // situation a pattern exists for.
            _ if row_pattern_len > 0 => row_pattern_len as i32,
            _ => 1,
        }
    }

    /// `Grid::uniform_row_height_` — the row height the design reduces to, if it has one.
    ///
    /// ⛔ **NOT "every row is the same height".** Upstream folds the row SITE heights pairwise:
    /// keep the SMALLER when the larger is an exact multiple of it, and give up otherwise. So a
    /// design mixing 2800-tall and 5600-tall rows IS uniform, at **2800** — the double-height
    /// rows are a whole number of single rows.
    ///
    /// ⚠️ **And it is computed from SITE heights, not from the spacing between grid-row
    /// boundaries.** Those are different questions: boundaries come from every row's origin AND
    /// top, so two overlapping row stacks can produce even spacing from sites that do not divide
    /// each other, and uneven spacing from sites that do.
    ///
    /// 🔑 `None` is what selects the ROWPATTERN branch in [`Grid::grid_height`] and upstream's
    /// `isMultiHeight`, so getting this wrong silently changes how tall every master is thought
    /// to be.
    pub fn uniform_row_height(&self) -> Option<i32> {
        let mut acc: Option<i32> = None;
        for &h in &self.site_heights {
            if h <= 0 {
                continue;
            }
            acc = match acc {
                None => Some(h),
                Some(prev) => {
                    let (smaller, larger) = (prev.min(h), prev.max(h));
                    if larger % smaller != 0 {
                        return None; // not uniform, and upstream stops looking
                    }
                    Some(smaller)
                }
            };
        }
        acc
    }

    /// How many grid rows a cell of height `h` starting at core-relative `y` spans.
    ///
    /// 🔑 Not `h / row_height`: grid rows are the distinct Y boundaries, so a hybrid design has
    /// rows of differing heights and the answer depends on WHERE the cell starts.
    pub fn rows_spanned(&self, y: i32, h: i32) -> usize {
        let Some(lo) = self.grid_y(y) else { return 1 };
        let hi = self.grid_y(y + h - 1).unwrap_or(lo);
        hi - lo + 1
    }

    /// The orientation the row at this square imposes — `placeCell` writes it onto the cell.
    pub fn site_orient_at(&self, x: i64, y: i64, site: &str) -> Option<String> {
        if y < 0 || y as usize >= self.row_count {
            return None;
        }
        // ⛔ The orientation of the matching (interval, SITE) entry — NOT the row's. See
        // `row_sites`: on a hybrid design the two differ, and the row's value is whichever row
        // happened to be parsed last.
        self.row_sites[y as usize]
            .iter()
            .find(|(lo, hi, s, _)| (x as usize) >= *lo && (x as usize) < *hi && s == site)
            .map(|(_, _, _, orient)| orient.clone())
    }

    /// How many squares are usable — the number a legal placement has to fit into.
    pub fn valid_sites(&self) -> usize {
        self.pixels.iter().flatten().filter(|p| p.is_valid).count()
    }
}

#[cfg(test)]
mod padding_paint_tests {
    /// A bare grid with `w` sites in one row, every square valid.
    fn grid(w: usize) -> super::Grid {
        super::Grid {
            core: (0, 0, w as i32 * 10, 10),
            site_width: 10,
            row_count: 1,
            row_site_count: w,
            pixels: vec![vec![super::Pixel { is_valid: true, ..Default::default() }; w]],
            row_sites: vec![vec![(0, w, "S".to_string(), "R0".to_string())]],
            row_y: vec![0, 10],
            row_orient: vec!["R0".to_string()],
            blocked_layers_populated: false,
            site_heights: vec![10],
        }
    }

    #[test]
    fn the_body_is_not_reserved_by_its_own_padding() {
        // ⛔ `paintCellPadding` paints the two PAD spans, never the body. A cell blocked by its
        // own reservation is the failure this separation exists to prevent.
        let mut g = grid(10);
        g.paint_cell_padding(4, 0, 2, 10, 1, 1, 7);
        assert_eq!(g.pixel(3, 0).unwrap().padding_reserved_by, Some(7), "left pad");
        assert_eq!(g.pixel(4, 0).unwrap().padding_reserved_by, None, "the body is NOT reserved");
        assert_eq!(g.pixel(5, 0).unwrap().padding_reserved_by, None, "the body is NOT reserved");
        assert_eq!(g.pixel(6, 0).unwrap().padding_reserved_by, Some(7), "right pad");
        assert_eq!(g.pixel(7, 0).unwrap().padding_reserved_by, None, "past the pad");
    }

    #[test]
    fn zero_padding_reserves_nothing_at_all() {
        // ⚠️ The case that hid the bug: at zero padding the body square already carries the cell
        // in `pixel.cell`, so painting it here changed no verdict and no test noticed.
        let mut g = grid(6);
        g.paint_cell_padding(2, 0, 2, 10, 0, 0, 1);
        assert!((0..6).all(|x| g.pixel(x, 0).unwrap().padding_reserved_by.is_none()),
                "zero padding claims no square");
    }

    #[test]
    fn the_later_cells_padding_wins_the_square() {
        // ⚠️ Upstream assigns unconditionally — LAST writer, unlike `paint_cell`'s first.
        let mut g = grid(8);
        g.paint_cell_padding(4, 0, 1, 10, 1, 0, 1);
        g.paint_cell_padding(2, 0, 1, 10, 0, 1, 2);
        assert_eq!(g.pixel(3, 0).unwrap().padding_reserved_by, Some(2));
    }

    #[test]
    fn the_first_cell_keeps_the_square_it_occupies() {
        // ⛔ The opposite rule for the BODY, and the contrast is the point.
        let mut g = grid(8);
        g.paint_cell(20, 0, 20, 10, Some(1));
        g.paint_cell(20, 0, 20, 10, Some(2));
        assert_eq!(g.pixel(2, 0).unwrap().cell, Some(1), "first writer keeps `cell`");
    }

    #[test]
    fn a_pad_reaching_past_the_core_edge_is_clipped_not_wrapped() {
        let mut g = grid(4);
        g.paint_cell_padding(0, 0, 1, 10, 3, 5, 9);
        assert_eq!(g.pixel(0, 0).unwrap().padding_reserved_by, None, "the body");
        assert!((1..4).all(|x| g.pixel(x, 0).unwrap().padding_reserved_by == Some(9)));
    }
}

#[cfg(test)]
mod grid_height_tests {
    fn uniform(pitch: i32, rows: usize) -> super::Grid {
        super::Grid {
            core: (0, 0, 100, pitch * rows as i32),
            site_width: 10, row_count: rows, row_site_count: 10,
            pixels: vec![vec![super::Pixel::default(); 10]; rows],
            row_sites: vec![vec![(0, 10, "S".to_string(), "R0".to_string())]; rows],
            row_y: (0..=rows).map(|i| i as i32 * pitch).collect(),
            row_orient: vec!["R0".to_string(); rows],
            blocked_layers_populated: false,
            site_heights: vec![pitch; rows],
        }
    }

    #[test]
    fn covering_returns_half_open_bounds() {
        // ⛔ **`xhi`/`yhi` are ONE PAST the last square**, which is why every caller iterates
        // `ylo..yhi`. Reading them as inclusive and adding 1 over-paints a row and a column.
        //
        // ⚠️ Measured: the negotiation driver did exactly that, so every FIXED cell blockaded one
        // row too many. On `simple07` the row above the one fixed cell went to capacity 0, the
        // first swept cell found upstream's answer at INF and walked five sites sideways instead.
        // 🔑 The symptom read as "the legalizer will not move cells vertically", not as an
        // off-by-one — a whole-row artefact does not look like arithmetic.
        let g = uniform(2800, 4);
        // One row tall (2800) and three sites wide (30 dbu at a 10-dbu site), at row 0, site 2.
        assert_eq!(g.covering(20, 0, 30, 2800), (2, 0, 5, 1),
                   "sites 2..5 and row 0..1 — three squares on ONE row");
        assert_eq!(g.covering(20, 2800, 30, 5600), (2, 1, 5, 3), "a two-row cell spans rows 1..3");
    }

    #[test]
    fn a_single_height_master_is_one_row_however_the_cell_sits() {
        // ⛔ The `simple02` regression: the cell is at y = 10 in a 2800-pitch design, so
        // `rows_spanned` says 2 and `grid_height` says 1. The master is what decides.
        let g = uniform(2800, 4);
        assert_eq!(g.grid_height(2800, 0), 1);
        assert_eq!(g.rows_spanned(10, 2800), 2, "the OTHER question, and its answer differs");
    }

    #[test]
    fn a_taller_master_rounds_up() {
        let g = uniform(2800, 6);
        assert_eq!(g.grid_height(5600, 0), 2);
        assert_eq!(g.grid_height(8400, 0), 3);
        // ⚠️ CEIL, not round: a master a hair over two rows needs three.
        assert_eq!(g.grid_height(5601, 0), 3);
        assert_eq!(g.grid_height(1, 0), 1, "and never zero");
    }

    #[test]
    fn hybrid_rows_use_the_pattern_length_not_a_division() {
        // ⛔ Sites of 2800 and 1800: 2800 % 1800 != 0, so the reduction gives up. Heights that
        // DIVIDE — 2800 and 5600 — would still be uniform, at 2800.
        let mut g = uniform(2800, 4);
        g.site_heights = vec![2800, 1800];
        assert_eq!(g.uniform_row_height(), None);
        // ⛔ The pattern's length IS the answer; the master height is not consulted at all.
        assert_eq!(g.grid_height(999999, 3), 3);
        assert_eq!(g.grid_height(2800, 0), 1, "no pattern on a hybrid design means one row");
    }

    #[test]
    fn a_uniform_design_ignores_the_row_pattern() {
        // ⚠️ Order matters: uniform is tested FIRST, so a pattern on a uniform design is unused.
        let g = uniform(2800, 4);
        assert_eq!(g.grid_height(2800, 3), 1);
    }
}

#[cfg(test)]
mod site_orientation_tests {
    /// One grid row carrying TWO sites with OPPOSITE orientations — the hybrid case.
    fn two_sites_one_row() -> super::Grid {
        super::Grid {
            core: (0, 0, 100, 10),
            site_width: 10, row_count: 1, row_site_count: 10,
            pixels: vec![vec![super::Pixel { is_valid: true, ..Default::default() }; 10]],
            row_sites: vec![vec![
                (0, 10, "SINGLE".to_string(), "FS".to_string()),
                (0, 10, "DOUBLE".to_string(), "N".to_string()),
            ]],
            row_y: vec![0, 10],
            // ⚠️ The row-level value is the LAST one parsed — which is what the old lookup
            // returned for every master in the row.
            row_orient: vec!["N".to_string()],
            blocked_layers_populated: false,
            site_heights: vec![10],
        }
    }

    #[test]
    fn orientation_follows_the_site_not_the_row() {
        // ⛔ The bug this pins: both masters used to get the row's single orientation, so a
        // multi-row master in a row shared with single-height ones came out flipped.
        let g = two_sites_one_row();
        assert_eq!(g.site_orient_at(0, 0, "SINGLE").as_deref(), Some("FS"));
        assert_eq!(g.site_orient_at(0, 0, "DOUBLE").as_deref(), Some("N"));
    }

    #[test]
    fn a_site_the_row_does_not_offer_has_no_orientation() {
        let g = two_sites_one_row();
        assert_eq!(g.site_orient_at(0, 0, "ABSENT"), None);
        assert_eq!(g.site_orient_at(0, 1, "SINGLE"), None, "off the grid vertically");
    }

    #[test]
    fn the_interval_bounds_the_lookup() {
        let mut g = two_sites_one_row();
        g.row_sites[0] = vec![(2, 5, "S".to_string(), "FS".to_string())];
        assert_eq!(g.site_orient_at(1, 0, "S"), None, "below the interval");
        assert_eq!(g.site_orient_at(2, 0, "S").as_deref(), Some("FS"), "lo is inclusive");
        assert_eq!(g.site_orient_at(5, 0, "S"), None, "hi is EXCLUSIVE");
    }
}

#[cfg(test)]
mod uniform_row_height_tests {
    fn with_site_heights(hs: Vec<i32>) -> super::Grid {
        super::Grid {
            core: (0, 0, 100, 100), site_width: 10, row_count: 1, row_site_count: 10,
            pixels: vec![vec![super::Pixel::default(); 10]],
            row_sites: vec![vec![]], row_y: vec![0, 10],
            row_orient: vec!["R0".to_string()], blocked_layers_populated: false,
            site_heights: hs,
        }
    }

    #[test]
    fn heights_that_divide_are_uniform_at_the_smaller_one() {
        // ⛔ NOT "all rows equal". A design mixing single- and double-height rows is uniform.
        assert_eq!(with_site_heights(vec![2800, 5600]).uniform_row_height(), Some(2800));
        assert_eq!(with_site_heights(vec![5600, 2800]).uniform_row_height(), Some(2800),
                   "and the order it meets them in does not matter");
        assert_eq!(with_site_heights(vec![2800, 5600, 8400]).uniform_row_height(), Some(2800));
    }

    #[test]
    fn heights_that_do_not_divide_are_not_uniform() {
        assert_eq!(with_site_heights(vec![2800, 1800]).uniform_row_height(), None);
        // ⚠️ And once it gives up it stays given up — a later divisible pair does not rescue it.
        assert_eq!(with_site_heights(vec![2800, 1800, 2800]).uniform_row_height(), None);
    }

    #[test]
    fn a_single_row_height_is_itself() {
        assert_eq!(with_site_heights(vec![2800]).uniform_row_height(), Some(2800));
        assert_eq!(with_site_heights(vec![]).uniform_row_height(), None, "no rows, no height");
    }

    #[test]
    fn it_reads_site_heights_not_boundary_spacing() {
        // 🔑 The distinction that makes this its own function: grid-row BOUNDARIES come from
        // every row's origin and top, so evenly spaced boundaries can arise from sites that do
        // not divide each other. Boundary spacing here is uniform; the sites are not.
        let mut g = with_site_heights(vec![2800, 1800]);
        g.row_y = vec![0, 100, 200, 300];
        assert_eq!(g.uniform_row_height(), None);
    }
}
