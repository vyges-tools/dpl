// SPDX-License-Identifier: Apache-2.0
//! `PlacementDRC::FixedSupplyVias` — `detailed_placement -pdn_aware` (upstream PR 11120, at
//! `4155ac3`): a cell may not sit where one of its shapes comes closer than spacing to the metal
//! of a FIXED SUPPLY VIA, unless the shape is a pin on that via's own net.
//!
//! What upstream reads, in its order:
//!
//! 1. the fixed shapes — every SPECIAL net of supply signal type, every via of its special wires,
//!    decomposed onto its layers (`getViaBoxes`), kept on ROUTING and CUT layers only;
//! 2. the masters of the block's core auto-placeable, non-block instances, and each one's shapes
//!    on those layers — obstructions first (no terminal), then every terminal's pin geometry;
//! 3. a layer no master has a shape on is dropped: nothing can come near it.
//!
//! The test at a candidate `(site x, row y, orientation)` (`FixedSupplyVias::check`) asks, for
//! every master shape in that orientation and every fixed shape on its layer, whether the site
//! lies in the shape's keep-out (`addConflict`). An obstruction in conflict fails the site at once;
//! a pin in conflict fails it unless the instance's net on that terminal IS the fixed shape's net.
//!
//! 🔑 Upstream computes the answer 64 sites at a time (`generatePage`, cached per master, orient,
//! row and page) and skips the test outright at legal sites when no fixed shape can reach any
//! cell vertically (`legal_sites_safe_`). Both are caches of the same answer — the page masks are
//! the per-site test over a range, and the shortcut holds only where every keep-out is out of
//! vertical reach — so this module asks the per-site question directly.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use vyges_opendb::Db;

use crate::drc::EdgeBox;
use crate::grid::Grid;

/// One fixed supply-via shape: its rectangle (absolute DBU) and its net, by index in block order.
#[derive(Clone, Debug)]
struct Fixed {
    rect: EdgeBox,
    net: usize,
}

/// One master shape: master coordinates, layer, and the terminal (index into the master's
/// terminals) for a pin shape — `None` for an obstruction.
#[derive(Clone, Debug)]
struct MasterShape {
    rect: EdgeBox,
    layer: String,
    term: Option<usize>,
}

struct LayerData {
    cut: bool,
    shapes: Vec<Fixed>,
    /// `spacing(layer, fixed, cell)` per `(fixed dx, fixed dy, cell dx, cell dy)`.
    spacing: HashMap<(i32, i32, i32, i32), i32>,
}

struct MasterModel {
    boundary: EdgeBox,
    shapes: Vec<MasterShape>,
    /// Terminal name and whether it is a supply (POWER/GROUND) terminal, in `getMTerms` order.
    terms: Vec<(String, bool)>,
}

/// The fixed-supply-via model of one block, built by `initFixedSupplyVias`.
pub struct FixedSupplyVias {
    layers: HashMap<String, LayerData>,
    masters: HashMap<String, MasterModel>,
    nets: Vec<String>,
    core: (i32, i32, i32, i32),
    site_width: i32,
    row_site_count: i32,
    row_count: i32,
    row_y: Vec<i32>,
    /// `DPL-0502` is said once per model.
    warned: RefCell<bool>,
    /// Messages raised by [`FixedSupplyVias::check`], for the caller's log.
    pub warnings: RefCell<Vec<String>>,
}

/// `floorSqrt` — the largest `r` with `r * r <= value`.
pub fn floor_sqrt(value: i64) -> i64 {
    if value <= 0 {
        return 0;
    }
    let (mut low, mut high) = (0i64, value.min(i32::MAX as i64));
    while low <= high {
        let mid = low + (high - low) / 2;
        if mid * mid <= value {
            low = mid + 1;
        } else {
            high = mid - 1;
        }
    }
    high
}

/// `verticalDistance` — the gap between the fixed shape and a cell shape placed at `origin_y`.
pub fn vertical_distance(fixed: EdgeBox, cell: EdgeBox, origin_y: i64) -> i64 {
    let (lo, hi) = (origin_y + cell.1 as i64, origin_y + cell.3 as i64);
    if hi < fixed.1 as i64 {
        fixed.1 as i64 - hi
    } else if lo > fixed.3 as i64 {
        lo - fixed.3 as i64
    } else {
        0
    }
}

