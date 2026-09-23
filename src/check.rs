// SPDX-License-Identifier: Apache-2.0
//! `check_placement` — is every cell legally placed?
//!
//! Transcribed from OpenROAD `src/dpl/src/CheckPlacement.cpp::Opendp::checkPlacement`.
//!
//! ## What is here, and what is deliberately not
//!
//! Upstream runs **nine** check families, and all nine are evaluated here. A family that runs
//! under a restriction says so in `limitations`, and one that cannot run on a design (no grid) is
//! named in `not_checked`, so a clean report cannot be mistaken for a complete one.
//!
//! | family | upstream | here |
//! | --- | --- | --- |
//! | site alignment | `left % siteWidth`, bottom on a row Y | ✅ |
//! | placed | `dbInst::isPlaced()` | ✅ |
//! | overlap | `checkOverlap` via grid pixels | ✅ **rule transcribed, acceleration differs** |
//! | in rows | `checkInRows` — pixel validity, site, multi-row power | ✅ |
//! | region placement | `checkRegionPlacement` + `checkRegionOverlap` | ✅ [`crate::regions`] |
//! | padding · edge spacing · blocked layers | `PlacementDRC`, in upstream's loop order | ✅ |
//! | one-site gaps | `checkOneSiteGaps`, a separate pass after the loop | ✅ |
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
/// ✅ **Empty as of 2026-09-22**: `edge_spacing` was the last, wired once the bindings exposed
/// each master's LEF58 edges. Kept, so a family that cannot run is named here rather than dropped.
pub const NOT_CHECKED: &[&str] = &[];

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
///
/// ⛔ **And a multi-row cell must be power-compatible with the row it starts in** — the function's
/// last line, `!isMultiRow || checkRowPowerCompatible(cell, ylo)`. `power_ok(ylo, rows)` answers
/// it; the caller passes `None` for a master that is not multi-row. Missed at first: measured on
/// `multi_height_power_align`'s INPUT (`dpl-check-gate.py --raw`), a double-height cell on an FS
/// row — VDD pin on a VSS rail — which the reference fails in-rows and we passed.
fn check_in_rows(g: &crate::grid::Grid, x: i32, y: i32, w: i32, h: i32, site: &str,
                 power_ok: Option<&dyn Fn(i64, i64) -> bool>) -> bool {
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
    power_ok.map_or(true, |ok| ok(ylo, yhi - ylo))
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
    check_placement_opts(db, !db.has_one_site_master(), &crate::negotiate::Padding::default())
}

