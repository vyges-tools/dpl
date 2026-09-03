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

/// `checkBlockedLayers` — does any square under the cell block a layer the cell uses?
///
/// `pixel.blocked_layers & cell.used_layers` over the footprint; any overlap fails.
///
/// ℹ️ `blocked_layers` is set by `Grid::markBlocked` from **vertical M2/M3 special-net wires only**
/// (`routing_level` 2 or 3, `getDir() != horizontal`), so this is narrower than it sounds: it is
/// about power straps crossing a cell, not about routing in general.
pub fn check_blocked_layers(
    x: i32, x_end: i32, y: i32, y_end: i32, used_layers: u32,
    blocked_at: &dyn Fn(i32, i32) -> Option<u32>,
) -> bool {
    for gy in y..y_end {
        for gx in x..x_end {
            if let Some(blocked) = blocked_at(gx, gy) {
                if blocked & used_layers != 0 {
                    return false;
                }
            }
        }
    }
    true
}

/// Which spelling of the one-site-gap neighbour test to use.
///
/// ⛔ **Upstream implements this check TWICE, and the two disagree at the core edge.**
///
/// | | `cellAtSite(x, y)` |
/// | --- | --- |
/// | `Place.cpp::checkPixels` | `pixel != nullptr && pixel->cell` — off-grid means NO cell |
/// | `PlacementDRC::checkOneSiteGap` | `pixel == nullptr \|\| pixel->cell` — off-grid means THERE IS one |
///
/// ⚠️ In `PlacementDRC` that makes `cellAtSite` **byte-identical to `isAbutted`**, which reads like
/// a copy-paste slip rather than a decision. The consequence is real: a cell one empty site from
/// the core edge is a violation under `PlacementDRC` and clean under `Place.cpp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeReading {
    /// `PlacementDRC`'s: off-grid counts as occupied. **This engine's default** — it is the
    /// behaviour of the file being transcribed.
    OffGridIsOccupied,
    /// `Place.cpp`'s: off-grid counts as empty.
    OffGridIsEmpty,
}

/// `checkOneSiteGap` — forbid leaving exactly one empty site between two cells.
///
/// Scanned per row: the square immediately left of the cell and the one immediately right. A
/// violation is *"that square is empty **and** the one beyond it holds a cell"* — i.e. a gap
/// exactly one site wide, which no filler can occupy.
///
/// ⚠️ **Off by default.** Upstream returns `true` immediately unless `disallow_one_site_gap_` is
/// set, so a design that never asks for it is never checked.
pub fn check_one_site_gap(
    enabled: bool, x: i32, x_end: i32, y: i32, y_end: i32,
    reading: EdgeReading,
    // `None` = off the grid; `Some(true)` = a cell is here.
    occupied_at: &dyn Fn(i32, i32) -> Option<bool>,
) -> bool {
    if !enabled {
        return true;
    }
    // `isAbutted`: off-grid or occupied — either way there is no gap to worry about.
    let is_abutted = |gx: i32, gy: i32| occupied_at(gx, gy).is_none_or(|c| c);
    let cell_at_site = |gx: i32, gy: i32| match reading {
        EdgeReading::OffGridIsOccupied => occupied_at(gx, gy).is_none_or(|c| c),
        EdgeReading::OffGridIsEmpty => occupied_at(gx, gy).unwrap_or(false),
    };
    for gy in y..y_end {
        if !is_abutted(x - 1, gy) && cell_at_site(x - 2, gy) {
            return false;
        }
        if !is_abutted(x_end, gy) && cell_at_site(x_end + 1, gy) {
            return false;
        }
    }
    true
}

/// One entry of the LEF58 cell-edge spacing table.
///
/// ⚠️ **`except_abutted` is PARSED AND NEVER READ** by `checkEdgeSpacing` at this pin — the entry
/// carries it, the check consults only `spc` and `is_exact`. Kept so the shape matches, and so
/// that wiring it up later is an addition rather than a re-parse.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EdgeSpacingEntry {
    pub spc: i32,
    pub is_exact: bool,
    pub except_abutted: bool,
}

/// The symmetric edge-type spacing table — `makeCellEdgeSpacingTable`.
///
/// Edge-type names are assigned indices in **encounter order** across the rules, and each rule
/// fills **both** `[i][j]` and `[j][i]`: the relation is symmetric.
#[derive(Debug, Default, Clone)]
pub struct EdgeSpacingTable {
    names: Vec<String>,
    table: Vec<Vec<EdgeSpacingEntry>>,
}

