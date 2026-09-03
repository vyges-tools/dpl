// SPDX-License-Identifier: Apache-2.0
//! `vyges-dpl` — detailed placement.
use std::process::ExitCode;
use vyges_opendb::Db;

/// The machine-readable contract `vyges mcp` and the docs generator read.
///
/// ⛔ **`maturity` has exactly THREE legal values** — `discovered`, `structured`,
/// `workflow-validated` — and `partial`, which this said until 2026-09-02, is not one of them.
/// `Maturity::parse` returns `None` for an unknown word and the JSON schema's `enum` rejects it,
/// so an invalid maturity does not read as a modest claim: it degrades to `discovered`, where
/// `can_assert()` is false and **the verdict is suppressed to `unknown` however well-formed the
/// assertion is**. A word chosen to be humble silently threw the result away.
///
/// 🔑 **The rung describes the shape of the EVIDENCE, not feature completeness.** `structured` is
/// "publishes a versioned operation and a normalized result"; `workflow-validated` additionally
/// requires a pinned design in-repo that the test suite runs end to end and asserts against. This
/// engine's correlation harness lives outside the repository, so `structured` is the honest rung.
/// What is unbuilt goes in `provenance_limitations`, which is required and can carry nuance.
///
/// ⚠️ A descriptor that outlives the truth is this suite's recurring defect — `pad` called five
/// shipped commands "not implemented", `tap` said "NOTHING IS PLACED" after scoring 9 of 9, and
/// this one said legalization was unimplemented while it matched the reference on every case.
///
/// ⚠️ **And it hit this engine too, in the OTHER direction**: this descriptor said
/// *"`detailed_placement` (legalization) is NOT implemented ... never makes one legal"* while
/// legalization was matching the reference on every comparable case. ⟹ **A descriptor rots
/// toward whatever was true when it was written**, understating as readily as overstating, and
/// nothing fails when it does. It is read by `vyges mcp` and rendered verbatim into the docs.
/// The pin, inherited from the crate every engine already depends on.
const CRATE_PIN: &str = vyges_opendb::OPENROAD_PIN;

/// The pin this binary was built against, injected into the descriptor at print time.
///
/// 🔑 **One definition for the whole programme, inherited rather than typed** — the SHA lives in
/// `openroad-pin.yaml` in `vyges-opendb-lib` and reaches here through `vyges-opendb`.
///
/// ⛔ **`dpl` was the ONE engine of eight still spelling the pin out as a literal**, and the
/// 2026-09-03 re-pin is how that surfaced: rebuilt against `7d490b8`, this binary went on
/// reporting `945a9f4` because the string is what it prints, not what it links. The other seven
/// had used this token since 2026-08-29. ⟹ **A self-reported pin that cannot disagree with the
/// build reports nothing** — `--pins` compares this against the oracle a harness is about to
/// launch, and a hand-typed constant makes that comparison vacuous.
const PIN_TOKEN: &str = "@OPENROAD_PIN@";

