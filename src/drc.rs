// SPDX-License-Identifier: Apache-2.0
//! `PlacementDRC` — the placement design-rule checks.
//!
//! Reference: OpenROAD `src/dpl/src/PlacementDRC.cpp` (495 lines) at
//! `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, tree verified with `git rev-parse HEAD`.
//!
//! 🔑 **Why this engine matters out of proportion to its size.** It is the third independent
//! blocker for the negotiation legalizer: `isCellLegal` fails every cell without it,
//! `findBestLocation` loses its penalty term, and `updateDrcHistoryCosts` has nothing to bump.
//!
//! `checkDRC` composes four checks. ⚠️ **The fast path evaluates them cheapest-first and bails on
//! the first failure** — `blocked layers → one-site gap → padding → edge spacing` — while the debug
//! path runs all four so a report can show each. The ORDER differs between them, which is safe only
//! because each check is independent of the others.

/// The master classes the padding matrix is defined over.
///
/// ```text
/// CR = CORE, CORE_FEEDTHRU, CORE_TIEHIGH, CORE_TIELOW, CORE_ANTENNACELL
/// WT = CORE_WELLTAP
/// SP = CORE_SPACER, ENDCAP_*
/// BL = BLOCK, BLOCK_BLACKBOX, BLOCK_SOFT
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Core logic — the cells being placed.
    Cr,
    /// Well taps.
    Wt,
    /// Blocks / macros.
    Bl,
    /// Spacers and endcaps.
    Sp,
    /// Covers, rings and pads — ⚠️ *"completely ignored by the placer"*.
    Ignored,
}

/// Classify a master type string as the placer does.
///
/// ⚠️ Upstream uses a `switch` over the enum *"so if new types are added we get a compiler
/// warning"*. Matching on strings here cannot get that warning, so an unknown type maps to
/// [`Class::Ignored`] — the same answer upstream's fall-through gives.
pub fn classify(master_type: &str) -> Class {
    match master_type {
        "CORE" | "CORE_FEEDTHRU" | "CORE_TIEHIGH" | "CORE_TIELOW" | "CORE_ANTENNACELL" => Class::Cr,
        "CORE_WELLTAP" => Class::Wt,
        "BLOCK" | "BLOCK_BLACKBOX" | "BLOCK_SOFT" => Class::Bl,
        t if t == "CORE_SPACER" || t.starts_with("ENDCAP") => Class::Sp,
        _ => Class::Ignored,
    }
}

/// `isCrWtBlClass` — is this class in the padded-overlap matrix at all?
fn in_matrix(c: Class) -> bool {
    matches!(c, Class::Cr | Class::Wt | Class::Bl)
}

/// `allowOverlap` — BLOCK/BLOCK pairs may overlap outright.
pub fn allow_overlap(a: Class, b: Class) -> bool {
    a == Class::Bl && b == Class::Bl
}

/// `allowPaddingOverlap` — may these two classes overlap once padding is counted?
///
/// ⛔ **Well tap against well tap is the exception**: both are in the matrix, yet they are allowed
/// to overlap each other's padding. Every other in-matrix pair is not.
pub fn allow_padding_overlap(a: Class, b: Class) -> bool {
    !in_matrix(a) || !in_matrix(b) || (a == Class::Wt && b == Class::Wt)
}

/// `hasPaddingConflict` — do these two cells conflict?
///
/// ⚠️ **A cell never conflicts with itself**, which is what lets the scan walk its own footprint.
pub fn has_padding_conflict(a: Class, b: Class, same_cell: bool) -> bool {
    !same_cell && !allow_padding_overlap(a, b) && !allow_overlap(a, b)
}

/// The pair rule, as the comment table in `PlacementDRC.cpp` states it.
///
/// ```text
///     CR WT BL SP
/// CR   P  P  P  O
/// WT   P  O  P  O
/// BL   P  P  -  O
/// SP   O  O  O  O
///
/// P = no padded overlap   O = no overlap, padding ignored   - = overlap allowed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairRule {
    /// `P` — the padded footprints may not overlap.
    NoPaddedOverlap,
    /// `O` — the footprints may not overlap, but padding is ignored.
    NoOverlap,
    /// `-` — overlap is allowed outright.
    OverlapAllowed,
}

/// Which rule governs a pair of classes.
pub fn pair_rule(a: Class, b: Class) -> PairRule {
    if allow_overlap(a, b) {
        return PairRule::OverlapAllowed;
    }
    if allow_padding_overlap(a, b) {
        PairRule::NoOverlap
    } else {
        PairRule::NoPaddedOverlap
    }
}

