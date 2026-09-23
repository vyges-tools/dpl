// SPDX-License-Identifier: Apache-2.0
//! `filler_placement` — fill every empty site run with filler masters (`Opendp::fillerPlacement`).
//!
//! In the reference's order, each step its own function named after the reference's:
//! [`get_masters_arg`] (the Tcl glob) → [`filter_filler_masters`] → [`split_by_implant`] (each group
//! sorted widest first) → the grid ([`set_grid_cells`]) → per row [`place_row_fillers`], which asks
//! [`gap_fillers`] for each run → the instances written, then counted.
//!
//! 🔑 **Filler insertion is ADDITIVE.** It creates new instances in the row gaps — placed, oriented,
//! connected to no signal net — and never moves, flips or reconnects one that was already there.
//!
//! ⚠️ The reference skips `importDb` when the call follows `detailed_placement` in one session.
//! This engine is a fresh process on the database the legalizer wrote, so it reads the grid and
//! every cell from it; the positions are the ones the legalizer stored, and filling reads only
//! footprints and validity, which do not depend on the session.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use vyges_opendb::Db;

use crate::grid::Grid;

/// One filler master as the fill reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillerMaster {
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// The routing-independent IMPLANT layer of its first implant obstruction, by layer number.
    pub implant: Option<i64>,
}

/// One filler the fill decided on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Filler {
    pub name: String,
    pub master: String,
    pub x: i32,
    pub y: i32,
    pub orient: String,
}

/// `MasterByImplant` — a `std::map<dbTechLayer*, dbMasterSeq>`, in KEY order.
///
/// ⚠️ Upstream orders by layer POINTER: `nullptr` (no implant) first, then — because tech layers are
/// allocated in one table in creation order — by layer number. Golden-blind for the multi-implant
/// order: the filler cases have one implant group.
pub type ByImplant = Vec<(Option<i64>, Vec<FillerMaster>)>;

/// What stops the fill, named after the reference's error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillError {
    /// DPL-0039: no pattern matched any master.
    NoMasters(String),
    /// DPL-0050: a neighbour's implant has no fillers.
    NoFillersForImplant(i64),
    /// DPL-0002: a gap no combination of fillers fits.
    CannotFill(String),
    /// The database refused a write, or the grid could not be built.
    Db(String),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillError::NoMasters(p) => write!(f, "DPL-0039: \"{p}\" did not match any masters."),
            FillError::NoFillersForImplant(l) => write!(f, "DPL-0050: No fillers found for implant layer {l}."),
            FillError::CannotFill(m) => write!(f, "DPL-0002: {m}"),
            FillError::Db(m) => write!(f, "{m}"),
        }
    }
}

/// The result: the fillers in creation order, and the patterns that matched nothing (DPL-0028).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FillReport {
    pub fillers: Vec<Filler>,
    pub unmatched_patterns: Vec<String>,
}

/// `filler_placement` — the reference's call sequence and nothing else.
pub fn filler_placement(db: &mut Db, patterns: &[String], prefix: &str) -> Result<FillReport, FillError> {
    let (masters, unmatched) = get_masters_arg(db, patterns)?;
    let masters = filter_filler_masters(db, masters);
    let by_implant = split_by_implant(masters);
    let grid = Grid::build(db).map_err(FillError::Db)?;
    let cells = set_grid_cells(db, &grid).map_err(FillError::Db)?;
    let site_height = |site: &str| db.site_get_height(site);
    let mut names = UniqueNames::new(db.inst_names());
    let mut fillers = Vec::new();
    for row in 0..grid.row_count {
        fillers.extend(place_row_fillers(row, prefix, &by_implant, &grid, &cells, &site_height, &mut names)?);
    }
    write_fillers(db, &fillers)?;
    Ok(FillReport { fillers, unmatched_patterns: unmatched })
}