impl EdgeSpacingTable {
    /// Build from `(first_edge, second_edge, spacing, exact, except_abutted)` rules.
    pub fn build(rules: &[(String, String, i32, bool, bool)]) -> EdgeSpacingTable {
        if rules.is_empty() {
            return EdgeSpacingTable::default();
        }
        // ⚠️ Encounter order, first edge then second, exactly as the two `try_emplace` calls run.
        let mut names: Vec<String> = Vec::new();
        let mut idx_of = std::collections::HashMap::new();
        for (a, b, ..) in rules {
            for n in [a, b] {
                if !idx_of.contains_key(n) {
                    idx_of.insert(n.clone(), names.len());
                    names.push(n.clone());
                }
            }
        }
        let n = names.len();
        let mut table = vec![vec![EdgeSpacingEntry::default(); n]; n];
        for (a, b, spc, exact, except_abutted) in rules {
            let e = EdgeSpacingEntry { spc: *spc, is_exact: *exact,
                                       except_abutted: *except_abutted };
            let (i, j) = (idx_of[a], idx_of[b]);
            table[i][j] = e;
            table[j][i] = e;
        }
        EdgeSpacingTable { names, table }
    }

    /// ⚠️ **An empty table means the check is skipped entirely** — `checkEdgeSpacing` returns true
    /// before doing anything when the technology states no rules.
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    pub fn index_of(&self, edge_type: &str) -> Option<usize> {
        self.names.iter().position(|n| n == edge_type)
    }

    pub fn entry(&self, a: usize, b: usize) -> EdgeSpacingEntry {
        self.table[a][b]
    }

    /// `getMaxSpacing` — the widest spacing this edge type demands against anything.
    pub fn max_spacing(&self, edge_type_idx: usize) -> i32 {
        self.table[edge_type_idx].iter().map(|e| e.spc).max().unwrap_or(0)
    }

    /// The search radius for an edge — `getMaxSpacing(...) + 1`.
    ///
    /// ⛔ **The `+1` is "to account for EXACT rules"**, in upstream's words. An exact rule fires
    /// when the distance EQUALS the spacing, so a query bloated by exactly `spc` would put a
    /// neighbour at distance `spc` on the boundary and risk missing it.
    pub fn query_radius(&self, edge_type_idx: usize) -> i32 {
        self.max_spacing(edge_type_idx) + 1
    }
}

/// Is the spacing between two parallel edges a violation?
///
/// ⛔ **An EXACT rule fires when the distance EQUALS the spacing — not when it is less.** So a
/// closer neighbour passes an exact rule and a farther one passes too; only the stated distance is
/// forbidden. Reading `is_exact` as "at least" inverts the rule for every distance below `spc`.
pub fn edge_spacing_violation(dist: i32, entry: EdgeSpacingEntry) -> bool {
    if entry.is_exact {
        dist == entry.spc
    } else {
        dist < entry.spc
    }
}

/// `getQueryRect` — bloat the edge box across its own direction by `spc`.
///
/// ⚠️ **A VERTICAL edge bloats HORIZONTALLY** and vice versa: the search widens across the gap the
/// edge faces, not along the edge itself.
pub fn query_rect(
    (xlo, ylo, xhi, yhi): (i32, i32, i32, i32), spc: i32, is_vertical: bool,
) -> (i32, i32, i32, i32) {
    if is_vertical {
        (xlo - spc, ylo, xhi + spc, yhi)
    } else {
        (xlo, ylo - spc, xhi, yhi + spc)
    }
}