/// `check_placement` with the one-site-gap decision supplied.
///
/// ℹ️ Exposed for tests, which need to exercise both settings on one design. Production callers
/// want [`check_placement`], which derives it the way upstream does.
pub fn check_placement_opts(db: &Db, disallow_one_site_gaps: bool, padding: &crate::negotiate::Padding) -> Report {
    let ys = row_ys(db);
    let sw = site_width(db);
    let x0 = core_x_min(db);
    // `createNetwork`'s cells, in its ORDER — name-sorted, less the non-core-auto-placeable
    // masters and the fixed instances outside the rows' outer shell. ⛔ The order decides which
    // of two overlapping cells is painted first, and the filter decides what is checked at all:
    // `obstruction1`'s four corner endcaps sit outside the core and upstream never sees them.
    let insts: Vec<String> = crate::network::network_insts(db);

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

    // `setUpPlacementGroups` + `groupAssignCellRegions`: each grouped cell's region, from its
    // placed location (the checker's `initialLocation` IS where the cell sits).
    let regions = match grid {
        Ok(ref g) => {
            let loc = |i: &str| -> Option<crate::regions::Box4> {
                let b = boxes.iter().find(|(n, _, _)| n == i)?.1;
                Some((b.0 as i32 - g.core.0, b.1 as i32 - g.core.1, b.2 as i32, b.3 as i32))
            };
            crate::regions::Regions::build(db, g.core, &insts, &loc)
        }
        Err(_) => crate::regions::Regions::default(),
    };

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
    // `checkEdgeSpacing`'s table and each master's edges (`makeCellEdgeSpacingTable`, `addMaster`).
    // ⛔ An EMPTY table skips the check, as `hasCellEdgeSpacingTable` does.
    let edge_table =
        crate::drc::EdgeSpacingTable::build(&db.tech_cell_edge_spacing().unwrap_or_default());
    let mut master_edges: std::collections::HashMap<String, (crate::drc::EdgeBox, Vec<(usize, crate::drc::EdgeBox)>)> =
        std::collections::HashMap::new();
    if let (false, Ok(g)) = (edge_table.is_empty(), grid.as_ref()) {
        for (n, _, _) in &boxes {
            let m = db.inst_master(n);
            if master_edges.contains_key(&m) {
                continue;
            }
            let b = db.master_placement_boundary(&m).unwrap_or_default();
            let bbox = if b.len() == 4 { (b[0], b[1], b[2], b[3]) } else { (0, 0, 0, 0) };
            let rows = g.grid_height(db.master_get_height(&m) as i32,
                                     db.row_pattern(&db.master_get_site(&m)).map_or(0, |p| p.len()));
            let spacer = crate::drc::is_core_spacer(&db.master_get_type(&m).unwrap_or_default());
            let lef = db.master_edge_types(&m).unwrap_or_default();
            let edges = crate::drc::master_edges(bbox, &lef, rows, &edge_table, spacer);
            master_edges.insert(m, (bbox, edges));
        }
    }
    // A cell's edges placed where it sits, in its own orientation — `adjustNodesOrient` gives every
    // node its instance's orient, and a neighbour's edges are taken at `getLeft/getBottom`.
    let orients: Vec<String> = boxes.iter().map(|(n, _, _)| db.inst_get_orient(n)).collect();
    let edges_at = |o: usize, x: i32, y: i32| -> Vec<(usize, crate::drc::EdgeBox)> {
        let Some((bbox, edges)) = master_edges.get(&db.inst_master(&boxes[o].0)) else {
            return Vec::new();
        };
        edges.iter()
            .map(|&(t, e)| (t, crate::drc::transform_edge_rect(e, *bbox, &orients[o], x, y)))
            .collect()
    };

    // `checkRowPowerCompatible`'s model, as the legalizer builds it.
    let power = grid.as_ref().ok().map(|g| crate::negotiate::PowerModel::build(db, g, &levels));
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

    // `set_placement_padding` lives in the session, not the database: the caller passes it
    // (`--padding-global/-master/-inst`), and each cell's `padLeft`/`padRight` is
    // [`Padding::of`](crate::negotiate::Padding::of) — instance, then master, then global, CORE
    // types only. With none given the rule still fires on the CLASS matrix alone.
    if *padding == crate::negotiate::Padding::default() {
        out.limitations.push(
            "padding: no set_placement_padding given, so only class-pair conflicts are caught".into());
    }
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
                let master = db.inst_master(name);
                let site = db.master_get_site(&master);
                // `Grid::isMultiHeight`.
                let multi_row = g.is_multi_height(
                    bx.3 as i32, db.row_pattern(&site).map_or(0, |p| p.len()));
                let power_ok = |row: i64, rows: i64| {
                    power.as_ref().map_or(true, |p| p.compatible(&master, row as i32, rows as i32))
                };
                if !check_in_rows(g, bx.0 as i32 - g.core.0, bx.1 as i32 - g.core.1,
                                  bx.2 as i32, bx.3 as i32, &site,
                                  if multi_row { Some(&power_ok) } else { None }) {
                    out.failures.push(Failure { family: "in_rows".into(), cell: name.clone(),
                                                with: None });
                }
                // `checkRegionPlacement`, std cells only, after in-rows.
                if !regions.check(name, bx.0 as i32 - g.core.0, bx.1 as i32 - g.core.1,
                                  bx.2 as i32, bx.3 as i32, g.site_width, &g.row_y, g.core.3) {
                    out.failures.push(Failure { family: "region_placement".into(),
                                                cell: name.clone(), with: None });
                }
            }
        }

        if !db.inst_is_placed(name) {
            out.failures.push(Failure { family: "placed".into(), cell: name.clone(), with: None });
        }

        // `checkOverlap` without a grid (none could be built): the cells visited BEFORE this one.
        if drc_grid.is_none() {
            if let Some((other, _, _)) = boxes[..idx].iter().enumerate().find(|(j, (_, ob, oblk))| {
                !misaligned.contains(j) && !(*blk && *oblk) && rects_overlap(*bx, *ob)
            }).map(|(_, t)| t) {
                out.failures.push(Failure { family: "overlap".into(), cell: name.clone(),
                                            with: Some(other.clone()) });
            }
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

            // `checkOverlap`: over the cell's squares, a square ALREADY holding another cell that
            // genuinely overlaps this one (`overlap`: strict, BLOCK/BLOCK exempt) makes this cell
            // fail; an empty square is claimed. ⛔ So only the LATER cell of a pair fails — the
            // first painted its squares before the second arrived. A rectangle sweep over all
            // cells reported BOTH: `check2`, 2 failures where upstream reports 1.
            let mut overlap_with: Option<usize> = None;
            for gx in gx0..gx1 {
                for gy in gy0..gy1 {
                    if let Some(o) = g.pixel(gx, gy).and_then(|p| p.cell) {
                        let o = o as usize;
                        if o != idx && !(*blk && boxes[o].2) && rects_overlap(*bx, boxes[o].1) {
                            overlap_with = Some(o);
                        }
                    }
                }
            }
            if let Some(o) = overlap_with {
                out.failures.push(Failure { family: "overlap".into(), cell: name.clone(),
                                            with: Some(boxes[o].0.clone()) });
            }
            g.paint_cell(cx, cy, bx.2 as i32, bx.3 as i32, Some(me));

            let cls = classes[idx];
            let master = db.inst_master(name);
            let (left_pad, right_pad) =
                padding.of(name, &master, &db.master_get_type(&master).unwrap_or_default());
            let at = |px: i32, py: i32| -> Option<(Option<(crate::drc::Class, bool)>,
                                                   Option<(crate::drc::Class, bool)>)> {
                let p = g.pixel(px as i64, py as i64)?;
                Some((p.cell.map(|c| (classes[c as usize], c == me)),
                      p.padding_reserved_by.map(|c| (classes[c as usize], c == me))))
            };
            if !crate::drc::check_padding(gx0 as i32, gx1 as i32, gy0 as i32, gy1 as i32,
                                          left_pad, right_pad, cls, &at) {
                out.failures.push(Failure { family: "padding".into(), cell: name.clone(),
                                            with: None });
            }

            g.paint_cell_padding(gx0, gy0, gx1 - gx0, bx.3 as i32, left_pad as i64, right_pad as i64, me);

            // `checkEdgeSpacing(cell)` — at `gridX(cell)`, `gridRoundY(cell)`, the cell's orient;
            // against the cells painted BEFORE it. ⛔ After `paintCellPadding`, as upstream.
            if !edge_table.is_empty() {
                let (gx, gy) = (cx.div_euclid(g.site_width), g.grid_round_y(cy));
                let (xr, yr) = (gx * g.site_width, g.row_y.get(gy).copied().unwrap_or(0));
                let mine = edges_at(idx, xr, yr);
                let sw = g.site_width;
                let row_y = &g.row_y;
                let ok = crate::drc::check_edge_spacing(
                    &edge_table, idx, &mine,
                    &|v| v.div_euclid(sw),
                    &|v| (f64::from(v) / f64::from(sw)).ceil() as i32,
                    &|v| row_y.iter().position(|&ry| ry >= v).unwrap_or(row_y.len()) as i32,
                    &|px, py| g.pixel(px as i64, py as i64).and_then(|p| p.cell).map(|c| c as usize),
                    &|o| edges_at(o, boxes[o].1.0 as i32 - g.core.0, boxes[o].1.1 as i32 - g.core.1),
                );
                if !ok {
                    out.failures.push(Failure { family: "edge_spacing".into(), cell: name.clone(),
                                                with: None });
                }
            }

            let used = used_layers_of(db, name);
            if !crate::drc::check_blocked_layers(
                gx0 as i32, gx1 as i32, gy0 as i32, gy1 as i32, used,
                &|px, py| g.pixel(px as i64, py as i64).map(|p| p.blocked_layers))
            {
                out.failures.push(Failure { family: "blocked_layers".into(), cell: name.clone(),
                                            with: None });
            }
        }
    }
    // `checkOneSiteGaps` — ⛔ a SECOND loop, after every cell is painted, as upstream runs it ("it
    // needs to be done after the overlap check"): each cell sees its neighbours on BOTH sides.
    // Checked inline, a cell saw only the cells visited before it and missed a gap to a later one.
    // Every network cell is visited, a site-misaligned one included (upstream's loop does not skip
    // it), against the grid as the first loop left it.
    if disallow_one_site_gaps {
        if let Some(g) = drc_grid.as_ref() {
            for (name, bx, _) in &boxes {
                let (cx, cy) = (bx.0 as i32 - g.core.0, bx.1 as i32 - g.core.1);
                let (gx0, gy0, gx1, gy1) = g.covering(cx, cy, bx.2 as i32, bx.3 as i32);
                let at = |px: i64, py: i64| g.pixel(px, py).map(|p| (p.is_valid, p.cell.is_some()));
                if check_one_site_gaps(gx0, gy0, gx1, gy1, &at) {
                    out.failures.push(Failure { family: "one_site_gap".into(), cell: name.clone(),
                                                with: None });
                }
            }
        }
    }
    out.failures.sort();
    out
}

