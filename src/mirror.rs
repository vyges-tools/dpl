// SPDX-License-Identifier: Apache-2.0
//! `optimize_mirroring` — `OptimizeMirroring::run`: flip placed cells about Y where that does not
//! lengthen their nets.
//!
//! The call sequence, one function per stage: [`find_net_boxes`] → sort by HPWL →
//! [`find_mirror_candidates`] → [`mirror_candidates`].
//!
//! 🔑 **HPWL is read LIVE from the database** (`dbNet::getTermBBox`, which averages each pin's
//! transformed shapes via `dbITerm::getAvgXY`), so after a flip the bindings return exactly the box
//! upstream recomputes — including the inverted `mergeInit` box of a net with no terms, whose
//! `dx + dy` C++ computes and this only reads.

use std::collections::{HashMap, HashSet};

use vyges_opendb::Db;

use crate::grid::Grid;

/// `OptimizeMirroring::mirror_max_iterm_count_`: nets with MORE iterms are ignored ("Reducing HPWL
/// on large nets (like clocks) is irrelevant to mirroring criteria").
pub const MIRROR_MAX_ITERM_COUNT: usize = 100;

#[derive(Debug, Default, serde::Serialize)]
pub struct MirrorReport {
    pub mirrored: Vec<String>,
    /// `edge_spacing_reject_count_` — candidates whose flip would break a LEF58 edge rule.
    pub edge_spacing_rejected: Vec<String>,
    pub candidates: usize,
}

/// One net's box, as `NetBox` holds it.
#[derive(Debug, Clone, Copy)]
struct NetBox {
    /// `(x_min, y_min, x_max, y_max)` of `getTermBBox`.
    bbox: (i32, i32, i32, i32),
    /// `Rect::dx() + dy()`, as odb computed them.
    hpwl: i64,
    ignore: bool,
}

fn term_box(db: &Db, net: &str) -> ((i32, i32, i32, i32), i64) {
    let b = (db.net_get_term_b_box_x_min(net), db.net_get_term_b_box_y_min(net),
             db.net_get_term_b_box_x_max(net), db.net_get_term_b_box_y_max(net));
    (b, db.net_get_term_b_box_dx(net) as i64 + db.net_get_term_b_box_dy(net) as i64)
}

/// `inst/pin` → `(inst, pin)`. ⚠️ On the LAST slash: instance names are hierarchical (`f0/_281_`).
fn split_iterm(it: &str) -> Option<(&str, &str)> {
    it.rsplit_once('/')
}

/// `orientMirrorY`.
pub fn orient_mirror_y(orient: &str) -> &'static str {
    match orient {
        "R0" => "MY",
        "MX" => "R180",
        "MY" => "R0",
        "R180" => "MX",
        "R90" => "MXR90",
        "MXR90" => "R90",
        "R270" => "MYR90",
        "MYR90" => "R270",
        _ => "R0",
    }
}

/// `findNetBoxes` — every net in block order, with `ignore` for supply, special and large nets.
fn find_net_boxes(db: &Db) -> (Vec<String>, HashMap<String, NetBox>) {
    let nets = db.block_get_nets();
    let mut boxes = HashMap::new();
    for n in &nets {
        let sig = db.net_get_sig_type(n);
        let ignore = sig == "POWER" || sig == "GROUND"
            || db.net_is_special(n)
            || db.net_get_i_terms(n).len() > MIRROR_MAX_ITERM_COUNT;
        let (bbox, hpwl) = term_box(db, n);
        boxes.insert(n.clone(), NetBox { bbox, hpwl, ignore });
    }
    (nets, boxes)
}

/// `findMirrorCandidates` — over the nets in HPWL order, every CORE, unfixed instance with a pin
/// whose average location lies ON the net box's boundary; first occurrence wins.
fn find_mirror_candidates(db: &Db, sorted: &[&String], boxes: &HashMap<String, NetBox>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for net in sorted {
        let b = &boxes[*net];
        if b.ignore {
            continue;
        }
        for it in db.net_get_i_terms(net) {
            let Some((inst, pin)) = split_iterm(&it) else { continue };
            if !db.inst_is_core(inst) || db.inst_is_fixed(inst) {
                continue;
            }
            let Some((x, y)) = db.iterm_avg_xy(inst, pin) else { continue };
            let (x0, y0, x1, y1) = b.bbox;
            if (x == x0 || x == x1 || y == y0 || y == y1) && seen.insert(inst.to_string()) {
                out.push(inst.to_string());
            }
        }
    }
    out
}

/// `OptimizeMirroring::hpwl(inst)` — the summed HPWL of the non-ignored nets on its pins, a net
/// counted once per pin on it.
fn inst_hpwl(db: &Db, inst: &str, boxes: &HashMap<String, NetBox>) -> i64 {
    db.inst_get_i_terms(inst).iter()
        .filter_map(|it| split_iterm(it).map(|(i, p)| db.iterm_get_net(i, p)))
        .filter(|n| !n.is_empty())
        .filter_map(|n| boxes.get(&n).filter(|b| !b.ignore).map(|b| b.hpwl))
        .sum()
}