/// `get_masters_arg` (Tcl): per pattern, every master of every library IN LIBRARY ORDER whose name
/// Tcl `string match`es it — a master two patterns match is listed twice. A pattern matching
/// nothing warns (DPL-0028); all of them matching nothing is DPL-0039.
pub fn get_masters_arg(db: &Db, patterns: &[String]) -> Result<(Vec<FillerMaster>, Vec<String>), FillError> {
    let all: Vec<String> = (0..db.num_masters().map_err(|e| FillError::Db(e.to_string()))?)
        .map(|i| db.nth_master_name(i).unwrap_or_default())
        .collect();
    let (mut out, mut unmatched) = (Vec::new(), Vec::new());
    for p in patterns {
        let hits: Vec<&String> = all.iter().filter(|m| tcl_string_match(p, m)).collect();
        if hits.is_empty() {
            unmatched.push(p.clone());
        }
        for m in hits {
            out.push(FillerMaster {
                name: m.clone(),
                width: db.master_get_width(m) as i32,
                height: db.master_get_height(m) as i32,
                implant: master_implant(db, m),
            });
        }
    }
    if !patterns.is_empty() && out.is_empty() {
        return Err(FillError::NoMasters(patterns.join(" ")));
    }
    Ok((out, unmatched))
}

/// `getImplant`: the layer of the master's FIRST obstruction on an IMPLANT-type layer.
fn master_implant(db: &Db, master: &str) -> Option<i64> {
    db.master_obstruction_boxes(master).ok()?.into_iter().map(|b| b.0).find(|&n| {
        db.layer_get_type(&db.layer_name_by_number(n)).is_ok_and(|t| t == "IMPLANT")
    })
}

/// `filterFillerMasters`: PAD and BLOCK masters cannot fill.
pub fn filter_filler_masters(db: &Db, masters: Vec<FillerMaster>) -> Vec<FillerMaster> {
    masters.into_iter().filter(|m| !db.master_is_pad(&m.name) && !db.master_is_block(&m.name)).collect()
}

/// `splitByImplant`, then each group sorted WIDEST first.
///
/// ⚠️ Upstream's sort is `std::ranges::sort` — not stable — so two masters of EQUAL width land in
/// an implementation-defined order; here they keep their pattern order. Golden-blind: the filler
/// libraries have one master per width.
pub fn split_by_implant(masters: Vec<FillerMaster>) -> ByImplant {
    let mut map: BTreeMap<Option<i64>, Vec<FillerMaster>> = BTreeMap::new();
    for m in masters {
        map.entry(m.implant).or_default().push(m);
    }
    map.into_iter()
        .map(|(k, mut v)| {
            v.sort_by(|a, b| b.width.cmp(&a.width));
            (k, v)
        })
        .collect()
}

/// A square's holder: the instance, and its master's implant (`getImplant`, read when the run
/// beside it picks its fillers).
pub type Holder = (String, Option<i64>);

/// `setGridCells`: which instance holds each square — `pixel->cell`, LAST writer wins — over every
/// instance's footprint WITHOUT padding (`visitCellPixels(cell, false, …)`): its OVERLAP-layer
/// obstructions when the master has any, its placed box otherwise. `[row][site]`.
pub fn set_grid_cells(db: &Db, grid: &Grid) -> Result<Vec<Vec<Option<Holder>>>, String> {
    let mut cells: Vec<Vec<Option<Holder>>> = vec![vec![None; grid.row_site_count]; grid.row_count];
    let (cx, cy) = (grid.core.0, grid.core.1);
    let paint = |cells: &mut Vec<Vec<Option<Holder>>>, (xlo, ylo, xhi, yhi): (i64, i64, i64, i64), h: &Holder| {
        for gy in ylo.max(0)..yhi.min(grid.row_count as i64) {
            for gx in xlo.max(0)..xhi.min(grid.row_site_count as i64) {
                cells[gy as usize][gx as usize] = Some(h.clone());
            }
        }
    };
    for inst in db.inst_names() {
        let master = db.inst_get_master(&inst);
        let holder: Holder = (inst.clone(), master_implant(db, &master));
        let overlap: Vec<(i32, i32, i32, i32)> = db
            .master_obstruction_boxes(&master)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|b| db.layer_get_type(&db.layer_name_by_number(b.0)).is_ok_and(|t| t == "OVERLAP"))
            .map(|b| (b.1, b.2, b.3, b.4))
            .collect();
        if overlap.is_empty() {
            let b = db.inst_bbox(&inst).map_err(|e| e.to_string())?;
            if b.len() < 4 {
                continue;
            }
            paint(&mut cells, grid.covering(b[0] - cx, b[1] - cy, b[2] - b[0], b[3] - b[1]), &holder);
        } else {
            // `inst->getTransform().apply(rect)`: the orientation about the ORIGIN, then the offset.
            let orient = db.inst_get_orient(&inst);
            let origin = (db.inst_get_origin_x(&inst), db.inst_get_origin_y(&inst));
            for r in overlap {
                let (x0, y0, x1, y1) = transform(&orient, origin, r);
                paint(&mut cells, grid.covering_rect(x0 - cx, y0 - cy, x1 - cx, y1 - cy), &holder);
            }
        }
    }
    Ok(cells)
}

