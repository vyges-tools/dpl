// SPDX-License-Identifier: Apache-2.0
//! `detailed_placement`'s opening lines: the areas and utilization (DPL-6 … DPL-9), the
//! displacement caps (DPL-5) and which legalizer runs (DPL-1101 / DPL-1102); and its closing
//! ones, `report_legalization_stats` (or DPL-501 under `-quiet`).

use vyges_opendb::Db;

use crate::negotiate::{is_core_auto_placeable, status_is_fixed};

/// One instance as the report reads it.
#[derive(Debug, Clone)]
pub struct Inst {
    pub master_type: String,
    pub status: String,
    /// The master's width and height (unrotated, as the network node takes them).
    pub size: (i64, i64),
    /// The placed box.
    pub bbox: (i64, i64, i64, i64),
}

/// The lines, in order. Rules:
/// - the movable area is the network's CELL nodes that are not fixed: core-auto-placeable
///   instances whose status is not fixed and whose master is not a block (a placed macro is made
///   fixed), each its master's width × height;
/// - the fixed area is every fixed-status instance's box CLIPPED to the core, any master;
/// - utilization is (movable + fixed) / core, in percent, 0 for an empty core;
/// - a displacement cap of 0 on either axis means the defaults, 500 sites and 100 rows.
pub fn header(core: (i64, i64, i64, i64), insts: &[Inst], dbu: i64, max_displacement: (i32, i32), diamond: bool) -> Vec<(u16, String)> {
    let core_area = (core.2 - core.0) * (core.3 - core.1);
    let mut inst_area = 0i64;
    let mut fixed_area = 0i64;
    for i in insts {
        let block = crate::drc::canonical_master_type(&i.master_type).starts_with("BLOCK");
        if is_core_auto_placeable(&i.master_type) && !status_is_fixed(&i.status) && !block {
            inst_area += i.size.0 * i.size.1;
        }
        if status_is_fixed(&i.status) {
            let (xl, yl, xh, yh) = (i.bbox.0.max(core.0), i.bbox.1.max(core.1), i.bbox.2.min(core.2), i.bbox.3.min(core.3));
            if xl <= xh && yl <= yh {
                fixed_area += (xh - xl) * (yh - yl);
            }
        }
    }
    let utilization = if core_area > 0 { (inst_area + fixed_area) as f64 / core_area as f64 * 100.0 } else { 0.0 };
    let um2 = |a: i64| a as f64 / (dbu * dbu) as f64;
    let (dx, dy) = if max_displacement.0 == 0 || max_displacement.1 == 0 { (500, 100) } else { max_displacement };
    vec![
        (6, format!("Core area: {:.2} um^2", um2(core_area))),
        (7, format!("Movable instances area: {:.2} um^2", um2(inst_area))),
        (8, format!("Fixed instances area within core: {:.2} um^2", um2(fixed_area))),
        (9, format!("Utilization: {utilization:.1}%")),
        (5, format!("Diamond search max displacement: +/- {dx} sites horizontally, +/- {dy} rows vertically.")),
        if diamond { (1101, "Legalizing using diamond search.".into()) } else { (1102, "Legalizing using negotiation legalizer.".into()) },
    ]
}

/// `odb::WireLengthEvaluator::hpwl` (DBU): over every block net, its terminal box's dx + dy; a
/// supply (POWER/GROUND) or special net counts 0, and so does an inverted (empty) box.
pub fn odb_hpwl(db: &Db) -> i64 {
    db.block_get_nets().iter().map(|n| {
        let sig = db.net_get_sig_type(n);
        if sig == "POWER" || sig == "GROUND" || db.net_is_special(n) {
            return 0;
        }
        if db.net_get_term_b_box_x_min(n) > db.net_get_term_b_box_x_max(n)
            || db.net_get_term_b_box_y_min(n) > db.net_get_term_b_box_y_max(n) {
            return 0;
        }
        db.net_get_term_b_box_dx(n) as i64 + db.net_get_term_b_box_dy(n) as i64
    }).sum()
}

/// `have_fillers_`: some instance the network imports is a filler (`isFiller`) — DPL-37.
pub fn has_fillers(db: &Db, network_insts: &[String]) -> bool {
    network_insts.iter().any(|n| crate::fillers::is_filler(&db.master_get_type(&db.inst_master(n)).unwrap_or_default(),
                                                           &db.inst_get_placement_status(n)))
}

/// `findDisplacementStats` (DBU): over the network's cells, `disp` = |x − x0| + |y − y0| from the
/// imported location. The average is INTEGER division of the sum by the cell count, as upstream's
/// `int64_t` member takes it; 0 for no cells.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Displacement {
    pub sum: i64,
    pub max: i64,
    pub avg: i64,
}

pub fn displacement(disps: impl IntoIterator<Item = i64>, num_cells: usize) -> Displacement {
    let mut d = Displacement::default();
    for x in disps {
        d.sum += x;
        d.max = d.max.max(x);
    }
    if num_cells != 0 {
        d.avg = d.sum / num_cells as i64;
    }
    d
}

