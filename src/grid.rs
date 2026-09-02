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
        }

        let mut g = Grid { core, site_width, row_count, row_site_count, pixels, row_sites, row_y };
        g.mark_blocked(db);
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

    /// The grid row index a core-relative Y sits in, if any.
    pub fn grid_y(&self, y: i32) -> Option<usize> {
        (0..self.row_count).find(|&i| self.row_y[i] <= y && y < self.row_y[i + 1])
    }

    /// `Grid::gridCovering` for a cell's box, in core-relative DBU.
    pub fn covering(&self, x: i32, y: i32, w: i32, h: i32) -> (i64, i64, i64, i64) {
        let xlo = (x / self.site_width) as i64;
        let xhi = ((x + w + self.site_width - 1) / self.site_width) as i64;
        let ylo = self.grid_y(y).map(|v| v as i64).unwrap_or(-1);
        let yhi = self
            .grid_y(y + h - 1)
            .map(|v| v as i64 + 1)
            .unwrap_or(ylo + 1);
        (xlo, ylo, xhi, yhi)
    }

    /// How many squares are usable — the number a legal placement has to fit into.
    pub fn valid_sites(&self) -> usize {
        self.pixels.iter().flatten().filter(|p| p.is_valid).count()
    }
}
