# vyges-dpl

Detailed placement over the OpenDB design database: **legality checking** and **legalization**.

```text
vyges physical dpl check-placement    design.odb
vyges physical dpl detailed-placement design.odb --out-odb out.odb
```

## Scope — read this before the correctness section

⛔ **Every run names what it did NOT do.** `check-placement` reports `not_checked` (families it
did not evaluate) and `limitations` (families it did evaluate, but could not see everything);
`detailed-placement` reports `not_done`. A clean verdict from a partial tool must not read as a
complete one, so those fields are emitted whether or not they are empty.

### Legalization

Two legalizers, and **the default is the one upstream defaults to**: negotiated congestion, in
which cells are allowed to overlap, contested sites accumulate a history cost, and each iteration
rips up and re-places until nobody overlaps. `--use-diamond-legalizer` selects the other — a
diamond search outward from each cell's own position — mirroring upstream's flag of the same name.

⚠️ The two produce **different placements**, so the report says which one ran. Comparing one
legalizer's output against the other's expected result measures nothing.

⬜ Not implemented, and named in `not_done` on every run: groups and regions, master symmetry,
DRC history costs, and the diamond-search recovery a stalled negotiation falls back on.

### Checking

**Seven of upstream's nine check families are evaluated:** site alignment, placed, overlap, in
rows, padding, blocked layers and one-site gaps.

| family | status |
| --- | --- |
| site alignment · placed · overlap · in rows | ✅ evaluated |
| padding | ✅ evaluated, with **zero padding** — no `set_placement_padding` model, so only class-pair conflicts are caught |
| blocked layers | ✅ evaluated where the design has a vertical M2/M3 special wire; where it has none the mask is empty and a pass would be vacuous, so it is reported as unchecked instead |
| one-site gaps | ✅ evaluated when the technology has no one-site master — the condition upstream derives it from, not a user option |
| region placement | ⬜ needs a region model |
| edge spacing | ⬜ the LEF58 cell-edge rule is implemented and tested; what is missing is each master's edge list |

🔑 The checker came first on purpose. It is the oracle — it needs no legalizer to be correct in
order to be useful — and upstream's own suite leans on it harder: **77 of 92 `.tcl` tests call
`check_placement`, 68 call `detailed_placement`**.

## Correctness

Correlated against OpenROAD at pin `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, **in both
directions** — a checker that cannot fail proves nothing:

| design | reference | this engine |
| --- | --- | --- |
| `aes.defok` (legal) | clean | 21,340 cells, **0 violations** |
| `cell_on_block1.def` (illegal) | *"Site aligned check failed (4)"* | **4** site-align failures |

Four behaviours that are easy to get wrong and are pinned as tests at their sites:

- **Site alignment is core-relative.** Upstream compares `cell->getLeft() % siteWidth`, and
  `getLeft()` is relative to `core_.xMin()`. Read as an absolute coordinate it reports every cell
  of a clean design as misaligned — measured, 21,340 of 21,340.
- **A site-alignment failure removes the cell from the overlap comparison entirely.** That is a
  side effect of upstream's `continue`, not a separate rule: `checkOverlap` is what paints a cell
  into its pixels, so a skipped cell is never there to collide with.
- **A cell's height in ROWS comes from its master, not from where the cell currently sits.** A
  cell resting a few database units above its row covers two row bands; it is still a
  single-height cell, and every rule that treats multi-row cells specially depends on the
  difference.
- **A hard placement blockage reaches the legality test only through pixel CAPACITY.** Nothing in
  the earlier guards — in-die, valid row, site orientation — consults pixel validity, so a legality
  test that stops before the final footprint loop calls a cell inside a blockage perfectly legal.

ℹ️ The overlap **acceleration** differs deliberately — a rectangle sweep here, a pixel walk
upstream. The predicate is identical and the failing set matches; which partner is reported can
differ, because upstream names whichever cell already owns the pixel.

## Status

`--describe` reports `maturity: partial`, and a test fails if that word is left behind when the
unimplemented families listed above are filled in.

⚠️ **Correlation against upstream's corpus is ongoing and the engine is not yet a drop-in
replacement.** The scope section above is the honest boundary; `not_done`, `not_checked` and
`limitations` in the run output are the machine-readable version of it, and they are what to trust
over any prose here.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

ℹ️ This engine reads the design database through `vyges-opendb`, which binds
OpenROAD's OpenDB (libodb) — BSD 3-Clause, Copyright (c) 2019-2026 The Regents of the
University of California. The attribution is in `NOTICE`.
