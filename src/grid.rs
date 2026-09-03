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
    /// Grid row -> the site names valid there, by x interval `[lo, hi)`.
    row_sites: Vec<Vec<(usize, usize, String)>>,
    /// Core-relative Y of each grid row boundary.
    pub row_y: Vec<i32>,
    /// The orientation each grid row imposes on a cell placed in it.
    pub row_orient: Vec<String>,
    /// Whether `mark_blocked_layers` found any special wire to record.
    blocked_layers_populated: bool,
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
        let mut row_sites: Vec<Vec<(usize, usize, String)>> = vec![Vec::new(); row_count];
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
            row_sites[gy].push((x_start, x_end, site.clone()));
            row_orient[gy] = _orient.clone();
        }

        let mut g = Grid { core, site_width, row_count, row_site_count, pixels, row_sites, row_y,
                           row_orient, blocked_layers_populated: false };
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
            .any(|(lo, hi, s)| (x as usize) >= *lo && (x as usize) < *hi && s == site)
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

    /// The row height, if every row in the grid has the same one.
    ///
    /// ⚠️ `None` for a hybrid-row design, which is what selects the ROWPATTERN branch above.
    pub fn uniform_row_height(&self) -> Option<i32> {
        let mut heights = self.row_y.windows(2).map(|w| w[1] - w[0]);
        let first = heights.next()?;
        if heights.all(|h| h == first) { Some(first) } else { None }
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
        self.row_sites[y as usize]
            .iter()
            .find(|(lo, hi, s)| (x as usize) >= *lo && (x as usize) < *hi && s == site)
            .map(|(_, _, _)| self.row_orient[y as usize].clone())
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
            row_sites: vec![vec![(0, w, "S".to_string())]],
            row_y: vec![0, 10],
            row_orient: vec!["R0".to_string()],
            blocked_layers_populated: false,
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
            row_sites: vec![vec![(0, 10, "S".to_string())]; rows],
            row_y: (0..=rows).map(|i| i as i32 * pitch).collect(),
            row_orient: vec!["R0".to_string(); rows],
            blocked_layers_populated: false,
        }
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
        // Rows of 2800 then 1400, alternating — no uniform height.
        let mut g = uniform(2800, 4);
        g.row_y = vec![0, 2800, 4200, 7000, 8400];
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
