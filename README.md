# vyges-dpl

Detailed placement over the OpenDB design database: **legality checking** and **legalization**.

```text
vyges physical dpl check-placement    design.odb
vyges physical dpl detailed-placement design.odb --out-odb out.odb
```

`detailed-placement` takes the reference's own tunables — `--max-displacement`,
`--site-search-window`, `--row-search-window`, `--drc-penalty`,
`--disable-window-extension`, `--use-diamond-legalizer` — with the reference's defaults. Run
`--help` for the full list, and `--describe` for the machine-readable contract.

ℹ️ Two of the reference's options are deliberately absent. `-disallow_one_site_gaps` is **deprecated
there** (it warns DPL-3 and ignores the flag, because the setting is derived from
whether the library has a one-site master), so accepting it would promise a control that cannot change the answer;
this engine refuses it with that explanation rather than accepting it silently. `-incremental`
is unimplemented and named in `not_done`.

## Scope — read this before the correctness section

⛔ **Every run names what it did NOT do.** `check-placement` reports `not_checked` (families it
did not evaluate) and `limitations` (families it did evaluate, but could not see everything);
`detailed-placement` reports `not_done`. A clean verdict from a partial tool must not read as a
complete one, so those fields are emitted whether or not they are empty.

### Legalization

Two legalizers, and **the default is the one the reference defaults to**: negotiated congestion, in
which cells are allowed to overlap, contested sites accumulate a history cost, and each iteration
rips up and re-places until nobody overlaps. `--use-diamond-legalizer` selects the other — a
diamond search outward from each cell's own position — mirroring the reference's flag of the same name.

⚠️ The two produce **different placements**, so the report says which one ran. Comparing one
legalizer's output against the other's expected result measures nothing.

✅ **Placement padding** (`set_placement_padding` — global, per master, per instance, for the
padded CORE classes only) is modelled on the negotiation path: `--padding-global L R`,
`--padding-master M L R`, `--padding-inst I L R`, in sites. Padding lives in opendp's memory, not
the database, so a caller that set it must pass it. With the diamond legalizer it is refused.

