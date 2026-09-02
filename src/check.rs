// SPDX-License-Identifier: Apache-2.0
//! `check_placement` — is every cell legally placed?
//!
//! Transcribed from OpenROAD `src/dpl/src/CheckPlacement.cpp::Opendp::checkPlacement`.
//!
//! ## What is here, and what is deliberately not
//!
//! Upstream runs **nine** check families. This implements the three that need only geometry and
//! the row grid; the rest need `Grid`/`Pixel`, `Padding` or `PlacementDRC`, and are declared
//! rather than silently omitted so a clean report cannot be mistaken for a complete one.
//!
//! | family | upstream | here |
//! | --- | --- | --- |
//! | site alignment | `left % siteWidth`, bottom on a row Y | ✅ |
//! | placed | `dbInst::isPlaced()` | ✅ |
//! | overlap | `checkOverlap` via grid pixels | ✅ **rule transcribed, acceleration differs** |
//! | in rows | `checkInRows` — pixel validity + site orientation | ⬜ needs `Grid` |
//! | region placement | `checkRegionPlacement` | ⬜ needs regions |
//! | padding · edge spacing · blocked layers | `PlacementDRC` | ⬜ |
//! | one-site gaps | separate pass after overlap | ⬜ needs `Grid` |
//!
//! ⚠️ **The overlap ACCELERATION differs and the answer set does not.** Upstream finds candidate
//! neighbours by walking the pixels a cell covers; this compares rectangles directly. The
//! predicate is identical — `ll1.x < ur2.x && ur1.x > ll2.x && ll1.y < ur2.y && ur1.y > ll2.y`,
//! with BLOCK/BLOCK pairs exempt — so the SET of overlapping cells matches. ⛔ **Which partner is
//! reported can differ**, because upstream reports whichever cell already owns the pixel and that
//! depends on visit order; a caller comparing partner names rather than the failing set will see
//! differences that are not disagreements.
//!
//! 🔑 **Upstream's `continue` on a site-alignment failure skips every later check for that cell**,
//! including placed and overlap. Transcribed: a misaligned cell appears once, not three times.
use serde::Serialize;
use vyges_opendb::Db;

/// One cell that failed a check, and which one.
#[derive(Serialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Failure {
    pub family: String,
    pub cell: String,
    /// For `overlap`, the cell it overlaps. ⚠️ See the acceleration note above before comparing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with: Option<String>,
}

/// The verdict of a legality check.
#[derive(Serialize, Debug, Default)]
pub struct Report {
    pub failures: Vec<Failure>,
    pub cells_checked: usize,
    /// Families this run did NOT evaluate — see the module table.
    pub not_checked: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The families this engine cannot yet evaluate. Named so `not_checked` is never empty by
/// accident: a checker that quietly stops checking is the `vacuous` class.
pub const NOT_CHECKED: &[&str] = &[
    "region_placement", "padding", "edge_spacing", "blocked_layers", "one_site_gap",
];

/// The rectangle a cell occupies, in DBU: `(x, y, w, h)`.
fn cell_box(db: &Db, inst: &str) -> (i64, i64, i64, i64) {
    let (x, y) = db.inst_location(inst);
    let m = db.inst_master(inst);
    (x as i64, y as i64, db.master_get_width(&m) as i64, db.master_get_height(&m) as i64)
}

/// `Opendp::overlap` — plain rectangle intersection, with BLOCK/BLOCK pairs exempt.
///
/// ⚠️ **Strict inequalities on all four sides**, so cells merely ABUTTING do not overlap. That is
/// the whole point in a legalizer: abutment is the legal steady state.
fn rects_overlap(a: (i64, i64, i64, i64), b: (i64, i64, i64, i64)) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by
}

/// Every distinct row Y coordinate — upstream's `grid_->getRowCoordinates()`.
fn row_ys(db: &Db) -> std::collections::BTreeSet<i64> {
    (0..db.num_rows().unwrap_or(0))
        .filter_map(|i| db.nth_row(i).ok().flatten())
        .map(|(bbox, _, _)| bbox[1] as i64)
        .collect()
}