/// `horizontalKeepout` — the horizontal reach of a Euclidean spacing `space` across a vertical
/// gap, strict in integer DBU; `-1` when the gap alone clears it.
pub fn horizontal_keepout(space: i32, vertical_gap: i64) -> i64 {
    if space <= 0 || vertical_gap >= space as i64 {
        return -1;
    }
    floor_sqrt(space as i64 * space as i64 - vertical_gap * vertical_gap - 1)
}

fn div_floor(a: i64, b: i64) -> i64 {
    a.div_euclid(b)
}

fn div_ceil(a: i64, b: i64) -> i64 {
    -(-a).div_euclid(b)
}

fn is_supply(sig: &str) -> bool {
    sig == "POWER" || sig == "GROUND"
}

impl FixedSupplyVias {
    /// `FixedSupplyVias(logger, grid, block)`; `None` when no fixed shape survives (`empty()`).
    pub fn build(db: &Db, grid: &Grid) -> Option<FixedSupplyVias> {
        let layer_type = |name: &str| db.layer_get_type(name).unwrap_or_default();
        let used = |name: &str| matches!(layer_type(name).as_str(), "ROUTING" | "CUT");
        let nets = db.net_names();
        let mut layers: HashMap<String, LayerData> = HashMap::new();
        for (ni, net) in nets.iter().enumerate() {
            if !db.net_is_special(net) || !is_supply(&db.net_get_sig_type(net)) {
                continue;
            }
            for (layer_no, from_via, x0, y0, x1, y1) in db.net_swire_expanded_boxes(net).unwrap_or_default() {
                if !from_via || layer_no < 0 {
                    continue;
                }
                let name = db.layer_name_by_number(layer_no);
                if !used(&name) {
                    continue;
                }
                let cut = layer_type(&name) == "CUT";
                layers.entry(name).or_insert_with(|| LayerData { cut, shapes: Vec::new(), spacing: HashMap::new() })
                    .shapes.push(Fixed { rect: (x0, y0, x1, y1), net: ni });
            }
        }
        if layers.is_empty() {
            return None;
        }

        // The masters: core auto-placeable, not a block, instantiated.
        let mut masters: HashMap<String, MasterModel> = HashMap::new();
        for inst in db.inst_names() {
            let m = db.inst_master(&inst);
            if masters.contains_key(&m) || !db.master_is_core_auto_placeable(&m) || db.master_is_block(&m) {
                continue;
            }
            let mut shapes = Vec::new();
            // Obstructions first, with no terminal.
            for (layer_no, x0, y0, x1, y1) in db.master_obstruction_boxes(&m).unwrap_or_default() {
                let name = db.layer_name_by_number(layer_no);
                if used(&name) && layers.contains_key(&name) {
                    shapes.push(MasterShape { rect: (x0, y0, x1, y1), layer: name, term: None });
                }
            }
            let terms: Vec<(String, bool)> = db.master_mterms(&m).unwrap_or_default()
                .into_iter().map(|(t, sig)| { let s = is_supply(&sig); (t, s) }).collect();
            for (k, (t, _)) in terms.iter().enumerate() {
                for (layer_no, x0, y0, x1, y1) in db.mterm_pin_boxes(&m, t).unwrap_or_default() {
                    let name = db.layer_name_by_number(layer_no);
                    if used(&name) && layers.contains_key(&name) {
                        shapes.push(MasterShape { rect: (x0, y0, x1, y1), layer: name, term: Some(k) });
                    }
                }
            }
            let b = db.master_placement_boundary(&m).unwrap_or_default();
            let boundary = if b.len() == 4 { (b[0], b[1], b[2], b[3]) } else { (0, 0, 0, 0) };
            masters.insert(m, MasterModel { boundary, shapes, terms });
        }

        // A layer no master has a shape on can never conflict.
        layers.retain(|name, _| masters.values().any(|m| m.shapes.iter().any(|s| &s.layer == name)));
        if layers.is_empty() {
            return None;
        }

        // `spacing(layer, fixed, cell)` for every pair of dimensions that can meet, both cell
        // orientations of each shape (a quarter turn swaps them).
        for (name, data) in layers.iter_mut() {
            let fixed_dims: Vec<(i32, i32)> = data.shapes.iter().map(|f| (f.rect.2 - f.rect.0, f.rect.3 - f.rect.1)).collect();
            let mut cell_dims: Vec<(i32, i32)> = Vec::new();
            for m in masters.values() {
                for s in m.shapes.iter().filter(|s| &s.layer == name) {
                    let (dx, dy) = (s.rect.2 - s.rect.0, s.rect.3 - s.rect.1);
                    cell_dims.push((dx, dy));
                    cell_dims.push((dy, dx));
                }
            }
            for &(fdx, fdy) in &fixed_dims {
                for &(cdx, cdy) in &cell_dims {
                    data.spacing.entry((fdx, fdy, cdx, cdy)).or_insert_with(|| {
                        spacing(db, name, data.cut, (fdx, fdy), (cdx, cdy))
                    });
                }
            }
        }

        Some(FixedSupplyVias {
            layers, masters, nets,
            core: grid.core, site_width: grid.site_width,
            row_site_count: grid.row_site_count as i32, row_count: grid.row_count as i32,
            row_y: grid.row_y.clone(),
            warned: RefCell::new(false), warnings: RefCell::new(Vec::new()),
        })
    }