/// `Opendp::checkOneSiteGaps` (`CheckPlacement.cpp`) — NOT `PlacementDRC::checkOneSiteGap`: the
/// checker has its own. Along the cell's left and right edges (`visitCellBoundaryPixels`, WEST then
/// EAST per row), where the square just outside is a VALID site holding no cell, the square two out
/// is read and `gap_cell` is ASSIGNED from it — so a later edge that finds no cell there clears a
/// gap an earlier one found. `at(x, y)` is `(is_valid, holds a cell)`, `None` off the grid.
pub fn check_one_site_gaps(
    gx0: i64, gy0: i64, gx1: i64, gy1: i64, at: &dyn Fn(i64, i64) -> Option<(bool, bool)>,
) -> bool {
    let mut gap = false;
    for y in gy0..gy1 {
        for (x, step) in [(gx0, -1i64), (gx1 - 1, 1)] {
            if at(x, y).is_none() {
                continue; // the boundary square itself is off the grid: not visited
            }
            let (valid, occupied) = at(x + step, y).unwrap_or((false, false));
            if valid && !occupied {
                if let Some((_, cell)) = at(x + 2 * step, y) {
                    gap = cell;
                }
            }
        }
    }
    gap
}

#[cfg(test)]
mod tests {
    /// Upstream `checkInRows` ends `!isMultiRow || checkRowPowerCompatible(cell, ylo)`: a
    /// multi-row cell on valid squares still fails in-rows when its start row's rails do not
    /// match, and the test is asked at the FIRST row with the number of rows spanned.
    #[test]
    fn a_multi_row_cell_must_be_power_compatible_with_its_start_row() {
        let g = crate::grid::Grid::uniform_for_test((0, 0, 100, 20), 10, vec![0, 10, 20]);
        assert!(super::check_in_rows(&g, 0, 0, 20, 20, "S", None), "not multi-row: no power test");
        let asked = std::cell::Cell::new((-1, -1));
        let reject = |row: i64, rows: i64| { asked.set((row, rows)); false };
        assert!(!super::check_in_rows(&g, 0, 0, 20, 20, "S", Some(&reject)));
        assert_eq!(asked.get(), (0, 2), "asked at the start row, over both rows");
        assert!(super::check_in_rows(&g, 0, 0, 20, 20, "S", Some(&|_, _| true)));
    }

