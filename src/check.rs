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
    /// Families that WERE evaluated but under a stated restriction.
    ///
    /// ⛔ **Distinct from `not_checked`, and both matter.** A family here did run and its verdict
    /// counts; the entry says what it could not see. A reader who treats "checked" as "checked
    /// completely" is the failure this field exists to prevent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The families `check-placement` does not evaluate. Named so `not_checked` is never empty by
/// accident: a checker that quietly stops checking is the `vacuous` class.
///
/// ⚠️ **Three families left this list on 2026-09-02** — padding, blocked layers and one-site gaps
/// — when `check_placement` gained the pixel grid they need. What each of them can and cannot see
/// is stated in `limitations` rather than implied by their absence here.
///
/// ⛔ **"Implemented but not wired" is not "checked".** `edge_spacing` stays because its rule has
/// no data to run on, not because it is unwritten.
pub const NOT_CHECKED: &[&str] = &[
    // Needs regions, which nothing in this crate models yet.
    "region_placement",
    // ⛔ Needs each master's LEF58 CELLEDGESPACINGTABLE edge list, which the bindings do not
    // expose. The RULE is built and tested in `crate::drc`; what is missing is the data.
    "edge_spacing",
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
    // ⛔ **`disallow_one_site_gaps_` is DERIVED FROM THE TECHNOLOGY, not asked for.** `importDb`
    // sets it to `!odb::hasOneSiteMaster(db_)`: if no placeable master is exactly one site wide,
    // a one-site gap can never be filled, so leaving one is a violation. Where such a master
    // exists the gap is fillable and the check is off.
    //
    // ⚠️ Taking it as a caller-supplied option would be wrong in both directions: it would let a
    // caller demand the check on a technology where upstream does not apply it, and skip it where
    // upstream does. It is a property of the library, not a preference.
    check_placement_opts(db, !db.has_one_site_master())
}