/// `dbTransform::apply` on a rectangle.
fn transform(orient: &str, origin: (i32, i32), r: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let apply = |(x, y): (i32, i32)| -> (i32, i32) {
        let (x, y) = match orient {
            "R90" => (-y, x),
            "R180" => (-x, -y),
            "R270" => (y, -x),
            "MY" => (-x, y),
            "MYR90" => (-y, -x),
            "MX" => (x, -y),
            "MXR90" => (y, x),
            _ => (x, y),
        };
        (x + origin.0, y + origin.1)
    };
    let (a, b) = (apply((r.0, r.1)), apply((r.2, r.3)));
    (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

/// `dbInst::makeUniqueDbInst`'s naming: the name itself when free, else `name_1`, `name_2`, …
/// counted per base name.
pub struct UniqueNames {
    taken: BTreeSet<String>,
    ids: HashMap<String, i32>,
}

impl UniqueNames {
    pub fn new(existing: impl IntoIterator<Item = String>) -> Self {
        UniqueNames { taken: existing.into_iter().collect(), ids: HashMap::new() }
    }
    /// ⚠️ The first retry is the base name AGAIN (its counter starts at 0), then `_1`, `_2`, … — a
    /// wasted probe upstream, harmless, kept for the counter it advances.
    pub fn make(&mut self, name: &str) -> String {
        if self.taken.insert(name.to_string()) {
            return name.to_string();
        }
        loop {
            let id = self.ids.entry(name.to_string()).or_insert(0);
            let full = if *id > 0 { format!("{name}_{id}") } else { name.to_string() };
            *id += 1;
            if self.taken.insert(full.clone()) {
                return full;
            }
        }
    }
}

/// `placeRowFillers`: along one grid row, every run of squares that are valid and hold no cell,
/// filled from its left end.
///
/// The run's site and orientation are the SHORTEST site at its first square; its fillers come
/// from the implant of the cell to its LEFT — or, at column 0, of the cell to its right — else
/// from the first implant group.
///
/// ⚠️ The neighbour is read only when a CELL holds that square: a run starting after an invalid
/// square (a row end, a blockage) falls back to the first group.
#[allow(clippy::too_many_arguments)]
pub fn place_row_fillers(
    row: usize,
    prefix: &str,
    by_implant: &ByImplant,
    grid: &Grid,
    cells: &[Vec<Option<Holder>>],
    site_height: &dyn Fn(&str) -> i32,
    names: &mut UniqueNames,
) -> Result<Vec<Filler>, FillError> {
    let Some(first_key) = by_implant.first().map(|g| g.0) else {
        return Err(FillError::NoMasters(String::new()));
    };
    let mut out = Vec::new();
    let n = grid.row_site_count;
    let open = |x: usize| cells[row][x].is_none() && grid.pixel(x as i64, row as i64).is_some_and(|p| p.is_valid);
    let mut j = 0usize;
    while j < n {
        if !open(j) {
            j += 1;
            continue;
        }
        let Some((site, orient)) = grid.shortest_site(j, row, site_height) else {
            return Err(FillError::Db(format!("row {row} column {j}: a valid square no row site covers")));
        };
        let mut k = j;
        while k < n && open(k) {
            k += 1;
        }
        // `implant = getImplant(neighbour's master)`; `if (!implant) implant = begin()->first`.
        let neighbour = if j > 0 { cells[row][j - 1].as_ref() } else if k < n { cells[row][k].as_ref() } else { None };
        let implant_key = neighbour.and_then(|h| h.1).or(first_key);
        let group = match by_implant.iter().find(|(key, _)| *key == implant_key) {
            Some((_, g)) => g,
            None => return Err(FillError::NoFillersForImplant(implant_key.unwrap_or(-1))),
        };
        let gap = (k - j) as i32;
        let row_height = site_height(&site);
        let fillers = gap_fillers(group, gap, row_height, grid.site_width);
        if fillers.is_empty() {
            let um = |d: i32| f64::from(d) / 1000.0;
            let (x, y) = (grid.core.0 + j as i32 * grid.site_width, grid.core.1 + grid.row_y[row]);
            let left = grid_inst_name(cells, row, j as i64 - 1, n);
            let right = grid_inst_name(cells, row, k as i64 + 1, n);
            return Err(FillError::CannotFill(format!(
                "Could not fill gap of {gap} sites ({:.2}x{:.2} um) at ({:.2}, {:.2}) um between {left} and {right}",
                um(gap * grid.site_width), um(row_height), um(x), um(y)
            )));
        }
        let mut at = j;
        for &f in &fillers {
            let m = &group[f];
            out.push(Filler {
                name: names.make(&format!("{prefix}{row}_{at}")),
                master: m.name.clone(),
                x: grid.core.0 + at as i32 * grid.site_width,
                y: grid.core.1 + grid.row_y[row],
                orient: orient.clone(),
            });
            at += (m.width / grid.site_width) as usize;
        }
        j += gap as usize;
    }
    Ok(out)
}

/// `gridInstName`: the instance at a square for the DPL-0002 message — `core_left` left of the row,
/// `core_right` past it, `?` on an empty square.
///
/// ⚠️ Upstream tests `col > row_site_count` and then reads the pixel, so `col == row_site_count` —
/// one past the last square — dereferences a null pixel. Read here as `core_right`.
fn grid_inst_name(cells: &[Vec<Option<Holder>>], row: usize, col: i64, n: usize) -> String {
    if col < 0 {
        return "core_left".into();
    }
    if col as usize >= n {
        return "core_right".into();
    }
    cells[row][col as usize].as_ref().map_or_else(|| "?".into(), |h| h.0.clone())
}

/// `gapFillers`: masters of the row's height, widest first, each taken as many times as fits —
/// never leaving exactly ONE site unless a one-site filler exists (`have_filler1`, judged on the
/// LAST master of the group whatever its height). Indices into the group; empty when no
/// combination fills the gap exactly.
pub fn gap_fillers(group: &[FillerMaster], gap: i32, row_height: i32, site_width: i32) -> Vec<usize> {
    let mut fillers = Vec::new();
    let Some(smallest) = group.last() else { return fillers };
    let have_filler1 = smallest.width == site_width;
    let mut width = 0;
    for (i, m) in group.iter().enumerate() {
        if m.height != row_height {
            continue;
        }
        let fw = m.width / site_width;
        if fw <= 0 {
            continue; // upstream would loop forever on a master narrower than a site
        }
        while width + fw <= gap && (have_filler1 || width + fw != gap - 1) {
            fillers.push(i);
            width += fw;
            if width == gap {
                return fillers;
            }
        }
    }
    Vec::new()
}

/// The instances, in creation order: orientation, location, PLACED, source DIST — upstream's order.
fn write_fillers(db: &mut Db, fillers: &[Filler]) -> Result<(), FillError> {
    let e = |e: vyges_opendb::Error| FillError::Db(e.to_string());
    for f in fillers {
        db.create_physical_inst(&f.master, &f.name).map_err(e)?;
        db.set_inst_orient(&f.name, &f.orient).map_err(e)?;
        db.set_inst_location(&f.name, f.x, f.y).map_err(e)?;
        db.inst_set_placement_status(&f.name, "PLACED").map_err(e)?;
        db.inst_set_source_type(&f.name, "DIST").map_err(e)?;
    }
    Ok(())
}

/// Tcl `string match`: `*`, `?`, `[chars]` / `[a-z]`, and `\x` for a literal `x`.
pub fn tcl_string_match(pattern: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|i| go(&p[1..], &s[i..])),
            Some('?') => !s.is_empty() && go(&p[1..], &s[1..]),
            Some('[') => {
                let Some(end) = p.iter().position(|&c| c == ']') else { return false };
                let Some(&c) = s.first() else { return false };
                let set = &p[1..end];
                let mut hit = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        let (a, b) = (set[i].min(set[i + 2]), set[i].max(set[i + 2]));
                        hit |= c >= a && c <= b;
                        i += 3;
                    } else {
                        hit |= c == set[i];
                        i += 1;
                    }
                }
                hit && go(&p[end + 1..], &s[1..])
            }
            Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && go(&p[2..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    go(&p, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, sites: i32, height: i32) -> FillerMaster {
        FillerMaster { name: name.into(), width: sites * 10, height, implant: None }
    }

    #[test]
    fn a_gap_is_filled_widest_first() {
        let g = [m("X8", 8, 100), m("X4", 4, 100), m("X2", 2, 100), m("X1", 1, 100)];
        let names = |v: Vec<usize>| v.into_iter().map(|i| g[i].name.clone()).collect::<Vec<_>>();
        assert_eq!(names(gap_fillers(&g, 15, 100, 10)), ["X8", "X4", "X2", "X1"]);
        assert_eq!(names(gap_fillers(&g, 16, 100, 10)), ["X8", "X8"]);
        // Only masters of the ROW's height count.
        assert!(gap_fillers(&g, 3, 200, 10).is_empty());
    }

    #[test]
    fn without_a_one_site_filler_a_gap_is_never_left_one_short() {
        // ⛔ `have_filler1` is judged on the group's LAST master. Without a one-site filler a step
        // that would leave exactly one site is refused: gap 5 from {X4, X2} is not X4 (one short)
        // — it is not fillable at all, since X2 X2 leaves one too.
        let g = [m("X4", 4, 100), m("X2", 2, 100)];
        assert!(gap_fillers(&g, 5, 100, 10).is_empty());
        assert_eq!(gap_fillers(&g, 6, 100, 10), vec![0, 1]);
        // With one, the same gap fills.
        let g1 = [m("X4", 4, 100), m("X2", 2, 100), m("X1", 1, 100)];
        assert_eq!(gap_fillers(&g1, 5, 100, 10), vec![0, 2]);
    }

    #[test]
    fn masters_match_as_tcl_string_match() {
        assert!(tcl_string_match("FILL*", "FILLCELL_X1"));
        assert!(!tcl_string_match("FILL*", "fillcell"), "case-sensitive");
        assert!(tcl_string_match("FILLCELL_X?", "FILLCELL_X8"));
        assert!(!tcl_string_match("FILLCELL_X?", "FILLCELL_X16"));
        assert!(tcl_string_match("X[12]", "X2") && !tcl_string_match("X[12]", "X3"));
        assert!(tcl_string_match("X[1-3]", "X3"));
        assert!(tcl_string_match("A\\*", "A*") && !tcl_string_match("A\\*", "AB"));
    }

    #[test]
    fn a_taken_name_gets_the_next_numbered_one() {
        // `makeUniqueDbInst`: the name when free, else `_1`, `_2`, … per base name.
        let mut n = UniqueNames::new(["FILLER_0_0".to_string()]);
        assert_eq!(n.make("FILLER_0_4"), "FILLER_0_4");
        assert_eq!(n.make("FILLER_0_0"), "FILLER_0_0_1");
        assert_eq!(n.make("FILLER_0_0"), "FILLER_0_0_2");
    }
}
