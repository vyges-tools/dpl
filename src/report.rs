// SPDX-License-Identifier: Apache-2.0
//! `detailed_placement`'s opening lines: the areas and utilization (DPL-6 … DPL-9), the
//! displacement caps (DPL-5) and which legalizer runs (DPL-1101 / DPL-1102).

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
}