fn describe() -> String {
    DESCRIBE.replace(PIN_TOKEN, CRATE_PIN)
}

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "openroad_pin": "@OPENROAD_PIN@",
  "name": "dpl",
  "summary": "detailed placement: legality checking and legalization over the design database",
  "maturity": "structured",
  "provenance_limitations": [
    "input_hash covers the argument vector, not the content of the .odb it names.",
    "LEGALIZATION is implemented and is the default path: the NEGOTIATION legalizer, which is what upstream's `detailed_placement` runs when `-use_diamond_legalizer` is absent. `--use-diamond-legalizer` selects the other one. The two produce DIFFERENT placements, so the report names which ran.",
    "Correlated at pin 7d490b8ecd357199c0c0e9f3e32becd5eb507c34: `detailed_placement` matches the reference on 28 of 28 comparable cases from its own regression suite, including aes (21340 components), ibex (34184) and gcd (549). The agreement is SWEEP-LEVEL, not final-placement only -- upstream's per-iteration debug trace and this engine's match line for line, every cell, every iteration, on simple05, simple07, gcd (574 lines) and hybrid_cells.",
    "It read 18 of 28 when the pin first moved from 945a9f48dc6e5cc91d865daa92c45a1094cb682c, because upstream reworked the negotiation legalizer's INITIAL SNAPPING across 7 commits and refreshed 24 of its own goldens. Four mechanisms were transcribed to recover it: `Grid::gridRoundX` (initial x ROUNDS to the nearest site rather than truncating), `displacementInSites`/`rowDispInSites` replacing the deleted `NegCell::displacement()` so displacement is site widths on BOTH axes, `initialSnap()`'s diamond search in place of a four-direction scan, and -- the one that mattered most -- running that snap as its OWN pass after every fixed cell is blockaded, testing grid CAPACITY rather than merely whether a site exists.",
    "BUT 35 of upstream's 63 `detailed_placement` cases are OUTSIDE that number and are not scored at all: 12 ship no golden, 8 need filler placement, 7 declare REGIONS/GROUPS, 7 need placement padding values, 1 needs both. `18 of 28` is a claim about what the corpus asks, not about every design.",
    "AND THE DENOMINATOR IS SHORT: upstream ships 69 .tcl cases calling `detailed_placement`, not 63. Five of them -- report_failures, fragmented_row03, pad02, fillers8, obstruction2 -- wrap the call as `catch { detailed_placement }`, so the harness's command filter does not see them and they are scored by nothing. All five are error-path cases.",
    "Two of `countDRCViolations` four terms are NOT evaluated -- `checkEdgeSpacing` (needs each master's LEF58 cell-edge list) and `checkBlockedLayers`. Nothing in the comparable corpus exercises either, so their absence is invisible to the score rather than proven harmless.",
    "Every instance the model filter excluded is named and counted in `filtered_out` on every run. A filter that drops instances silently is indistinguishable from a design that has none of them.",
    "`-disallow_one_site_gaps` is NOT accepted: upstream deprecated it (DPL-3/DPL-4) and derives the setting from `hasOneSiteMaster()`, so the flag cannot change the result. `-incremental` is not implemented and is named in `not_done`.",
    "SEVEN of upstream's NINE check families are evaluated: site alignment, placed, overlap, in_rows, padding, blocked_layers and one_site_gap. What is NOT evaluated is named in the report's `not_checked` field on every run -- region_placement needs regions, edge_spacing needs each master's LEF58 cell-edge list -- because a clean verdict from a partial checker must not read as a complete one. Families that ran under a restriction are named in `limitations` rather than left to be inferred.",
    "Site alignment is CORE-RELATIVE: upstream compares `cell->getLeft() % siteWidth` where getLeft() is relative to core_.xMin(). Measured on aes.defok, reading it as an absolute coordinate reports every one of 21340 cells misaligned on a design the reference calls clean.",
    "A site-alignment failure removes the cell from the overlap comparison entirely. That is a side effect of upstream's `continue`, not a separate rule: checkOverlap is what paints a cell into its pixels, so a cell that was skipped is never there for a later cell to collide with.",
    "The OVERLAP ACCELERATION differs from upstream deliberately: a rectangle sweep here, a pixel walk there. The predicate is identical and the failing SET matches, but which partner is reported can differ, because upstream reports whichever cell already owns the pixel and that depends on visit order.",
    "status is one of clean, violations, vacuous or error. VACUOUS IS NOT CLEAN: it means the run examined no cell, and a design with no instances is an absent placement rather than a legal one.",
    "Correlated at pin 945a9f48dc6e5cc91d865daa92c45a1094cb682c in both directions on three designs: aes.defok gives 21340 cells and 0 violations against the reference's clean verdict; cell_on_block1.def gives 4 site-align failures against `Site aligned check failed (4)`; fragmented_row04.def gives 1 in_rows failure against `Placed in rows check failed (1)`."
  ],
  "invocation": {
    "args_template": ["check-placement", "{odb}"],
    "optional": [{ "arg": "out", "flag": "-o" }],
    "emits_json": true
  },
  "commands": [
    {
      "name": "check-placement",
      "summary": "report whether a placement is legal, family by family",
      "args_template": ["check-placement", "{odb}"],
      "optional": [{ "arg": "out", "flag": "-o" }],
      "assertion": { "id": "placement-legal", "field": "status", "pass_when": { "eq": "clean" } }
    },
    {
      "name": "detailed-placement",
      "summary": "legalize the placement -- negotiated congestion by default",
      "args_template": ["detailed-placement", "{odb}"],
      "optional": [
        { "arg": "out_odb", "flag": "--out-odb", "description": "write the legalized database here" },
        { "arg": "dry_run", "flag": "--dry-run", "description": "legalize and report, write nothing" },
        { "arg": "use_diamond_legalizer", "flag": "--use-diamond-legalizer", "description": "use the diamond search instead of negotiation" },
        { "arg": "max_displacement", "flag": "--max-displacement", "description": "cap the move at N sites and M rows, 'N' or 'N,M' (default 500,100)" },
        { "arg": "site_search_window", "flag": "--site-search-window", "description": "base search width along the row, in sites (default 20)" },
        { "arg": "row_search_window", "flag": "--row-search-window", "description": "base search height, in rows (default 5)" },
        { "arg": "drc_penalty", "flag": "--drc-penalty", "description": "cost added per DRC violation at a candidate site (default 5)" },
        { "arg": "disable_window_extension", "flag": "--disable-window-extension", "description": "do not widen the search past a macro or a wall" }
      ],
      "assertion": { "id": "placement-legalized", "field": "status", "pass_when": { "eq": "legalized" } }
    }
  ],
  "inputs": {
    "type": "object",
    "required": ["odb"],
    "properties": {
      "odb": { "type": "string", "description": "the design database to check or legalize" },
      "out_odb": { "type": "string", "description": "write the legalized database here" },
      "max_displacement": { "type": "string", "description": "move cap, 'SITES' or 'SITES,ROWS'" },
      "site_search_window": { "type": "integer", "description": "base search width in sites" },
      "row_search_window": { "type": "integer", "description": "base search height in rows" },
      "drc_penalty": { "type": "number", "description": "cost per DRC violation at a candidate" },
      "out": { "type": "string", "description": "write the report to FILE instead of stdout" }
    }
  },
  "consumes": ["odb"],
  "produces": ["placement_check", "odb"],
  "artifacts": [
    { "role": "placement_check", "field": "report_path" },
    { "role": "odb", "field": "out_odb" }
  ],
  "assertion": { "id": "placement-legal", "field": "status", "pass_when": { "eq": "clean" } }
}"#;

