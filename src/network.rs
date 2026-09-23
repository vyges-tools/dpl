// SPDX-License-Identifier: Apache-2.0
//! `Opendp::createNetwork`'s instance list — which instances become cells, and in what ORDER.
//!
//! In upstream's order: every instance, stable-sorted by NAME; a master that is not
//! core-auto-placeable is skipped; a FIXED instance whose box does not touch the rows' outer shell
//! ([`OuterShell`]) is skipped. Both the checker and the legalizer read this network, so both
//! see the same instances in the same order.
//!
//! ⛔ **The ORDER is behaviour.** `checkPlacement` paints each cell as it visits it, so which of two
//! overlapping cells is reported depends on which came first — by name, not by database order.

use vyges_opendb::Db;

/// `buildRowOuterShell` — the union of every non-PAD row's box, with its INTERIOR holes filled:
/// a void (a part of the rows' bounding box no row covers) that does not reach that box's edge is
/// a cutout surrounded by rows — a macro's — and counts as inside.
///
/// ⚠️ Upstream builds it with Boost.Polygon; here it is a compressed grid over the rows' own
/// edges, where every cell is wholly row or wholly void, and the voids are grouped by shared EDGES
/// (a void polygon is edge-connected; two touching only at a corner are two polygons).
#[derive(Debug, Clone, Default)]
pub struct OuterShell {
    xs: Vec<i32>,
    ys: Vec<i32>,
    /// `[j][i]`: the cell `xs[i]..xs[i+1] × ys[j]..ys[j+1]` is inside the shell.
    inside: Vec<Vec<bool>>,
}

impl OuterShell {
    pub fn build(rows: &[(i32, i32, i32, i32)]) -> OuterShell {
        if rows.is_empty() {
            return OuterShell::default();
        }
        let mut xs: Vec<i32> = rows.iter().flat_map(|r| [r.0, r.2]).collect();
        let mut ys: Vec<i32> = rows.iter().flat_map(|r| [r.1, r.3]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let (nx, ny) = (xs.len() - 1, ys.len() - 1);
        let mut inside = vec![vec![false; nx]; ny];
        for (j, row) in inside.iter_mut().enumerate() {
            for (i, cell) in row.iter_mut().enumerate() {
                let (cx, cy) = (xs[i], ys[j]);
                *cell = rows.iter().any(|r| r.0 <= cx && cx < r.2 && r.1 <= cy && cy < r.3);
            }
        }
        // Fill every edge-connected void that does not reach the bounding box's edge.
        let mut seen = vec![vec![false; nx]; ny];
        for j0 in 0..ny {
            for i0 in 0..nx {
                if inside[j0][i0] || seen[j0][i0] {
                    continue;
                }
                let (mut stack, mut comp, mut touches) = (vec![(i0, j0)], Vec::new(), false);
                seen[j0][i0] = true;
                while let Some((i, j)) = stack.pop() {
                    comp.push((i, j));
                    touches |= i == 0 || j == 0 || i == nx - 1 || j == ny - 1;
                    let mut push = |a: usize, b: usize| {
                        if !inside[b][a] && !seen[b][a] {
                            seen[b][a] = true;
                            stack.push((a, b));
                        }
                    };
                    if i > 0 { push(i - 1, j); }
                    if i + 1 < nx { push(i + 1, j); }
                    if j > 0 { push(i, j - 1); }
                    if j + 1 < ny { push(i, j + 1); }
                }
                if !touches {
                    for (i, j) in comp {
                        inside[j][i] = true;
                    }
                }
            }
        }
        OuterShell { xs, ys, inside }
    }

    /// `bboxIntersectsOuterShell` — `Rect::intersects`, which INCLUDES touching edges.
    pub fn intersects(&self, b: (i32, i32, i32, i32)) -> bool {
        for (j, row) in self.inside.iter().enumerate() {
            for (i, &inside) in row.iter().enumerate() {
                if inside && b.0 <= self.xs[i + 1] && self.xs[i] <= b.2 && b.1 <= self.ys[j + 1] && self.ys[j] <= b.3 {
                    return true;
                }
            }
        }
        false
    }
}

/// The rows the shell is built from: every row whose site is not PAD, as its placed box.
pub fn row_boxes(db: &Db) -> Vec<(i32, i32, i32, i32)> {
    (0..db.num_rows().unwrap_or(0))
        .filter_map(|i| {
            let (bbox, site, _) = db.nth_row(i).ok()??;
            (db.site_get_class(&site).unwrap_or_default() != "PAD" && bbox.len() == 4).then(|| (bbox[0], bbox[1], bbox[2], bbox[3]))
        })
        .collect()
}

/// `createNetwork`'s instances, in its order: name-sorted, less the non-core-auto-placeable and
/// the fixed ones outside the shell.
pub fn network_insts(db: &Db) -> Vec<String> {
    let shell = OuterShell::build(&row_boxes(db));
    let mut insts = db.inst_names();
    insts.sort(); // stable, by name — `std::ranges::stable_sort` on `getName()`
    insts
        .into_iter()
        .filter(|inst| {
            let mtype = db.master_get_type(&db.inst_master(inst)).unwrap_or_default();
            if !crate::negotiate::is_core_auto_placeable(&mtype) {
                return false;
            }
            if crate::negotiate::status_is_fixed(&db.inst_get_placement_status(inst)) {
                let b = db.inst_bbox(inst).unwrap_or_default();
                if b.len() == 4 && !shell.intersects((b[0], b[1], b[2], b[3])) {
                    return false;
                }
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cutout_surrounded_by_rows_is_inside_the_shell() {
        // Rows around a 20x20 hole at (10..30, 10..30): the hole does not reach the box's edge,
        // so it is filled; a macro inside it is kept.
        let rows = [(0, 0, 40, 10), (0, 30, 40, 40), (0, 10, 10, 30), (30, 10, 40, 30)];
        let s = OuterShell::build(&rows);
        assert!(s.intersects((15, 15, 25, 25)), "inside the filled hole");
        assert!(!s.intersects((50, 50, 60, 60)), "outside everything");
        assert!(s.intersects((40, 0, 50, 5)), "touching the shell's edge counts (inclusive)");
    }

    #[test]
    fn a_notch_open_to_the_edge_is_not_filled() {
        // A notch cut from the top edge reaches the bounding box: it stays outside.
        let rows = [(0, 0, 40, 10), (0, 10, 10, 40), (30, 10, 40, 40)];
        let s = OuterShell::build(&rows);
        assert!(!s.intersects((15, 20, 25, 30)), "in the open notch");
    }
}