/// `reportLegalizationStats` — the Tcl command prints it after `detailed_placement` unless
/// `-quiet`. The iteration lines are debug-only upstream and not here. Delta HPWL is a ROUNDED
/// integer percent (half away from zero), 0 when the HPWL before was 0. A blank line ends it.
pub fn legalization_stats(diamond: bool, total_moves: i64, d: Displacement, hpwl_before: i64, hpwl_after: i64, dbu: i64) -> Vec<String> {
    let um = |v: i64| v as f64 / dbu as f64;
    let delta = if hpwl_before == 0 { 0 } else {
        ((hpwl_after as f64 - hpwl_before as f64) / hpwl_before as f64 * 100.0).round() as i64
    };
    vec![
        "Detailed Placement Analysis".into(),
        "---------------------------------".into(),
        format!("legalizer            {}", if diamond { "diamond search" } else { "negotiation" }),
        format!("total moves          {total_moves:10}"),
        format!("total displacement   {:10.1} u", um(d.sum)),
        format!("average displacement {:10.1} u", um(d.avg)),
        format!("max displacement     {:10.1} u", um(d.max)),
        format!("original HPWL        {:10.1} u", um(hpwl_before)),
        format!("legalized HPWL       {:10.1} u", um(hpwl_after)),
        format!("delta HPWL           {delta:10} %"),
        String::new(),
    ]
}

/// DPL-501, `reportLegalizationSummary` — the one line that stands in for the analysis under
/// `-quiet` (the antenna repair's legalization). The delta is a signed one-decimal percent.
pub fn legalization_summary(total_moves: i64, d: Displacement, hpwl_before: i64, hpwl_after: i64, dbu: i64) -> String {
    let um = |v: i64| v as f64 / dbu as f64;
    let delta = if hpwl_before == 0 { 0.0 } else { (hpwl_after as f64 - hpwl_before as f64) / hpwl_before as f64 * 100.0 };
    format!("Legalized with {total_moves} moves: displacement total {:.1} u, max {:.1} u; HPWL {:.1} -> {:.1} u ({delta:+.1} %).",
            um(d.sum), um(d.max), um(hpwl_before), um(hpwl_after))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(t: &str, s: &str, size: (i64, i64), bbox: (i64, i64, i64, i64)) -> Inst {
        Inst { master_type: t.into(), status: s.into(), size, bbox }
    }

    /// Rules (detailedPlacement): a placed macro is fixed for the movable area but not counted as
    /// fixed area (that reads the database status); a fixed pad's box counts only inside the core;
    /// an unplaced core cell is movable; a 0 cap on one axis brings back both defaults.
    #[test]
    fn header_areas_and_caps() {
        let insts = [
            inst("CORE", "PLACED", (2000, 1000), (0, 0, 2000, 1000)),
            inst("CORE", "NONE", (1000, 1000), (0, 0, 1000, 1000)),
            inst("BLOCK", "PLACED", (4000, 4000), (0, 0, 4000, 4000)),
            inst("PAD", "FIRM", (2000, 2000), (9000, 9000, 11000, 11000)),
            inst("CORE", "FIRM", (1000, 1000), (5000, 5000, 6000, 6000)),
        ];
        let got = header((0, 0, 10000, 10000), &insts, 1000, (0, 7), false);
        let text: Vec<String> = got.iter().map(|(i, t)| format!("{i} {t}")).collect();
        assert_eq!(text, [
            "6 Core area: 100.00 um^2",
            "7 Movable instances area: 3.00 um^2",
            "8 Fixed instances area within core: 2.00 um^2",
            "9 Utilization: 5.0%",
            "5 Diamond search max displacement: +/- 500 sites horizontally, +/- 100 rows vertically.",
            "1102 Legalizing using negotiation legalizer.",
        ]);
        assert_eq!(header((0, 0, 10, 10), &[], 1000, (3, 4), true)[4..], [(5, "Diamond search max displacement: +/- 3 sites horizontally, +/- 4 rows vertically.".to_string()), (1101, "Legalizing using diamond search.".to_string())]);
    }

    /// Rule (reportLegalizationStats, upstream test simple03): field widths, the integer average,
    /// the rounded integer delta, the trailing blank line.
    #[test]
    fn stats_block_layout() {
        let d = displacement([49400, 0], 2);
        assert_eq!(d, Displacement { sum: 49400, max: 49400, avg: 24700 });
        let got = legalization_stats(false, 0, displacement([49400], 1), 125500, 26800, 1000);
        assert_eq!(got, [
            "Detailed Placement Analysis",
            "---------------------------------",
            "legalizer            negotiation",
            "total moves                   0",
            "total displacement         49.4 u",
            "average displacement       49.4 u",
            "max displacement           49.4 u",
            "original HPWL             125.5 u",
            "legalized HPWL             26.8 u",
            "delta HPWL                  -79 %",
            "",
        ]);
        assert_eq!(legalization_stats(true, 3, d, 0, 5, 1000)[9], "delta HPWL                    0 %");
        assert_eq!(displacement([], 0), Displacement::default());
    }

    /// Rule (DPL-501): `{:+.1f}` percent — a sign on BOTH sides, `-0.0` for a tiny drop (as the
    /// reference's antenna repair printed `689048.3 -> 689046.9 u (-0.0 %)`).
    #[test]
    fn quiet_summary_sign() {
        let d = Displacement { sum: 1400, max: 900, avg: 0 };
        assert_eq!(legalization_summary(0, d, 689_048_300, 689_046_900, 1000),
                   "Legalized with 0 moves: displacement total 1.4 u, max 0.9 u; HPWL 689048.3 -> 689046.9 u (-0.0 %).");
        assert_eq!(legalization_summary(221, d, 689_010_100, 689_048_300, 1000),
                   "Legalized with 221 moves: displacement total 1.4 u, max 0.9 u; HPWL 689010.1 -> 689048.3 u (+0.0 %).");
        assert_eq!(legalization_summary(1, d, 0, 5, 1000).rsplit_once('(').unwrap().1, "+0.0 %).");
    }
}