    #[test]
    fn a_gap_is_seen_on_both_sides_and_a_later_edge_can_clear_it() {
        use super::check_one_site_gaps;
        // Row of 10 squares, all valid; the cell spans 4..6.
        let grid = |cells: &'static [i64]| move |x: i64, y: i64| {
            (y == 0 && (0..10).contains(&x)).then(|| (true, cells.contains(&x)))
        };
        // A neighbour two squares RIGHT (x=7): gap — seen whichever cell was visited first.
        assert!(check_one_site_gaps(4, 0, 6, 1, &grid(&[4, 5, 7])));
        // A neighbour two squares LEFT (x=2) and the cell ABUTTED on the right (x=6): gap — the
        // east edge finds an occupied square next door and assigns nothing.
        assert!(check_one_site_gaps(4, 0, 6, 1, &grid(&[4, 5, 2, 6])));
        // ⛔ The same left neighbour with OPEN space to the right: NO failure. The east edge runs
        // last, finds a valid empty square at x=6, reads the empty x=7 and ASSIGNS no-cell over the
        // gap the west edge found — upstream's `gap_cell = gap_pixel->cell`, transcribed.
        assert!(!check_one_site_gaps(4, 0, 6, 1, &grid(&[4, 5, 2])));
        // Abutting on the left (x=3 occupied): not a gap.
        assert!(!check_one_site_gaps(4, 0, 6, 1, &grid(&[4, 5, 3, 2])));
    }

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