/// `check_placement` with the one-site-gap decision supplied.
///
/// ℹ️ Exposed for tests, which need to exercise both settings on one design. Production callers
/// want [`check_placement`], which derives it the way upstream does.
pub fn check_placement_opts(db: &Db, disallow_one_site_gaps: bool) -> Report {
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

    // ── the pixel state the DRC rules read ────────────────────────────────────────────────────
    //
    // ⛔ **A SECOND grid, painted in the loop below rather than up front.** Upstream's
    // `checkPlacement` paints each cell as it visits it (`checkOverlap` sets `pixel->cell`, then
    // `paintCellPadding` sets `padding_reserved_by`), so a cell is checked against the cells
    // BEFORE it in the loop and not against those after. Painting everything first would make
    // every cell see every other and change which of a pair is reported.
    let mut drc_grid = crate::grid::Grid::build(db).ok();
    let classes: Vec<crate::drc::Class> = boxes
        .iter()
        .map(|(n, _, _)| crate::drc::classify(
            &db.master_get_type(&db.inst_master(n)).unwrap_or_default()))
        .collect();

    // `Node::getUsedLayers` per master, from its pin geometry.
    let levels = {
        let layers = db.layers_with_direction().unwrap_or_default();
        let types: Vec<(String, String)> = layers
            .iter()
            .map(|(n, _)| (n.clone(), db.layer_get_type(n).unwrap_or_default()))
            .collect();
        crate::drc::routing_levels(&types)
    };
    let mut used_layers_cache: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut used_layers_of = |db: &Db, inst: &str| -> u32 {
        let master = db.inst_master(inst);
        if let Some(&m) = used_layers_cache.get(&master) {
            return m;
        }
        let pin_levels = db
            .master_pin_boxes(&master)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(ln, ..)| levels.get(&db.layer_name_by_number(ln)).copied());
        let m = crate::drc::used_layers(pin_levels);
        used_layers_cache.insert(master, m);
        m
    };

    // ⛔ **Zero padding, because this engine has no `set_placement_padding` model.** The rule
    // still fires on the CLASS matrix — a CORE cell sharing a square with a WELLTAP is a padding
    // conflict at zero padding too — but it cannot catch a violation that only a nonzero pad
    // would create. Stated in `limitations`, not left for a reader to infer.
    let (left_pad, right_pad) = (0i64, 0i64);
    out.limitations.push(
        "padding: evaluated with zero padding (no set_placement_padding model), so only \
         class-pair conflicts are caught".into());
    if disallow_one_site_gaps {
        out.limitations.push(
            "one_site_gap: PlacementDRC's reading, where a square off the grid counts as \
             OCCUPIED — Place.cpp reads the same test the other way".into());
    } else {
        out.not_checked.push(
            "one_site_gap (the technology HAS a one-site master, so upstream does not apply it)"
                .into());
    }
    match drc_grid.as_ref().map(|g| g.blocked_layer_status()) {
        Some((true, n)) => out.limitations.push(format!(
            "blocked_layers: {n} grid square(s) carry a blocked level, from vertical M2/M3 \
             special wires only")),
        _ => out.not_checked.push(
            "blocked_layers (no vertical M2/M3 special wire in this design: the mask is empty, \
             so a pass would be vacuous)".into()),
    }

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

        // ── PlacementDRC, in upstream's order ────────────────────────────────────────────────
        //
        // 🔑 `checkPadding` → `paintCellPadding` → `checkEdgeSpacing` → `checkBlockedLayers`,
        // and the PAINT sits in the middle: a cell's own padding reservation must not exist when
        // its own padding is checked, and must exist when the next cell's is.
        if let Some(g) = drc_grid.as_mut() {
            let (cx, cy) = (bx.0 as i32 - g.core.0, bx.1 as i32 - g.core.1);
            let (gx0, gy0, gx1, gy1) = g.covering(cx, cy, bx.2 as i32, bx.3 as i32);
            let me = idx as u32;

            // `checkOverlap` is what paints `pixel->cell` upstream; the rectangle sweep above
            // gives the same answer set but paints nothing, so the paint happens here.
            g.paint_cell(cx, cy, bx.2 as i32, bx.3 as i32, Some(me));

            let cls = classes[idx];
            let at = |px: i32, py: i32| -> Option<(Option<(crate::drc::Class, bool)>,
                                                   Option<(crate::drc::Class, bool)>)> {
                let p = g.pixel(px as i64, py as i64)?;
                Some((p.cell.map(|c| (classes[c as usize], c == me)),
                      p.padding_reserved_by.map(|c| (classes[c as usize], c == me))))
            };
            if !crate::drc::check_padding(gx0 as i32, gx1 as i32, gy0 as i32, gy1 as i32,
                                          left_pad as i32, right_pad as i32, cls, &at) {
                out.failures.push(Failure { family: "padding".into(), cell: name.clone(),
                                            with: None });
            }

            let used = used_layers_of(db, name);
            if !crate::drc::check_blocked_layers(
                gx0 as i32, gx1 as i32, gy0 as i32, gy1 as i32, used,
                &|px, py| g.pixel(px as i64, py as i64).map(|p| p.blocked_layers))
            {
                out.failures.push(Failure { family: "blocked_layers".into(), cell: name.clone(),
                                            with: None });
            }

            if disallow_one_site_gaps
                && !crate::drc::check_one_site_gap(
                    true, gx0 as i32, gx1 as i32, gy0 as i32, gy1 as i32,
                    crate::drc::EdgeReading::OffGridIsOccupied,
                    &|px, py| g.pixel(px as i64, py as i64).map(|p| p.cell.is_some()))
            {
                out.failures.push(Failure { family: "one_site_gap".into(), cell: name.clone(),
                                            with: None });
            }

            g.paint_cell_padding(gx0, gy0, gx1 - gx0, bx.3 as i32, left_pad, right_pad, me);
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
            let m = db.inst_master(&n);
            // 🔑 The SITE, because every row-legality test keys on it: a master whose site no
            // row carries can never be seated, and nothing in a position reveals that.
            serde_json::json!({"inst": n, "x": x, "y": y, "master": m.clone(),
                               "site": db.master_get_site(&m),
                               "height": db.master_get_height(&m)})
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
        // The distinct sites the ROWS offer, which is what a master's site must match.
        "row_sites": (0..db.num_rows().unwrap_or(0))
            .filter_map(|i| db.nth_row(i).ok().flatten())
            .map(|(_, site, _)| site)
            .collect::<std::collections::BTreeSet<_>>(),
        // 🔑 The GRID's own dimensions, not the database's. They are what every placement
        // decision is clamped against, and a diagnostic that reports only the DEF cannot show a
        // grid that came out the wrong size.
        "grid": match crate::grid::Grid::build(db) {
            Ok(g) => serde_json::json!({
                "row_count": g.row_count, "row_site_count": g.row_site_count,
                "site_width": g.site_width, "core": [g.core.0, g.core.1, g.core.2, g.core.3],
                "row_y": g.row_y, "valid_sites": g.valid_sites(),
            }),
            Err(e) => serde_json::json!({"error": e}),
        },
        "sample_insts": sample,
    })
}
