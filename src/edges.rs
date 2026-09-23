// SPDX-License-Identifier: Apache-2.0
//! The LEF58 cell-edge model of one design, bound to its grid — what `PlacementDRC::checkEdgeSpacing`
//! reads, shared by `check_placement` and `optimize_mirroring`.
//!
//! ⛔ **One model, two callers.** The legalizer keeps its own (it reads `NegGrid` occupancy); the
//! checker and the mirroring pass both read the DPL grid's `pixel->cell`, and a second hand-copy of
//! this for mirroring is exactly how a legality test went stale twice in the legalizer.

use std::collections::HashMap;

use vyges_opendb::Db;

use crate::drc::{EdgeBox, EdgeSpacingTable};
use crate::grid::Grid;

pub struct EdgeModel {
    pub table: EdgeSpacingTable,
    /// Per master: its placement boundary and its typed edges (`makeCellEdgeSpacingTable` +
    /// `addMaster`).
    masters: HashMap<String, (EdgeBox, Vec<(usize, EdgeBox)>)>,
}

impl EdgeModel {
    /// ⛔ An EMPTY table skips the check outright, as `hasCellEdgeSpacingTable` does, and then no
    /// master is read.
    pub fn build<'a>(db: &Db, grid: &Grid, masters: impl Iterator<Item = &'a str>) -> EdgeModel {
        let table = EdgeSpacingTable::build(&db.tech_cell_edge_spacing().unwrap_or_default());
        let mut out = EdgeModel { table, masters: HashMap::new() };
        if out.table.is_empty() {
            return out;
        }
        for m in masters {
            if out.masters.contains_key(m) {
                continue;
            }
            let b = db.master_placement_boundary(m).unwrap_or_default();
            let bbox = if b.len() == 4 { (b[0], b[1], b[2], b[3]) } else { (0, 0, 0, 0) };
            let rows = grid.grid_height(db.master_get_height(m) as i32,
                                        db.row_pattern(&db.master_get_site(m)).map_or(0, |p| p.len()));
            let spacer = crate::drc::is_core_spacer(&db.master_get_type(m).unwrap_or_default());
            let lef = db.master_edge_types(m).unwrap_or_default();
            let edges = crate::drc::master_edges(bbox, &lef, rows, &out.table, spacer);
            out.masters.insert(m.to_string(), (bbox, edges));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// A master's edges placed with its boundary's lower-left at `(x, y)` (core-relative) in
    /// `orient` — `cell_edges::transformEdgeRect`.
    pub fn edges_at(&self, master: &str, orient: &str, x: i32, y: i32) -> Vec<(usize, EdgeBox)> {
        let Some((bbox, edges)) = self.masters.get(master) else { return Vec::new() };
        edges.iter()
            .map(|&(t, e)| (t, crate::drc::transform_edge_rect(e, *bbox, orient, x, y)))
            .collect()
    }

    /// `PlacementDRC::checkEdgeSpacing(cell, gridX(cell), gridRoundY(cell), orient)` for the cell
    /// `me` whose box's lower-left is `(cx, cy)`, core-relative. `occupant(x, y)` is `pixel->cell`;
    /// `neighbour(o)` another cell's edges where it sits (`getLeft/getBottom/getOrient`).
    #[allow(clippy::too_many_arguments)]
    pub fn check(&self, grid: &Grid, me: usize, master: &str, orient: &str, cx: i32, cy: i32,
                 occupant: &dyn Fn(i32, i32) -> Option<usize>,
                 neighbour: &dyn Fn(usize) -> Vec<(usize, EdgeBox)>) -> bool {
        if self.is_empty() {
            return true;
        }
        let sw = grid.site_width;
        let (gx, gy) = (cx.div_euclid(sw), grid.grid_round_y(cy));
        let (xr, yr) = (gx * sw, grid.row_y.get(gy).copied().unwrap_or(0));
        let mine = self.edges_at(master, orient, xr, yr);
        let row_y = &grid.row_y;
        crate::drc::check_edge_spacing(
            &self.table, me, &mine,
            &|v| v.div_euclid(sw),
            &|v| (f64::from(v) / f64::from(sw)).ceil() as i32,
            &|v| row_y.iter().position(|&ry| ry >= v).unwrap_or(row_y.len()) as i32,
            occupant,
            neighbour,
        )
    }
}
