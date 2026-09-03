# vyges-dpl

Detailed placement over the OpenDB design database: **legality checking** and **legalization**.

```text
vyges physical dpl check-placement    design.odb
vyges physical dpl detailed-placement design.odb --out-odb out.odb
```

`detailed-placement` takes upstream's own tunables — `--max-displacement`,
`--site-search-window`, `--row-search-window`, `--drc-penalty`,
`--disable-window-extension`, `--use-diamond-legalizer` — with upstream's defaults. Run
`--help` for the full list, and `--describe` for the machine-readable contract.

ℹ️ Two of upstream's options are deliberately absent. `-disallow_one_site_gaps` is **deprecated
there** (it warns DPL-3 and ignores the flag, because the setting is derived from
`hasOneSiteMaster()`), so accepting it would promise a control that cannot change the answer;
this engine refuses it with that explanation rather than accepting it silently. `-incremental`
is unimplemented and named in `not_done`.

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

⬜ Not implemented, and named in `not_done` on every run: groups and regions, placement padding
values, incremental placement, and two of `countDRCViolations`' four terms — `checkEdgeSpacing`
(needs each master's LEF58 cell-edge list) and `checkBlockedLayers`.

⚠️ **Nothing in the comparable corpus exercises those two DRC terms**, so their absence is
invisible to the score rather than shown to be harmless. `checkPadding` was in exactly that
position until it was built, and wiring it moved `aes` by 5,700 cells.

Every run also names, in `filtered_out`, each instance the model filter excluded — a filter that
drops instances silently is indistinguishable from a design that has none of them.

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

## Status

✅ **Legalization matches the reference on every comparable case in its own regression suite** —
**28 of 28**, at pin `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, including the three large
designs:

| design | components | result |
| --- | ---: | --- |
| `aes` | 21,340 | identical |
| `ibex` | 34,184 | identical |
| `gcd` | 549 | identical |
| the other 25 cases | — | identical |

🔑 **The agreement is sweep-level, not final-placement only.** Upstream's own per-iteration debug
trace and this engine's match line for line — same cell, same order, same chosen position, every
iteration. A matching output can be coincidence; a matching decision sequence is the algorithm.

⛔ **That is a claim about what this corpus asks, not about every design.** 35 of upstream's 63
`detailed_placement` cases are outside it and are not scored at all: 12 ship no golden, 8 need
filler placement, 7 declare regions or groups, 7 need placement padding values, 1 needs both.

⚠️ **Every score is scoped to one upstream commit.** A score quoted without its pin says nothing:
the reference moves.

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