    /// The messages raised since the last call, in order — for the caller's log, where upstream's
    /// logger would have printed them.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.borrow_mut())
    }

    /// The instance's net per terminal of its master, `""` where it has none.
    pub fn nets_of(&self, db: &Db, inst: &str, master: &str) -> Vec<String> {
        let Some(m) = self.masters.get(master) else { return Vec::new() };
        let by_term: HashMap<String, String> = db.inst_iterm_info(inst).unwrap_or_default()
            .into_iter().map(|(t, _, n)| (t, n)).collect();
        m.terms.iter().map(|(t, _)| by_term.get(t).cloned().unwrap_or_default()).collect()
    }

    /// `FixedSupplyVias::check(cell, x, y, orient)` — may `inst` (of `master`, with `nets` from
    /// [`FixedSupplyVias::nets_of`]) sit at site `gx` of row `gy` in `orient`?
    #[allow(clippy::too_many_arguments)]
    pub fn check(&self, inst: &str, master: &str, nets: &[String], gx: i32, gy: i32, orient: &str) -> bool {
        if gx < 0 || gx >= self.row_site_count || gy < 0 || gy >= self.row_count {
            return true;
        }
        let Some(m) = self.masters.get(master) else { return true };
        // Pin conflicts, keyed and ordered as upstream's `std::map` of (terminal id, net id).
        let mut entries: BTreeMap<(usize, usize), ()> = BTreeMap::new();
        for s in &m.shapes {
            let Some(data) = self.layers.get(&s.layer) else { continue };
            // `masterShapes(master, orient)`: the orientation, then the boundary's lower-left
            // moved to the origin.
            let rect = crate::drc::transform_edge_rect(s.rect, m.boundary, orient, 0, 0);
            for f in &data.shapes {
                if !self.conflict(data, rect, f.rect, gx, gy) {
                    continue;
                }
                match s.term {
                    // An obstruction in conflict fails the site, whatever else is true.
                    None => return false,
                    Some(k) => { entries.insert((k, f.net), ()); }
                }
            }
        }
        for &(k, net) in entries.keys() {
            let pin_net = nets.get(k).map_or("", String::as_str);
            if pin_net != self.nets[net] {
                if pin_net.is_empty() && m.terms[k].1 && !*self.warned.borrow() {
                    self.warnings.borrow_mut().push(format!(
                        "Unconnected supply pin {inst}/{} prevents same-net PDN exemptions. \
                         Connect supply pins before PDN-aware legalization.", m.terms[k].0));
                    *self.warned.borrow_mut() = true;
                }
                return false;
            }
        }
        true
    }

    /// `addConflict` for one site: does a cell shape (relative to the cell origin) placed at site
    /// `gx` of row `gy` come within the layer's spacing of `fixed`?
    fn conflict(&self, data: &LayerData, cell: EdgeBox, fixed: EdgeBox, gx: i32, gy: i32) -> bool {
        let (core_x, core_y) = (self.core.0 as i64, self.core.1 as i64);
        let key = (fixed.2 - fixed.0, fixed.3 - fixed.1, cell.2 - cell.0, cell.3 - cell.1);
        let space = data.spacing.get(&key).copied().unwrap_or(0);
        if data.cut {
            // A cut keep-out is checked by ROW first: which origins put the cell shape inside the
            // fixed shape grown by `space - 1`.
            let grow = (space - 1).max(0) as i64;
            let o_min = fixed.1 as i64 - grow - cell.3 as i64 - core_y;
            let o_max = fixed.3 as i64 + grow - cell.1 as i64 - core_y;
            let first_origin = self.row_y[0] as i64;
            let last_origin = self.row_y[(self.row_count - 1) as usize] as i64;
            if o_max < first_origin || o_min > last_origin {
                return false;
            }
            let rc = self.row_count as i64;
            let row_begin = (self.row_y.iter().position(|&ry| ry as i64 >= o_min).unwrap_or(self.row_y.len()) as i64)
                .clamp(0, rc - 1);
            let ub = self.row_y.iter().position(|&ry| ry as i64 > o_max).unwrap_or(self.row_y.len());
            let row_end = (ub.saturating_sub(1) as i64).clamp(0, rc - 1);
            if (gy as i64) < row_begin || gy as i64 > row_end {
                return false;
            }
        }
        let origin_y = core_y + self.row_y[gy as usize] as i64;
        let (low, high) = if space == 0 {
            // Zero spacing allows contact, never positive-area overlap.
            if origin_y + cell.3 as i64 <= fixed.1 as i64 || origin_y + cell.1 as i64 >= fixed.3 as i64 {
                return false;
            }
            (fixed.0 as i64 - cell.2 as i64 + 1, fixed.2 as i64 - cell.0 as i64 - 1)
        } else {
            let keep = horizontal_keepout(space, vertical_distance(fixed, cell, origin_y));
            if keep < 0 {
                return false;
            }
            (fixed.0 as i64 - keep - cell.2 as i64, fixed.2 as i64 + keep - cell.0 as i64)
        };
        if high < core_x {
            return false;
        }
        let sw = self.site_width as i64;
        let begin = div_ceil(low - core_x, sw);
        let end = div_floor(high - core_x, sw);
        begin <= gx as i64 && gx as i64 <= end
    }
}