/// The distance between two parallel edges — the span of their merged rect.
///
/// ⚠️ **`dx` for vertical edges, `dy` for horizontal** — measured ACROSS the gap. Upstream calls
/// this the "generalized intersection": merge the two boxes and take the span in the direction the
/// edges face.
pub fn edge_distance(
    a: (i32, i32, i32, i32), b: (i32, i32, i32, i32), is_vertical: bool,
) -> i32 {
    let (xlo, ylo) = (a.0.min(b.0), a.1.min(b.1));
    let (xhi, yhi) = (a.2.max(b.2), a.3.max(b.3));
    if is_vertical { xhi - xlo } else { yhi - ylo }
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
    fn a_blocked_layer_the_cell_uses_fails_it() {
        let m2 = 1 << 2;
        let m3 = 1 << 3;
        let blocked = |gx: i32, _: i32| Some(if gx == 1 { m2 } else { 0 });
        assert!(!check_blocked_layers(0, 3, 0, 1, m2, &blocked), "the cell uses M2, M2 is blocked");
        assert!(check_blocked_layers(0, 3, 0, 1, m3, &blocked), "it uses M3 only: clear");
        assert!(check_blocked_layers(0, 1, 0, 1, m2, &blocked), "square 1 is outside the footprint");
    }

    #[test]
    fn one_site_gap_is_off_unless_asked_for() {
        // ⚠️ A design that never enables it is never checked.
        let always_bad = |_: i32, _: i32| Some(true);
        assert!(check_one_site_gap(false, 0, 1, 0, 1, EdgeReading::OffGridIsOccupied,
                                   &always_bad));
    }

    #[test]
    fn exactly_one_empty_site_between_two_cells_is_the_violation() {
        // Cell occupies [0,1). Square 2 is empty, square 3 holds a cell -> a one-site gap.
        let occ = |gx: i32, _: i32| Some(matches!(gx, 3));
        assert!(!check_one_site_gap(true, 0, 2, 0, 1, EdgeReading::OffGridIsEmpty, &occ));
        // Move the neighbour one further out and the gap is two sites: legal.
        let occ2 = |gx: i32, _: i32| Some(matches!(gx, 4));
        assert!(check_one_site_gap(true, 0, 2, 0, 1, EdgeReading::OffGridIsEmpty, &occ2));
    }

    #[test]
    fn the_gap_is_checked_on_the_left_side_too() {
        // ⚠️ Added after a mutation that changed the LEFT neighbour distance survived: the
        // right-side test alone cannot catch it. Cell at [5,7); square 4 empty, square 3 holds a
        // cell -> a one-site gap on the left.
        let occ = |gx: i32, _: i32| Some(matches!(gx, 3));
        assert!(!check_one_site_gap(true, 5, 7, 0, 1, EdgeReading::OffGridIsEmpty, &occ));
        // One further out is a two-site gap: legal.
        let occ2 = |gx: i32, _: i32| Some(matches!(gx, 2));
        assert!(check_one_site_gap(true, 5, 7, 0, 1, EdgeReading::OffGridIsEmpty, &occ2));
    }

    #[test]
    fn the_two_upstream_readings_disagree_at_the_core_edge() {
        // ⛔ The cell sits at [0,2); square 2 is empty and square 3 is OFF THE GRID.
        // PlacementDRC calls off-grid occupied, so this is a violation; Place.cpp does not.
        let occ = |gx: i32, _: i32| if gx >= 3 { None } else { Some(false) };
        assert!(!check_one_site_gap(true, 0, 2, 0, 1, EdgeReading::OffGridIsOccupied, &occ),
                "PlacementDRC's reading flags it");
        assert!(check_one_site_gap(true, 0, 2, 0, 1, EdgeReading::OffGridIsEmpty, &occ),
                "Place.cpp's reading does not");
    }

    fn rules() -> Vec<(String, String, i32, bool, bool)> {
        vec![
            ("A".into(), "B".into(), 100, false, false),
            ("A".into(), "C".into(), 250, false, false),
            ("B".into(), "B".into(), 40, true, false),
        ]
    }

    #[test]
    fn the_table_is_symmetric_and_indexed_in_encounter_order() {
        let t = EdgeSpacingTable::build(&rules());
        // A first (first rule's first edge), then B, then C.
        assert_eq!(t.index_of("A"), Some(0));
        assert_eq!(t.index_of("B"), Some(1));
        assert_eq!(t.index_of("C"), Some(2));
        assert_eq!(t.index_of("Z"), None);
        // Both halves are filled by each rule.
        assert_eq!(t.entry(0, 1).spc, 100);
        assert_eq!(t.entry(1, 0).spc, 100, "the relation is symmetric");
    }

    #[test]
    fn an_empty_technology_table_skips_the_check_entirely() {
        // ⚠️ checkEdgeSpacing returns true before doing anything when there are no rules.
        assert!(EdgeSpacingTable::build(&[]).is_empty());
        assert!(!EdgeSpacingTable::build(&rules()).is_empty());
    }

    #[test]
    fn the_query_radius_is_the_max_spacing_plus_one() {
        // ⛔ The +1 exists so an EXACT rule at distance == spc is still inside the query.
        let t = EdgeSpacingTable::build(&rules());
        assert_eq!(t.max_spacing(0), 250, "A's widest demand is against C");
        assert_eq!(t.query_radius(0), 251);
    }

    #[test]
    fn an_exact_rule_fires_only_at_the_stated_distance() {
        // ⛔ Not "at least" — closer passes, farther passes, only equal is forbidden.
        let exact = EdgeSpacingEntry { spc: 40, is_exact: true, except_abutted: false };
        assert!(!edge_spacing_violation(39, exact), "closer than the exact distance is fine");
        assert!(edge_spacing_violation(40, exact), "exactly the stated distance is the violation");
        assert!(!edge_spacing_violation(41, exact));

        let normal = EdgeSpacingEntry { spc: 40, is_exact: false, except_abutted: false };
        assert!(edge_spacing_violation(39, normal), "an ordinary rule is a minimum");
        assert!(!edge_spacing_violation(40, normal));
    }

    #[test]
    fn a_vertical_edge_bloats_horizontally() {
        // ⚠️ The search widens ACROSS the gap the edge faces, not along the edge.
        assert_eq!(query_rect((10, 0, 10, 100), 5, true), (5, 0, 15, 100));
        assert_eq!(query_rect((0, 10, 100, 10), 5, false), (0, 5, 100, 15));
    }

    #[test]
    fn the_distance_is_the_span_of_the_merged_boxes() {
        // Two zero-width vertical edges at x=10 and x=30: the merged span is 20.
        assert_eq!(edge_distance((10, 0, 10, 50), (30, 0, 30, 50), true), 20);
        // Horizontal edges at y=10 and y=45 -> 35.
        assert_eq!(edge_distance((0, 10, 50, 10), (0, 45, 50, 45), false), 35);
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

/// `PlacementDRC::checkDRC` — all four checks, in upstream's fast-path order.
///
/// ⛔ **Cheapest first, bailing on the first failure**: blocked layers → one-site gap → padding →
/// edge spacing. ⚠️ Upstream's *debug* path runs all four in a different order so a report can
/// show each; the two orders are only interchangeable because the checks are independent, and this
/// takes the fast one.
pub struct DrcVerdict {
    pub blocked_layers: bool,
    pub one_site_gap: bool,
    pub padding: bool,
    pub edge_spacing: bool,
}

impl DrcVerdict {
    pub fn ok(&self) -> bool {
        self.blocked_layers && self.one_site_gap && self.padding && self.edge_spacing
    }

    /// `countDRCViolations` — how many of the four fail.
    ///
    /// ⚠️ **A COUNT, not a boolean.** `findBestLocation` multiplies it by the escalating penalty,
    /// so a position failing two rules must cost more than one failing a single rule — collapsing
    /// it to 0/1 would make those positions indistinguishable.
    pub fn count(&self) -> i32 {
        [self.blocked_layers, self.one_site_gap, self.padding, self.edge_spacing]
            .iter()
            .filter(|ok| !**ok)
            .count() as i32
    }
}

#[cfg(test)]
mod verdict_tests {
    use super::*;

    fn v(bl: bool, gap: bool, pad: bool, edge: bool) -> DrcVerdict {
        DrcVerdict { blocked_layers: bl, one_site_gap: gap, padding: pad, edge_spacing: edge }
    }

    #[test]
    fn all_four_must_pass() {
        assert!(v(true, true, true, true).ok());
        for i in 0..4 {
            let mut f = [true; 4];
            f[i] = false;
            assert!(!v(f[0], f[1], f[2], f[3]).ok(), "check {i} alone must fail the verdict");
        }
    }

    #[test]
    fn the_count_distinguishes_one_violation_from_several() {
        // ⚠️ findBestLocation multiplies this by the escalating penalty, so 2 must outrank 1.
        assert_eq!(v(true, true, true, true).count(), 0);
        assert_eq!(v(false, true, true, true).count(), 1);
        assert_eq!(v(false, false, true, true).count(), 2);
        assert_eq!(v(false, false, false, false).count(), 4);
    }
}
