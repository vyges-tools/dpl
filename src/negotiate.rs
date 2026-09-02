// SPDX-License-Identifier: Apache-2.0
//! `NegotiationLegalizer` — the DEFAULT legalizer at the pin.
//!
//! Reference: OpenROAD `src/dpl/src/NegotiationLegalizer{,Pass}.cpp` at
//! `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, read from the tree whose `git rev-parse HEAD`
//! matches that pin.
//!
//! ⛔ **Why this and not `diamondDPL`.** `use_diamond_legalizer_` defaults to **false** and
//! `isUseNegotiationLegalizer()` returns its negation, so negotiation is what a plain
//! `detailed_placement` runs. Only **4 of 67** upstream cases pass `-use_diamond_legalizer`.
//!
//! 🔑 **PathFinder-style negotiated congestion.** Cells are allowed to overlap; contested sites
//! accumulate a *history cost*; each iteration rips up and re-places until nothing overlaps. That
//! is a different shape from `diamondDPL`, which seats each cell once at the nearest legal site.
//!
//! ⚠️ **Only ILLEGAL cells are negotiated** — a cell that is already legal is left where it is.
//!
//! ⬜ **Status: the model and the ordering are built; the iteration core is not.** `findBestLocation`
//! (the cost function), `ripUp`/`place`, the history update and phase 2 remain. The full call
//! sequence is recorded in `vyges-tools-internal/docs/openroad/dpl/negotiation-legalizer.md` so
//! the next pass builds from a read reference rather than re-reading 2,315 lines.

/// One cell in the negotiation model — upstream's `NegCell`.
///
/// ⚠️ Coordinates are in GRID units (site columns, row indices), not DBU. The negotiation grid is
/// its own, separate from `dpl`'s `Grid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegCell {
    pub name: String,
    pub x: i32,
    pub y: i32,
    /// Width in SITES and height in ROWS.
    pub width: i32,
    pub height: i32,
    pub fixed: bool,
    pub legal: bool,
}

/// One square of the negotiation grid.
///
/// 🔑 `usage` is what makes this "negotiated": a site may be claimed by more than one cell, and the
/// excess is the pressure the algorithm works to remove. `hist_cost` remembers contested sites
/// across iterations so a site that keeps being fought over becomes expensive.
#[derive(Debug, Clone, Default)]
pub struct NegPixel {
    pub usage: i32,
    pub hist_cost: f64,
    pub valid: bool,
}

impl NegPixel {
    /// Sites claimed beyond the one the square can serve.
    ///
    /// ⚠️ **A fixed cell makes `usage` 1 with no cell negotiating for it**, so `overuse` is
    /// `max(0, usage - 1)` rather than `usage > 0`. Treating any usage as overuse would report a
    /// legal design as fully congested.
    pub fn overuse(&self) -> i32 {
        (self.usage - 1).max(0)
    }
}

/// The key `sortByNegotiationOrder` sorts on.
///
/// ⛔ **`(overuse DESC, height DESC, width DESC, idx ASC)`** — and the trailing index is the
/// determinism tie-break, the same role `sequence` plays in `diamondSearch`. Upstream builds this
/// as a decorate-sort and its comment records that the decorated form *"yields identical results
/// to scoring (a, b) directly"*, so the decoration is a speed-up rather than a behaviour change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub overuse: i32,
    pub height: i32,
    pub width: i32,
    pub idx: usize,
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Most-overused first, then tallest, then widest, then lowest index.
        other
            .overuse
            .cmp(&self.overuse)
            .then(other.height.cmp(&self.height))
            .then(other.width.cmp(&self.width))
            .then(self.idx.cmp(&other.idx))
    }
}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Order the active cells for one negotiation sweep.
pub fn sort_by_negotiation_order(keys: &mut [SortKey]) {
    keys.sort();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(overuse: i32, height: i32, width: i32, idx: usize) -> SortKey {
        SortKey { overuse, height, width, idx }
    }

    #[test]
    fn a_fixed_site_is_used_but_not_overused() {
        // ⛔ The distinction the whole cost function rests on.
        assert_eq!(NegPixel { usage: 0, ..Default::default() }.overuse(), 0);
        assert_eq!(NegPixel { usage: 1, ..Default::default() }.overuse(), 0);
        assert_eq!(NegPixel { usage: 2, ..Default::default() }.overuse(), 1);
        assert_eq!(NegPixel { usage: 5, ..Default::default() }.overuse(), 4);
    }

    #[test]
    fn the_most_overused_cell_is_negotiated_first() {
        let mut v = [k(0, 1, 1, 0), k(7, 1, 1, 1), k(3, 1, 1, 2)];
        sort_by_negotiation_order(&mut v);
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [1, 2, 0]);
    }

    #[test]
    fn equal_overuse_breaks_on_height_then_width() {
        let mut v = [k(2, 1, 9, 0), k(2, 3, 1, 1), k(2, 1, 2, 2)];
        sort_by_negotiation_order(&mut v);
        // Tallest first; then among the height-1 pair the wider one.
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [1, 0, 2]);
    }

    #[test]
    fn the_index_is_the_determinism_tie_break() {
        // ⛔ Without it these are indistinguishable and the sweep order is whatever the sort does.
        let mut v = [k(1, 1, 1, 9), k(1, 1, 1, 2), k(1, 1, 1, 5)];
        sort_by_negotiation_order(&mut v);
        assert_eq!(v.iter().map(|s| s.idx).collect::<Vec<_>>(), [2, 5, 9]);
    }

    #[test]
    fn the_order_does_not_depend_on_the_input_order() {
        let mk = || vec![k(0, 2, 2, 3), k(4, 1, 1, 1), k(4, 2, 1, 7), k(0, 2, 2, 0)];
        let (mut a, mut b) = (mk(), mk());
        b.reverse();
        sort_by_negotiation_order(&mut a);
        sort_by_negotiation_order(&mut b);
        assert_eq!(a, b, "the sweep order must not depend on how the active set was built");
    }
}