⬜ Not implemented, and named in `not_done` on every run: groups and regions, incremental placement, and two of the legalizer's four DRC terms — edge spacing
(needs each master's LEF58 cell-edge list) and blocked layers.

⚠️ **Nothing in the comparable corpus exercises those two DRC terms**, so their absence is
invisible to the score rather than shown to be harmless. The padding term was in exactly that
position until it was built, and wiring it moved `aes` by 5,700 cells.

Every run also names, in `filtered_out`, each instance the model filter excluded — a filter that
drops instances silently is indistinguishable from a design that has none of them.

### Checking

**Seven of the reference's nine check families are evaluated:** site alignment, placed, overlap, in
rows, padding, blocked layers and one-site gaps.

| family | status |
| --- | --- |
| site alignment · placed · overlap · in rows | ✅ evaluated |
| padding | ✅ evaluated by `check-placement` with **zero padding** — the checker takes no padding values yet (the legalizer does), so only class-pair conflicts are caught |
| blocked layers | ✅ evaluated where the design has a vertical M2/M3 special wire; where it has none the mask is empty and a pass would be vacuous, so it is reported as unchecked instead |
| one-site gaps | ✅ evaluated when the technology has no one-site master — the condition the reference derives it from, not a user option |
| region placement | ⬜ needs a region model |
| edge spacing | ⬜ the LEF58 cell-edge rule is implemented and tested; what is missing is each master's edge list |

🔑 The checker came first on purpose. It is the oracle — it needs no legalizer to be correct in
order to be useful — and the reference suite leans on it harder: **77 of 92 tests call
`check_placement`, 68 call `detailed_placement`**.

## Status

At pin `27288f5c90265ae5296b2f5a9fa6e8714fa191ad` (2026-10-12), `detailed_placement` matches the reference on
76 of 78 comparable cases and `check-placement` agrees on 77 of 78; the cases that differ are new reference cases
for pins under power-via-stack metal (`blocked_via_layers`, `blocked_via_pins`), which the blocked-layers rule
does not yet evaluate.

At pin `da9f29f18b6487825aa880597176e0fa97110b31` (2026-09-22), with padding modelled:
**46 of 46 comparable cases match** the reference component for component, with filler
placement (`filler-placement`) scored on eight of them — `obstruction1` alone is 19,768 components —
and the sweep trace (the reference's per-iteration debug line) identical for the whole run on
`cell_on_block2` (16,176 lines), `obstruction2` (248) and `edge_spacing`, among others.
`edge_spacing` needed LEF58 cell-edge spacing in the legalizer's DRC term: each master's edges
and the technology's table, read through new `vyges-opendb` accessors, with rectangle overlap
STRICT, as odb defines it, so edges that meet only at a corner are not compared.

The last three defects, each found by printing every candidate of the first diverging
location search on both sides (`VYGES_DPL_CELL` / `VYGES_DPL_ITER`, and an instrumented
reference run):

- the search window was built from the raw base site window, not the effective site window =
  `max(base, cell width)` (`cell_on_block2`);
- the stall recovery's placement test read negotiation CAPACITY where the reference reads the DPL
  grid's validity — they differ under a fixed cell clamped at load, where the negotiation
  grid is blockaded from the clamped x but the DPL pixels carry the macro at its real position
  (`obstruction2`: 0 of 31 stuck cells recovered, reference 31);
- the negotiation-order sort reorders the run's active list IN PLACE, and the recovery visits cells in
  that order; ours sorted a private copy (`obstruction2`).

The earlier statement, kept for its history:

✅ **Legalization matches the reference on every comparable case in its own regression suite** —
**28 of 28**, at pin `7d490b8ecd357199c0c0e9f3e32becd5eb507c34`.

🔑 **The agreement is SWEEP-LEVEL, not final-placement only.** The reference's own per-iteration debug
trace and this engine's match line for line — same cell, same order, same chosen position, every
iteration — on `simple05`, `simple07`, `gcd` (574 lines) and `hybrid_cells`.

ℹ️ **It read 18 of 28 when the pin first moved** from `945a9f48dc6e5cc91d865daa92c45a1094cb682c`,
because the reference reworked the negotiation legalizer's initial snapping across seven commits and
refreshed 24 of its own goldens. Four mechanisms recovered it:

| mechanism | what it changed |
| --- | --- |
| initial x snap | the initial x **rounds** to the nearest site rather than truncating |
| displacement units | displacement is **site widths on both axes**, so a row jump is no longer priced as one step — and the search wavefront reorders with it |
| the initial snap's diamond search | replaces a four-direction scan that could not see a diagonal |
| **the snap runs as its own pass** | after every fixed cell is blockaded, testing grid **capacity** rather than merely whether a site exists |

⚠️ **The last one is a CALL SEQUENCE fix, not a logic one** — the function was right and was being
called on inputs that were not ready. It took the gate from 23/28 to 28/28 on its own.

The three large designs, at the current pin:

| design | components | result at `7d490b8` |
| --- | ---: | --- |
| `aes` | 21,340 | identical |
| `ibex` | 34,184 | identical |
| `gcd` | 549 | identical |
| the other 25 cases | — | identical |

⚠️ **And the denominator is short.** The reference suite ships **69** cases calling
`detailed_placement`, not 63: five (`report_failures`, `fragmented_row03`, `pad02`, `fillers8`,
`obstruction2`) wrap it as `catch { detailed_placement }`, so the harness's command filter does not
see them and they are scored by nothing. All five are error-path cases.

🔑 **The agreement is sweep-level, not final-placement only.** The reference's own per-iteration debug
trace and this engine's match line for line — same cell, same order, same chosen position, every
iteration. A matching output can be coincidence; a matching decision sequence is the algorithm.

⛔ **That is a claim about what this corpus asks, not about every design.** 35 of the reference's 63
`detailed_placement` cases are outside it and are not scored at all: 12 ship no golden, 8 need
filler placement, 7 declare regions or groups, 7 need placement padding values, 1 needs both.

⚠️ **Every score is scoped to one reference commit.** A score quoted without its pin says nothing:
the reference moves.

## Correctness

Correlated against the reference at pin `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, **in both
directions** — a checker that cannot fail proves nothing:

| design | reference | this engine |
| --- | --- | --- |
| `aes` (legal) | clean | 21,340 cells, **0 violations** |
| `cell_on_block1` (illegal) | *"Site aligned check failed (4)"* | **4** site-align failures |

Four behaviours that are easy to get wrong and are pinned as tests at their sites:

- **Site alignment is core-relative.** The reference measures a cell's x offset modulo the
  site width relative to the core's lower-left corner. Read as an absolute coordinate it reports every cell
  of a clean design as misaligned — measured, 21,340 of 21,340.
- **A site-alignment failure removes the cell from the overlap comparison entirely.** That is a
  side effect of the reference's early exit, not a separate rule: the overlap check is what paints a cell
  into its pixels, so a skipped cell is never there to collide with.
- **A cell's height in ROWS comes from its master, not from where the cell currently sits.** A
  cell resting a few database units above its row covers two row bands; it is still a
  single-height cell, and every rule that treats multi-row cells specially depends on the
  difference.
- **A hard placement blockage reaches the legality test only through pixel CAPACITY.** Nothing in
  the earlier guards — in-die, valid row, site orientation — consults pixel validity, so a legality
  test that stops before the final footprint loop calls a cell inside a blockage perfectly legal.

ℹ️ The overlap **acceleration** differs deliberately — a rectangle sweep here, a pixel walk
in the reference. The predicate is identical and the failing set matches; which partner is reported can
differ, because the reference names whichever cell already owns the pixel.

## Status

`--describe` reports `maturity: partial`, and a test fails if that word is left behind when the
unimplemented families listed above are filled in.

⚠️ **Correlation against the reference corpus is ongoing and the engine is not yet a drop-in
replacement.** The scope section above is the honest boundary; `not_done`, `not_checked` and
`limitations` in the run output are the machine-readable version of it, and they are what to trust
over any prose here.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

ℹ️ This engine reads the design database through `vyges-opendb`, which binds
OpenROAD's OpenDB (libodb) — BSD 3-Clause, Copyright (c) 2019-2026 The Regents of the
University of California. The attribution is in `NOTICE`.
