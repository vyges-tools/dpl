// SPDX-License-Identifier: Apache-2.0
//! `dpl` — detailed placement: legality checking, and (later) legalization.
//!
//! Reference is OpenROAD `src/dpl` at pin `945a9f48dc6e5cc91d865daa92c45a1094cb682c`.
//!
//! ⛔ **Scope, stated because the name promises more than this delivers today.** `check-placement`
//! is here; `detailed_placement` is not. That order is deliberate: the checker is the ORACLE, it
//! needs no placement to be correct, and upstream's own suite leans on it more heavily — **77 of
//! 92 `.tcl` tests call `check_placement`, 68 call `detailed_placement`**.
//!
//! 🔑 **The Grid is the engine.** `checkInRows` and `checkOverlap` both resolve through
//! `Grid`/`Pixel`, and `checkOverlap` MUTATES it (`pixel->cell = &cell` on empty pixels), which is
//! why upstream runs the one-site-gap loop separately, afterwards, with a comment saying so. A
//! scoping estimate recorded before the source was read predicted the risk was "the `Grid`/
//! `Padding` model rather than the search" — that is now confirmed rather than assumed.
//!
//! **Filler insertion ([`fillers`]) is ADDITIVE, and every consumer may rely on that.** Filling the
//! row gaps only creates new instances — placed, oriented, connected to no signal net — and never
//! moves, flips or reconnects an instance that was already there. Its effect downstream is therefore
//! exactly the set of new instances: a routing result or guide set produced before it is unchanged
//! by it, and a harness may skip it when the only thing scored is such a result. ⛔ That holds only
//! while nothing AFTER it reads the placement: a later legalization, route or repair sees the
//! fillers as occupied sites. A placement or DEF comparison after it is never unaffected — which
//! is what `filler-placement` is for.
pub mod check;
pub mod drc;
pub mod edges;
pub mod fillers;
pub mod grid;
pub mod mirror;
pub mod negotiate;
pub mod network;
pub mod place;
pub mod regions;

/// This crate's version, as Cargo knows it — the single number the whole suite is released on.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The copyright line `--version` prints.
pub const COPYRIGHT: &str = "© 2026 Vyges. All Rights Reserved.  https://vyges.com";