const USAGE: &str = "\
vyges physical dpl — detailed placement: legality checking and legalization over the design database

USAGE:
  vyges physical dpl check-placement    <design.odb> [--json] [-o FILE]
  vyges physical dpl detailed-placement <design.odb> [--out-odb FILE] [--dry-run] [OPTIONS]
  vyges physical dpl --describe | --help | --version

OPTIONS:
  --out-odb FILE            write the legalized database here (default: nothing is written)
  --dry-run                 legalize and report, write no database
  --use-diamond-legalizer   use the diamond search instead of negotiation (upstream's flag)
  --max-displacement N[,M]  cap the move at N sites and M rows (default: 500,100)
  --site-search-window N    base search width along the row, in sites (default: 20)
  --row-search-window N     base search height, in rows (default: 5)
  --drc-penalty F           cost added per DRC violation at a candidate site (default: 5)
  --disable-window-extension  do not widen the search window past a macro or a wall
  -o FILE                   write the report to FILE instead of stdout
  --json                    emit JSON (the default)
  --describe                print a machine-readable JSON description of the command

EXIT STATUS:
  0  legalized   every cell was seated; the database was written unless --dry-run
  0  clean       check-placement found no violation
  0  vacuous     the run placed nothing -- NOT a completed legalization; read the count
  1  failed      a cell could not be seated, or a check family found violations
  2  error       usage error, an unreadable database, or a failed write

⛔ SCOPE: legalization runs the NEGOTIATION legalizer, which is upstream's default path;
   `--use-diamond-legalizer` selects the diamond one, as upstream's own flag does. Whichever
   runs, what it does NOT implement is named in `not_done` on every run rather than omitted,
   and every instance the model filter excluded is named in `filtered_out`.

⚠️ Seven of upstream's nine check families are evaluated. `region_placement` and `edge_spacing`
   are reported in `not_checked` rather than passed over in silence, and a family that ran under
   a restriction says so in `limitations`.

ℹ️ `-disallow_one_site_gaps` has no equivalent here ON PURPOSE: upstream deprecated it and
   derives the setting from `hasOneSiteMaster()`, so the flag cannot change the result.
   `-incremental` is not implemented and is named in `not_done`.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--describe") => {
            println!("{}", describe());
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!("vyges-dpl {} ({})", vyges_dpl::VERSION, env!("VYGES_GIT_SHA"));
            println!("{}", vyges_dpl::COPYRIGHT);
            ExitCode::SUCCESS
        }
        Some("check-placement") => check(&args[1..]),
        Some("detailed-placement") => legalize(&args[1..]),
        // A diagnostic, not a command anyone should need in a flow.
        Some("grid-facts") => {
            let Some(p) = args.get(1) else { eprintln!("need <design.odb>"); return ExitCode::from(2) };
            match Db::open(p) {
                Ok(db) => {
                    println!("{}", serde_json::to_string_pretty(&vyges_dpl::check::grid_facts(&db)).unwrap());
                    ExitCode::SUCCESS
                }
                Err(e) => { eprintln!("cannot open {p}: {e}"); ExitCode::from(2) }
            }
        }
        Some(other) => {
            eprintln!("vyges-dpl: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn legalize(args: &[String]) -> ExitCode {
    // ⚠️ `use_diamond_legalizer_` DEFAULTS TO FALSE upstream and
    // `isUseNegotiationLegalizer()` is `!use_diamond_legalizer_` — so NEGOTIATION is the default
    // path and the diamond one is opt-in (4 of 67 upstream cases pass `-use_diamond_legalizer`).
    let (mut odb, mut out_odb, mut dry, mut diamond) = (None, None, false, false);
    // ⛔ Defaults are upstream's, in `negotiate::Options` — not repeated here, so there is one
    // place to be wrong about them.
    let mut opts = vyges_dpl::negotiate::Options::default();
    // `-max_displacement disp|{disp_x disp_y}`: ONE value sets both axes, two set them
    // separately. Upstream accepts a Tcl list; the shell equivalent is `X` or `X,Y`.
    let mut num = |i: &mut usize, what: &str| -> Option<String> {
        *i += 1;
        match args.get(*i) {
            Some(v) => Some(v.clone()),
            None => { eprintln!("vyges-dpl: {what} needs a value"); None }
        }
    };
    let mut bad = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out-odb" => { i += 1; out_odb = args.get(i).cloned(); }
            "--dry-run" => dry = true,
            "--use-diamond-legalizer" => diamond = true,
            "--disable-window-extension" => opts.disable_window_extension = true,
            "--max-displacement" => match num(&mut i, "--max-displacement") {
                None => bad = true,
                Some(v) => {
                    let (a, b) = v.split_once(',').unwrap_or((v.as_str(), v.as_str()));
                    match (a.trim().parse::<i32>(), b.trim().parse::<i32>()) {
                        (Ok(x), Ok(y)) if x >= 0 && y >= 0 => {
                            opts.max_displacement_x = x;
                            opts.max_displacement_y = y;
                        }
                        _ => {
                            eprintln!("vyges-dpl: --max-displacement wants SITES or SITES,ROWS \
                                       (non-negative), got `{v}`");
                            bad = true;
                        }
                    }
                }
            },
            o @ ("--site-search-window" | "--row-search-window") => {
                match num(&mut i, o).map(|v| (v.trim().parse::<i32>(), v)) {
                    Some((Ok(n), _)) if n >= 0 => {
                        if o == "--site-search-window" { opts.site_search_window = n }
                        else { opts.row_search_window = n }
                    }
                    Some((_, v)) => {
                        eprintln!("vyges-dpl: {o} wants a non-negative integer, got `{v}`");
                        bad = true;
                    }
                    None => bad = true,
                }
            }
            "--drc-penalty" => match num(&mut i, "--drc-penalty").map(|v| (v.trim().parse::<f64>(), v)) {
                Some((Ok(n), _)) if n >= 0.0 => opts.drc_penalty = n,
                Some((_, v)) => {
                    eprintln!("vyges-dpl: --drc-penalty wants a non-negative number, got `{v}`");
                    bad = true;
                }
                None => bad = true,
            },
            // ⚠️ Accepted and REFUSED rather than silently ignored. Upstream deprecated it
            // (DPL-3) because the value is derived from `hasOneSiteMaster`; a flag that cannot
            // change the answer must not look as though it did.
            "--disallow-one-site-gaps" => {
                eprintln!("vyges-dpl: --disallow-one-site-gaps is not accepted: upstream \
                           deprecated it and derives the setting from hasOneSiteMaster(), so \
                           the flag cannot change the result");
                bad = true;
            }
            a if a.starts_with('-') => {
                eprintln!("vyges-dpl: unknown option `{a}`");
                return ExitCode::from(2);
            }
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }
    if bad {
        return ExitCode::from(2);
    }
    let Some(path) = odb else {
        eprintln!("vyges-dpl: detailed-placement needs <design.odb>\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let mut db = match Db::open(&path) {
        Ok(d) => d,
        Err(e) => { eprintln!("vyges-dpl: cannot open {path}: {e}"); return ExitCode::from(2); }
    };
    // ⚠️ The tunables belong to the NEGOTIATION legalizer — upstream's setters are on
    // `NegotiationLegalizer`, and `diamondDPL` reads only the displacement caps.
    let res = match if diamond { vyges_dpl::place::legalize(&db) }
                    else { vyges_dpl::negotiate::legalize_with(&db, opts) } {
        Ok(r) => r,
        Err(e) => { eprintln!("vyges-dpl: {e}"); return ExitCode::from(2); }
    };
    let moved = res.placed.iter().filter(|p| p.moved).count();
    if !dry {
        for p in &res.placed {
            // ⛔ **ORIENTATION FIRST, THEN LOCATION**, and the order is not a preference.
            // `dbInst::setOrient` re-derives the origin so the cell's bbox stays put, so setting
            // it AFTER a move undoes the move. Measured: the planner chose y=2800 and the written
            // database read back y=0, while a location-only write round-tripped exactly.
            //
            // 🔑 Upstream's `updateDbInstLocations` does orient, then location, and guards each
            // with an "only if it differs" test — the comment there says it is to avoid
            // triggering callbacks. This engine copies the ORDER for correctness and the GUARD
            // because it is free.
            //
            // ⚠️ The `tap` engine hit this exact bug: its applier was right about where cells go
            // and wrong about the order it wrote them in.
            if db.inst_get_orient(&p.name) != p.orient {
                let _ = db.set_inst_orient(&p.name, &p.orient);
            }
            if db.inst_location(&p.name) != (p.x, p.y) {
                if let Err(e) = db.set_inst_location(&p.name, p.x, p.y) {
                    eprintln!("vyges-dpl: cannot move {}: {e}", p.name);
                    return ExitCode::from(2);
                }
            }
            let _ = db.inst_set_placement_status(&p.name, "PLACED");
        }
        if let Some(o) = out_odb.as_deref() {
            if let Err(e) = db.write(o) {
                eprintln!("vyges-dpl: cannot write {o}: {e}");
                return ExitCode::from(2);
            }
        }
    }
    // ⛔ Upstream errors DPL-36 when any cell could not be seated. A legalizer that reports
    // success having left cells illegal is worse than one that fails.
    let status = if !res.failures.is_empty() { "failed" }
                 else if res.placed.is_empty() { "vacuous" } else { "legalized" };
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "tool": "vyges-dpl", "command": "detailed-placement", "status": status,
        // 🔑 Which legalizer ran is part of the result — the two produce different placements
        // and a report that does not say which one it was cannot be compared to anything.
        "legalizer": if diamond { "diamond" } else { "negotiation" },
        "cells": res.placed.len(), "moved": moved,
        "failures": res.failures, "not_done": res.not_done,
        // ⛔ **What the MODEL FILTER dropped, named and counted.** An instance excluded here has
        // no cell, no blockade and no capacity, so every number above is computed as though the
        // design did not contain it. Measured on `gcd`: 255 tap cells silently left the model
        // because their master type arrives spelled `CORE WELLTAP`, not `CORE_WELLTAP`.
        "filtered_out": res.filtered_out,
        // ⚠️ The DECISION, not just the count. A placer whose output cannot be inspected cannot
        // be debugged — the first bug here was invisible in a summary that said "1 moved".
        "placed": res.placed,
    })).expect("valid JSON"));
    eprintln!("detailed-placement: {} cell(s), {moved} moved, {} failed, status {status}",
              res.placed.len(), res.failures.len());
    if !res.filtered_out.is_empty() {
        let n: usize = res.filtered_out.values().sum();
        eprintln!("detailed-placement: {n} instance(s) outside the model: {:?}", res.filtered_out);
    }
    match status { "legalized" => ExitCode::SUCCESS, "vacuous" => ExitCode::from(3),
                   _ => ExitCode::from(1) }
}