/// `checkPadding` — walk the cell's footprint WIDENED by its padding and look for a conflict.
///
/// ⛔ **Padding widens the scan in X only** (`x - left_pad` .. `x_end + right_pad`); the Y range is
/// the cell's own rows. Padding in this engine is a horizontal spacing rule.
///
/// ⚠️ **Both `pixel->cell` AND `pixel->padding_reserved_by` are checked.** The second is how a
/// fixed cell's padding claim is honoured — a square nobody occupies can still be reserved, and
/// missing it lets a cell sit in another's spacing.
///
/// ℹ️ A pixel off the grid (`None`) is skipped, not failed — that is the core edge.
pub fn check_padding(
    x: i32, x_end: i32, y: i32, y_end: i32, left_pad: i32, right_pad: i32,
    cell: Class,
    // `(occupant, padding_reserver)` at a square, each with whether it is this same cell.
    at: &dyn Fn(i32, i32) -> Option<(Option<(Class, bool)>, Option<(Class, bool)>)>,
) -> bool {
    for gx in (x - left_pad)..(x_end + right_pad) {
        for gy in y..y_end {
            let Some((occupant, reserver)) = at(gx, gy) else { continue };
            for other in [occupant, reserver].into_iter().flatten() {
                if has_padding_conflict(cell, other.0, other.1) {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matrix_matches_the_table_in_the_source() {
        use Class::*;
        use PairRule::*;
        //     CR WT BL SP
        // CR   P  P  P  O
        assert_eq!(pair_rule(Cr, Cr), NoPaddedOverlap);
        assert_eq!(pair_rule(Cr, Wt), NoPaddedOverlap);
        assert_eq!(pair_rule(Cr, Bl), NoPaddedOverlap);
        assert_eq!(pair_rule(Cr, Sp), NoOverlap);
        // WT   P  O  P  O   — ⛔ well-tap against well-tap is the exception
        assert_eq!(pair_rule(Wt, Cr), NoPaddedOverlap);
        assert_eq!(pair_rule(Wt, Wt), NoOverlap);
        assert_eq!(pair_rule(Wt, Bl), NoPaddedOverlap);
        assert_eq!(pair_rule(Wt, Sp), NoOverlap);
        // BL   P  P  -  O
        assert_eq!(pair_rule(Bl, Cr), NoPaddedOverlap);
        assert_eq!(pair_rule(Bl, Wt), NoPaddedOverlap);
        assert_eq!(pair_rule(Bl, Bl), OverlapAllowed);
        assert_eq!(pair_rule(Bl, Sp), NoOverlap);
        // SP   O  O  O  O
        for other in [Cr, Wt, Bl, Sp] {
            assert_eq!(pair_rule(Sp, other), NoOverlap, "spacers ignore padding against {other:?}");
        }
    }

    #[test]
    fn classification_follows_the_master_type() {
        assert_eq!(classify("CORE"), Class::Cr);
        assert_eq!(classify("CORE_TIEHIGH"), Class::Cr);
        assert_eq!(classify("CORE_WELLTAP"), Class::Wt, "a well tap is NOT plain core");
        assert_eq!(classify("BLOCK_SOFT"), Class::Bl);
        assert_eq!(classify("CORE_SPACER"), Class::Sp);
        assert_eq!(classify("ENDCAP_LEF58_RIGHTEDGE"), Class::Sp, "every ENDCAP variant");
        assert_eq!(classify("PAD_INPUT"), Class::Ignored);
        assert_eq!(classify("SOMETHING_NEW"), Class::Ignored, "unknown falls through as ignored");
    }

    #[test]
    fn a_cell_never_conflicts_with_itself() {
        // ⚠️ This is what lets the scan walk the cell's own footprint.
        assert!(!has_padding_conflict(Class::Cr, Class::Cr, true));
        assert!(has_padding_conflict(Class::Cr, Class::Cr, false));
    }

    #[test]
    fn padding_widens_the_scan_in_x_only() {
        // A conflicting cell one square LEFT of the footprint is caught only via left_pad;
        // the same cell one row BELOW is never scanned, because padding does not widen Y.
        let left_neighbour = |gx: i32, gy: i32| {
            Some(if (gx, gy) == (-1, 0) { (Some((Class::Cr, false)), None) } else { (None, None) })
        };
        assert!(check_padding(0, 2, 0, 1, 0, 0, Class::Cr, &left_neighbour), "no padding: clear");
        assert!(!check_padding(0, 2, 0, 1, 1, 0, Class::Cr, &left_neighbour), "left_pad sees it");

        let below = |gx: i32, gy: i32| {
            Some(if (gx, gy) == (0, -1) { (Some((Class::Cr, false)), None) } else { (None, None) })
        };
        assert!(check_padding(0, 2, 0, 1, 5, 5, Class::Cr, &below),
                "padding never widens Y, so the row below is out of scope");
    }

    #[test]
    fn a_reserved_square_conflicts_even_with_no_occupant() {
        // ⚠️ padding_reserved_by is how a fixed cell's spacing claim is honoured.
        let reserved = |_: i32, _: i32| Some((None, Some((Class::Cr, false))));
        assert!(!check_padding(0, 1, 0, 1, 0, 0, Class::Cr, &reserved));
    }

    #[test]
    fn off_grid_squares_are_skipped_not_failed() {
        // ℹ️ That is the core edge, not a violation.
        let off = |_: i32, _: i32| None;
        assert!(check_padding(0, 4, 0, 2, 3, 3, Class::Cr, &off));
    }

    #[test]
    fn spacers_and_blocks_behave_as_the_table_says() {
        // A spacer beside a core cell is fine even with padding demanded.
        let spacer = |_: i32, _: i32| Some((Some((Class::Sp, false)), None));
        assert!(check_padding(0, 1, 0, 1, 2, 2, Class::Cr, &spacer));
        // Two blocks may overlap outright.
        let block = |_: i32, _: i32| Some((Some((Class::Bl, false)), None));
        assert!(check_padding(0, 1, 0, 1, 0, 0, Class::Bl, &block));
        // But a block against a core cell may not.
        assert!(!check_padding(0, 1, 0, 1, 0, 0, Class::Cr, &block));
    }
}