/// The core's left edge — the X the site grid is measured FROM.
///
/// ⛔ **Site alignment is CORE-RELATIVE, and reading it as absolute reports every cell as
/// misaligned.** Upstream compares `cell->getLeft() % siteWidth`, and `getLeft()` is relative to
/// `core_.xMin()` — `updateDbInstLocations` adds it back (`core_.xMin() + cell->getLeft()`).
/// Measured on `aes.defok`: rows start at x = 28000, site width 380, and an instance at 88040 has
/// `88040 % 380 = 260` but `(88040 - 28000) % 380 = 0`. The reference calls that design clean; the
/// absolute reading called all 21,340 cells misaligned.
fn core_x_min(db: &Db) -> i64 {
    (0..db.num_rows().unwrap_or(0))
        .filter_map(|i| db.nth_row(i).ok().flatten())
        .map(|(bbox, _, _)| bbox[0] as i64)
        .min()
        .unwrap_or(0)
}

/// The narrowest site width in the design, in DBU.
fn site_width(db: &Db) -> i64 {
    (0..db.num_rows().unwrap_or(0))
        .filter_map(|i| db.nth_row(i).ok().flatten())
        .map(|(_, site, _)| db.site_get_width(&site) as i64)
        .filter(|w| *w > 0)
        .min()
        .unwrap_or(0)
}

/// `Opendp::checkInRows` — every square the cell covers must be a valid row square, and the
/// cell's site must be one the first row it sits in actually offers.
///
/// ⚠️ **The site check applies to the FIRST ROW ONLY** (`first_row = (y == grid_rect.ylo)`), not
/// to every row a multi-height cell spans. Applying it to all of them rejects legal multi-row
/// cells whose upper rows offer a different site.
fn check_in_rows(g: &crate::grid::Grid, x: i32, y: i32, w: i32, h: i32, site: &str) -> bool {
    let (xlo, ylo, xhi, yhi) = g.covering(x, y, w, h);
    if ylo < 0 {
        return false;
    }
    for gy in ylo..yhi {
        for gx in xlo..xhi {
            match g.pixel(gx, gy) {
                None => return false,               // outside the core
                Some(p) if !p.is_valid => return false,
                _ => {}
            }
            if gy == ylo && !g.site_valid_at(gx, gy, site) {
                return false;
            }
        }
    }
    true
}