fn inst_nets(db: &Db, inst: &str) -> Vec<String> {
    db.inst_get_i_terms(inst).iter()
        .filter_map(|it| split_iterm(it).map(|(i, p)| db.iterm_get_net(i, p)))
        .filter(|n| !n.is_empty())
        .collect()
}

/// `OptimizeMirroring::run`.
pub fn optimize_mirroring(db: &mut Db) -> Result<MirrorReport, String> {
    // `importDb` → `adjustNodesOrient` → `initGrid` → `setGridCells`: the network's cells, each
    // painted in network order, in its own orientation.
    let mut grid = Grid::build(db)?;
    let core = grid.core;
    let network = crate::network::network_insts(db);
    let index: HashMap<String, usize> =
        network.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();
    let masters: Vec<String> = network.iter().map(|n| db.inst_master(n)).collect();
    let mut orients: Vec<String> = network.iter().map(|n| db.inst_get_orient(n)).collect();
    let boxes_at: Vec<(i32, i32, i32, i32)> = network.iter().map(|n| {
        let (x, y) = db.inst_location(n);
        let m = db.inst_master(n);
        (x - core.0, y - core.1, db.master_get_width(&m) as i32, db.master_get_height(&m) as i32)
    }).collect();
    for (i, b) in boxes_at.iter().enumerate() {
        grid.paint_cell(b.0, b.1, b.2, b.3, Some(i as u32));
    }
    let edges = crate::edges::EdgeModel::build(db, &grid, masters.iter().map(String::as_str));

    // `findNetBoxes`, then the sort: HPWL descending, net id ascending — the block's net order
    // IS id order, so a stable sort on HPWL alone gives the tie-break.
    let (nets, mut boxes) = find_net_boxes(db);
    let mut sorted: Vec<&String> = nets.iter().collect();
    sorted.sort_by(|a, b| boxes[*b].hpwl.cmp(&boxes[*a].hpwl));

    let candidates = find_mirror_candidates(db, &sorted, &boxes);
    let mut report = MirrorReport { candidates: candidates.len(), ..Default::default() };

    // `mirrorCandidates`.
    for inst in &candidates {
        let Some(&i) = index.get(inst) else {
            return Err(format!("[ERROR DPL-0025] Instance {inst} is missing its dpl node."));
        };
        let orient = orients[i].clone();
        let orient_my = orient_mirror_y(&orient);
        // `isEdgeSpacingLegal(cell, orient_my)` at the cell's own grid position, against every
        // painted cell in ITS current orientation (earlier flips included).
        let b = boxes_at[i];
        let legal = edges.check(
            &grid, i, &masters[i], orient_my, b.0, b.1,
            &|px, py| grid.pixel(px as i64, py as i64).and_then(|p| p.cell).map(|c| c as usize),
            &|o| edges.edges_at(&masters[o], &orients[o], boxes_at[o].0, boxes_at[o].1),
        );
        if !legal {
            report.edge_spacing_rejected.push(inst.clone());
            continue;
        }
        let before = inst_hpwl(db, inst, &boxes);
        let touched = inst_nets(db, inst);
        let saved: Vec<(String, NetBox)> =
            touched.iter().filter_map(|n| boxes.get(n).map(|b| (n.clone(), *b))).collect();
        db.set_inst_location_orient(inst, orient_my).map_err(|e| e.to_string())?;
        for n in &touched {
            if let Some(b) = boxes.get_mut(n) {
                if !b.ignore {
                    let (bbox, hpwl) = term_box(db, n);
                    b.bbox = bbox;
                    b.hpwl = hpwl;
                }
            }
        }
        let after = inst_hpwl(db, inst, &boxes);
        if after > before {
            db.set_inst_location_orient(inst, &orient).map_err(|e| e.to_string())?;
            for (n, b) in saved {
                boxes.insert(n, b);
            }
        } else {
            orients[i] = orient_my.to_string();
            report.mirrored.push(inst.clone());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream `orientMirrorY`: an involution over the eight orients.
    #[test]
    fn mirror_y_is_upstreams_table_and_an_involution() {
        for (a, b) in [("R0", "MY"), ("MX", "R180"), ("R90", "MXR90"), ("R270", "MYR90")] {
            assert_eq!(orient_mirror_y(a), b);
            assert_eq!(orient_mirror_y(b), a);
        }
    }

    /// Hierarchical instance names keep their slashes; the pin is after the LAST one.
    #[test]
    fn an_iterm_splits_on_its_last_slash() {
        assert_eq!(split_iterm("f0/_281_/A"), Some(("f0/_281_", "A")));
    }
}