/// `spacing(layer, fixed, cell)`: a cut layer's own spacing; on a routing layer the largest of
/// the width/length rule for either shape and the two-widths table for the pair.
fn spacing(db: &Db, layer: &str, cut: bool, fixed: (i32, i32), cell: (i32, i32)) -> i32 {
    if cut {
        return db.layer_get_spacing(layer);
    }
    let fw = fixed.0.min(fixed.1);
    let cw = cell.0.min(cell.1);
    let len = fixed.0.max(fixed.1).max(cell.0).max(cell.1);
    let a = db.layer_get_spacing_for(layer, fw, len).unwrap_or(0);
    let b = db.layer_get_spacing_for(layer, cw, len).unwrap_or(0);
    let c = db.layer_find_tw_spacing(layer, fw, cw, len).unwrap_or(0);
    a.max(b).max(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rule (floorSqrt): the integer square root rounded down; non-positive values give 0.
    #[test]
    fn floor_sqrt_rounds_down() {
        assert_eq!(floor_sqrt(0), 0);
        assert_eq!(floor_sqrt(-5), 0);
        assert_eq!(floor_sqrt(15), 3);
        assert_eq!(floor_sqrt(16), 4);
        assert_eq!(floor_sqrt(17), 4);
    }

    // Rule (horizontalKeepout): spacing is STRICT Euclidean in integer DBU, so the reach across a
    // gap g is floor(sqrt(s^2 - g^2 - 1)); a gap of s or more, or no spacing, reaches nothing.
    #[test]
    fn keepout_is_strict_euclidean() {
        assert_eq!(horizontal_keepout(5, 0), 4, "sqrt(24): a shape 5 away is legal, 4 is not");
        assert_eq!(horizontal_keepout(5, 3), 3, "sqrt(15)");
        assert_eq!(horizontal_keepout(5, 4), 2, "sqrt(8)");
        assert_eq!(horizontal_keepout(5, 5), -1);
        assert_eq!(horizontal_keepout(0, 0), -1);
    }

    // Rule (verticalDistance): the gap between the two y ranges, 0 when they overlap or touch.
    #[test]
    fn vertical_distance_is_the_gap_or_zero() {
        let fixed = (0, 100, 10, 200);
        assert_eq!(vertical_distance(fixed, (0, 0, 10, 50), 0), 50, "cell below");
        assert_eq!(vertical_distance(fixed, (0, 0, 10, 50), 230), 30, "cell above");
        assert_eq!(vertical_distance(fixed, (0, 0, 10, 50), 120), 0, "overlap");
        assert_eq!(vertical_distance(fixed, (0, 0, 10, 50), 50), 0, "touching");
    }

    #[test]
    fn floor_and_ceil_division_follow_the_sign() {
        assert_eq!(div_floor(-1, 4), -1);
        assert_eq!(div_ceil(-1, 4), 0);
        assert_eq!(div_floor(7, 4), 1);
        assert_eq!(div_ceil(7, 4), 2);
        assert_eq!(div_ceil(8, 4), 2);
    }
}
