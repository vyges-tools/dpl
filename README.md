# vyges-dpl

Detailed placement over the OpenDB design database. Today: **legality checking**.

```text
vyges physical dpl check-placement design.odb
```

## Scope — read this before the correctness section

⛔ **`check-placement` only. Legalization (`detailed_placement`) is not implemented**, so this
engine tells you whether a placement is legal and never makes one legal.

The checker came first on purpose. It is the oracle — it needs no placement to be correct in order
to be useful — and upstream's own suite leans on it harder: **77 of 92 `.tcl` tests call
`check_placement`, 68 call `detailed_placement`**.

⚠️ **Three of upstream's nine check families are evaluated.** The other six are listed in
`not_checked` on **every** run, because a clean verdict from a partial checker must not read as a
complete one:

| family | status |
| --- | --- |
| site alignment · placed · overlap | ✅ evaluated |
| in rows · region placement · one-site gaps | ⬜ need the `Grid`/`Pixel` model |
| padding · edge spacing · blocked layers | ⬜ need `PlacementDRC` |

## Correctness

Correlated against OpenROAD at pin `945a9f48dc6e5cc91d865daa92c45a1094cb682c`, **in both
directions** — a checker that cannot fail proves nothing:

| design | reference | this engine |
| --- | --- | --- |
| `aes.defok` (legal) | clean | 21,340 cells, **0 violations** |
| `cell_on_block1.def` (illegal) | *"Site aligned check failed (4)"* | **4** site-align failures |

Two behaviours that are easy to get wrong and are pinned at their sites:

- **Site alignment is core-relative.** Upstream compares `cell->getLeft() % siteWidth`, and
  `getLeft()` is relative to `core_.xMin()`. Read as an absolute coordinate it reports every cell
  of a clean design as misaligned — measured, 21,340 of 21,340.
- **A site-alignment failure removes the cell from the overlap comparison entirely.** That is a
  side effect of upstream's `continue`, not a separate rule: `checkOverlap` is what paints a cell
  into its pixels, so a skipped cell is never there to collide with.

ℹ️ The overlap **acceleration** differs deliberately — a rectangle sweep here, a pixel walk
upstream. The predicate is identical and the failing set matches; which partner is reported can
differ, because upstream names whichever cell already owns the pixel.

## Status

`--describe` reports `maturity: partial`, and a test fails if that word is left behind when the
unchecked families are implemented.

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