/// Run the legality check.
pub fn check_placement(db: &Db) -> Report {
    let ys = row_ys(db);
    let sw = site_width(db);
    let x0 = core_x_min(db);
    let insts: Vec<String> = (0..db.num_insts()).map(|i| db.nth_inst_name(i)).collect();

    // Cells to consider: upstream skips anything that is not `Node::CELL`, and applies the
    // site-alignment and in-rows checks only to STD CELLS. A block (macro) is neither.
    let is_block = |i: &str| {
        db.master_get_type(&db.inst_master(i)).map(|t| t.contains("BLOCK")).unwrap_or(false)
    };
    let boxes: Vec<(String, (i64, i64, i64, i64), bool)> =
        insts.iter().map(|i| (i.clone(), cell_box(db, i), is_block(i))).collect();

    // ⚠️ A design the grid cannot be built for is not a clean design: `in_rows` goes back into
    // `not_checked` and says why, rather than being silently skipped.
    let grid = crate::grid::Grid::build(db);
    let mut not_checked: Vec<String> = NOT_CHECKED.iter().map(|s| s.to_string()).collect();
    if let Err(ref why) = grid {
        not_checked.push(format!("in_rows (grid unavailable: {why})"));
    }
    let mut out = Report { not_checked, ..Default::default() };

    // ⛔ **A site-align failure removes the cell from the overlap comparison ENTIRELY**, and that
    // is a side effect of upstream's `continue`, not a separate rule. `checkOverlap` is what paints
    // a cell into its pixels (`pixel->cell = &cell`); a cell that `continue`d never runs it, so it
    // is never there for a later cell to collide with.
    //
    // ⚠️ Measured on `cell_on_block1`: without this, we reported `block1` overlapping `u1` while
    // the reference reported site-alignment alone. Our rectangle sweep sees every cell whether or
    // not it was skipped; upstream's pixel map only sees the ones that got painted.
    let misaligned: std::collections::HashSet<usize> = boxes
        .iter()
        .enumerate()
        .filter(|(_, (_, bx, blk))| !*blk && sw > 0 && ((bx.0 - x0) % sw != 0 || !ys.contains(&bx.1)))
        .map(|(i, _)| i)
        .collect();

    for (idx, (name, bx, blk)) in boxes.iter().enumerate() {
        out.cells_checked += 1;

        // ⛔ Site alignment first, and it `continue`s — a misaligned cell is reported once.
        if !*blk && sw > 0 {
            // ⚠️ `bx.0 - x0`, not `bx.0`. Y needs no such adjustment here because both sides are
            // read absolutely — the row Ys come from the same rows the cells sit in.
            if misaligned.contains(&idx) {
                out.failures.push(Failure { family: "site_align".into(), cell: name.clone(),
                                            with: None });
                continue;
            }
        }

        if let Ok(ref g) = grid {
            if !*blk {
                let site = db.master_get_site(&db.inst_master(name));
                if !check_in_rows(g, bx.0 as i32 - g.core.0, bx.1 as i32 - g.core.1,
                                  bx.2 as i32, bx.3 as i32, &site) {
                    out.failures.push(Failure { family: "in_rows".into(), cell: name.clone(),
                                                with: None });
                }
            }
        }

        if !db.inst_is_placed(name) {
            out.failures.push(Failure { family: "placed".into(), cell: name.clone(), with: None });
        }

        // BLOCK/BLOCK overlaps are allowed, so a pair is only a failure when at least one is a cell.
        if let Some((other, _, _)) = boxes.iter().enumerate().find(|(j, (_, ob, oblk))| {
            *j != idx && !misaligned.contains(j) && !(*blk && *oblk) && rects_overlap(*bx, *ob)
        }).map(|(_, t)| t) {
            out.failures.push(Failure { family: "overlap".into(), cell: name.clone(),
                                        with: Some(other.clone()) });
        }
    }
    out.failures.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::rects_overlap;

    #[test]
    fn abutting_cells_do_not_overlap() {
        // 🔑 The strict inequalities are the rule, not an off-by-one: a legalizer's whole output
        // is cells packed edge to edge, and calling that an overlap would fail every legal design.
        let a = (0, 0, 10, 10);
        assert!(!rects_overlap(a, (10, 0, 10, 10)), "side by side");
        assert!(!rects_overlap(a, (0, 10, 10, 10)), "stacked");
        assert!(rects_overlap(a, (9, 0, 10, 10)), "one unit of real overlap");
        assert!(rects_overlap(a, (0, 9, 10, 10)));
    }

    #[test]
    fn containment_and_identity_overlap() {
        assert!(rects_overlap((0, 0, 10, 10), (2, 2, 3, 3)), "fully contained");
        assert!(rects_overlap((0, 0, 10, 10), (0, 0, 10, 10)), "identical");
    }

    #[test]
    fn disjoint_cells_do_not_overlap() {
        assert!(!rects_overlap((0, 0, 10, 10), (100, 100, 5, 5)));
    }
}

/// Diagnostic: the grid facts the site-alignment check depends on.
///
/// ⚠️ Exists because the first run of that check reported EVERY cell misaligned on a design the
/// reference calls clean. A wrong site width or an empty row set produces exactly that, and both
/// look identical from the failure list.
fn status_counts(db: &Db) -> std::collections::BTreeMap<String, usize> {
    let mut c: std::collections::BTreeMap<String, usize> = Default::default();
    for i in 0..db.num_insts() {
        *c.entry(db.inst_get_placement_status(&db.nth_inst_name(i))).or_default() += 1;
    }
    c
}

pub fn grid_facts(db: &Db) -> serde_json::Value {
    let ys: Vec<i64> = row_ys(db).into_iter().take(5).collect();
    let sample: Vec<serde_json::Value> = (0..db.num_insts().min(3))
        .map(|i| {
            let n = db.nth_inst_name(i);
            let (x, y) = db.inst_location(&n);
            serde_json::json!({"inst": n, "x": x, "y": y, "master": db.inst_master(&n)})
        })
        .collect();
    serde_json::json!({
        "core_area": [db.block_get_core_area_x_min(), db.block_get_core_area_y_min(),
                      db.block_get_core_area_x_max(), db.block_get_core_area_y_max()],
        "num_rows": db.num_rows().unwrap_or(0),
        "placement_statuses": status_counts(db),
        "site_width": site_width(db),
        "first_row_ys": ys,
        "row_x_origins": (0..db.num_rows().unwrap_or(0))
            .filter_map(|i| db.nth_row(i).ok().flatten())
            .map(|(b, _, _)| b[0] as i64)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter().take(4).collect::<Vec<_>>(),
        "row_count_distinct_y": row_ys(db).len(),
        "sample_insts": sample,
    })
}