fn check(args: &[String]) -> ExitCode {
    let mut odb = None;
    let mut out = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {}
            "-o" => {
                i += 1;
                out = args.get(i).cloned();
            }
            a if a.starts_with('-') => {
                eprintln!("vyges-dpl: unknown option `{a}`");
                return ExitCode::from(2);
            }
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }
    let Some(path) = odb else {
        eprintln!("vyges-dpl: check-placement needs <design.odb>\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let db = match Db::open(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("vyges-dpl: cannot open {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let report = vyges_dpl::check::check_placement(&db);
    // ⛔ `vacuous` is reserved across this suite: a run that examined no cell must not report
    // clean. A design with no instances is not a legal placement, it is an absent one.
    let status = if report.cells_checked == 0 {
        "vacuous"
    } else if report.is_clean() {
        "clean"
    } else {
        "violations"
    };
    let json = serde_json::json!({
        "tool": "vyges-dpl",
        "command": "check-placement",
        "status": status,
        "cells_checked": report.cells_checked,
        "violations": report.failures.len(),
        "not_checked": report.not_checked,
        // ⛔ **Emitted, because a field the command drops might as well not exist.** These are the
        // families that DID run but could not see everything — the reader who treats "checked" as
        // "checked completely" is exactly who this is for.
        "limitations": report.limitations,
        "failures": report.failures,
    });
    let text = serde_json::to_string_pretty(&json).expect("the report is valid JSON");
    match out {
        Some(p) => {
            if let Err(e) = std::fs::write(&p, format!("{text}\n")) {
                eprintln!("vyges-dpl: cannot write {p}: {e}");
                return ExitCode::from(2);
            }
        }
        None => println!("{text}"),
    }
    eprintln!("check-placement: {} cell(s), {} violation(s), status {status}",
              report.cells_checked, report.failures.len());
    // Upstream errors (DPL-33) when any family has failures; the exit code is the verdict.
    match status {
        "clean" => ExitCode::SUCCESS,
        "vacuous" => ExitCode::from(3),
        _ => ExitCode::from(1),
    }
}

#[cfg(test)]
mod descriptor_tests {
    //! ⛔ **A descriptor that outlives the truth is this suite's recurring defect.** `pad` called
    //! five shipped commands "not implemented"; `tap` said "NOTHING IS PLACED" after scoring 9 of
    //! 9. Both were caught by a person reading, which is not a gate. This is the gate.
    use super::{DESCRIBE, USAGE};

    fn json() -> serde_json::Value {
        serde_json::from_str(DESCRIBE).expect("--describe must be valid JSON")
    }

    #[test]
    fn the_descriptor_parses_and_carries_the_contract_fields() {
        let d = json();
        for k in ["schema", "openroad_pin", "name", "summary", "maturity",
                  "provenance_limitations", "invocation", "consumes", "artifacts", "assertion"] {
            assert!(d.get(k).is_some(), "descriptor is missing `{k}`");
        }
        assert_eq!(d["name"], "dpl");
    }

    /// ⛔ **This test is the one `dpl` did not have**, and the 2026-09-03 re-pin is what it cost:
    /// rebuilt against `7d490b8`, the binary went on reporting `945a9f4` because the field was a
    /// hand-typed literal. ⟹ **A self-reported pin that cannot disagree with the build reports
    /// nothing**, and `--pins` compares exactly this field against the oracle a harness is about
    /// to launch.
    ///
    /// ⚠️ It guards the FIELD, not the prose. A correlation claim names the commit it was
    /// MEASURED at and must stay a literal — `945a9f4` in a limitation is correct and must not
    /// become the token, or the claim silently re-asserts itself at every future pin.
    #[test]
    fn the_descriptor_reports_the_pin_this_binary_was_built_against() {
        let d = super::describe();
        assert!(
            !d.contains(super::PIN_TOKEN),
            "the pin placeholder survived into the output -- the substitution did not run"
        );
        let v: serde_json::Value =
            serde_json::from_str(&d).expect("the descriptor is still valid JSON once filled in");
        assert_eq!(
            v["openroad_pin"], super::CRATE_PIN,
            "the descriptor must report the pin this binary was actually built against"
        );
        assert_eq!(super::CRATE_PIN.len(), 40, "a full commit SHA, not an abbreviation");
    }

    /// The `openroad_pin` FIELD specifically must never be a literal — that is the regression.
    #[test]
    fn the_openroad_pin_field_is_the_token_not_a_literal() {
        let raw: serde_json::Value = serde_json::from_str(DESCRIBE)
            .expect("the raw descriptor is valid JSON before substitution");
        assert_eq!(
            raw["openroad_pin"], super::PIN_TOKEN,
            "openroad_pin must be {} in the source so it tracks the build",
            super::PIN_TOKEN
        );
    }

    #[test]
    fn maturity_is_one_of_the_three_legal_rungs() {
        // ⛔ **The ladder is a closed enum**, and this test exists because the previous one
        // asserted `partial` — a word that is not on it. `Maturity::parse` returns `None` for an
        // unknown value and the consumer then treats the engine as `discovered`, where a verdict
        // is suppressed to `unknown` however well-formed the assertion is. ⟹ **An invalid
        // maturity is not a modest claim, it is a discarded result.**
        let m = json()["maturity"].as_str().unwrap_or_default().to_string();
        assert!(["discovered", "structured", "workflow-validated"].contains(&m.as_str()),
                "`{m}` is not a legal maturity; an unrecognised one degrades to `discovered` \
                 and suppresses the verdict");
        // 🔑 And the rung must not overstate the EVIDENCE. `workflow-validated` requires a pinned
        // design in-repo that the suite runs end to end and asserts against; this engine's
        // correlation harness lives elsewhere, so claiming it here would be false.
        assert_ne!(m, "workflow-validated",
                   "no in-repo end-to-end fixture asserts against a pinned golden");
        // ⚠️ What is UNBUILT belongs in the limitations, not in the rung — and it must be there.
        let unbuilt = vyges_dpl::check::NOT_CHECKED.len() + vyges_dpl::negotiate::NOT_DONE.len();
        if unbuilt > 0 {
            assert!(!json()["provenance_limitations"].as_array().unwrap().is_empty(),
                    "{unbuilt} families are unbuilt and the descriptor states no limitation");
        }
    }

    #[test]
    fn the_absence_is_declared_not_merely_absent() {
        let text = json()["provenance_limitations"].to_string();
        for family in vyges_dpl::check::NOT_CHECKED {
            assert!(text.contains(family),
                    "`{family}` is not evaluated and the descriptor does not say so");
        }
    }

    #[test]
    fn the_descriptor_does_not_understate_what_the_engine_does() {
        // ⛔ **This engine's descriptor rotted in the UNDERSTATING direction**, which the suite
        // had only ever seen the other way round. It said *"`detailed_placement` (legalization)
        // is NOT implemented ... never makes one legal"* for as long as legalization was
        // matching the reference on every comparable case — and the test that was supposed to
        // guard the descriptor ASSERTED that sentence, so it aged with it.
        //
        // 🔑 ⟹ **A guard written as "the descriptor must say we cannot do X" becomes a guard
        // that we never do X.** Pin what IS true and let the negative claims fall out.
        let text = json()["provenance_limitations"].to_string();
        assert!(text.contains("LEGALIZATION is implemented"),
                "legalization is implemented and the descriptor must say so");
        for stale in ["is NOT implemented", "never makes one legal"] {
            assert!(!text.contains(stale), "the descriptor still carries the stale claim `{stale}`");
        }
        // ⚠️ And the number must not be quotable without its caveat: 28 of 28 is a claim about
        // what the corpus asks, and 35 of its 63 cases are not scored at all.
        assert!(text.contains("28 of 28"), "the correlation result belongs in the contract");
        assert!(text.contains("35 of upstream's 63"),
                "the corpus caveat must travel with the number it qualifies");
    }

    #[test]
    fn every_command_the_help_advertises_is_dispatched() {
        // ⚠️ `--describe` was advertised in USAGE before it existed. That is the same class of
        // stale claim, one direction over.
        for cmd in ["check-placement", "detailed-placement", "--describe", "--help", "--version"] {
            assert!(USAGE.contains(cmd), "`{cmd}` is dispatched but not in --help");
        }
    }

    #[test]
    fn every_option_the_parser_accepts_is_documented() {
        // ⛔ An option that works and is undocumented is as bad as one that is documented and
        // does not: the generated CLI reference page IS `--help`, verbatim.
        for flag in ["--out-odb", "--dry-run", "--use-diamond-legalizer", "--max-displacement",
                     "--site-search-window", "--row-search-window", "--drc-penalty",
                     "--disable-window-extension", "-o", "--json"] {
            assert!(USAGE.contains(flag), "`{flag}` is accepted but not in --help");
        }
        // 🔑 And the two upstream options deliberately NOT offered must say why they are absent,
        // or the next reader will file their absence as an oversight.
        assert!(USAGE.contains("disallow_one_site_gaps") && USAGE.contains("incremental"),
                "an option refused on purpose must be documented as refused");
    }

    #[test]
    fn the_assertion_reads_the_status_field_the_tool_emits() {
        let d = json();
        assert_eq!(d["assertion"]["field"], "status");
        assert_eq!(d["assertion"]["pass_when"]["eq"], "clean");
    }
}
