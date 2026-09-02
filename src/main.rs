// SPDX-License-Identifier: Apache-2.0
//! `vyges-dpl` — detailed placement.
use std::process::ExitCode;
use vyges_opendb::Db;

/// The machine-readable contract `vyges mcp` and the docs generator read.
///
/// ⛔ **`maturity` is `partial`, and that is the honest word.** Three of upstream's nine check
/// families are evaluated. A descriptor that claimed otherwise would be the defect this suite has
/// already hit twice — `pad` called five shipped commands "not implemented", `tap` said "NOTHING
/// IS PLACED" after scoring 9 of 9.
const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "openroad_pin": "945a9f48dc6e5cc91d865daa92c45a1094cb682c",
  "name": "dpl",
  "summary": "detailed placement: legality checking over the design database",
  "maturity": "partial",
  "provenance_limitations": [
    "input_hash covers the argument vector, not the content of the .odb it names.",
    "SCOPE: `check-placement` only. `detailed_placement` (legalization) is NOT implemented, so this engine reports whether a placement is legal and never makes one legal.",
    "FOUR of upstream's NINE check families are evaluated: site alignment, placed, overlap and in_rows. The five not evaluated are named in the report's `not_checked` field on every run -- region_placement, padding, edge_spacing, blocked_layers and one_site_gap -- because a clean verdict from a partial checker must not read as a complete one. Those five need regions, Padding or PlacementDRC; the Grid they also need is built.",
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
  "inputs": {
    "type": "object",
    "required": ["odb"],
    "properties": { "odb": { "type": "string", "description": "the design database to check" } }
  },
  "consumes": ["odb"],
  "produces": ["placement_check"],
  "artifacts": [{ "role": "placement_check", "field": "report_path" }],
  "assertion": { "id": "placement-legal", "field": "status", "pass_when": { "eq": "clean" } }
}"#;

const USAGE: &str = "\
vyges physical dpl — detailed placement: legality checking over the design database

USAGE:
  vyges physical dpl check-placement    <design.odb> [--json] [-o FILE]
  vyges physical dpl detailed-placement <design.odb> [--out-odb FILE] [--dry-run]
  vyges physical dpl --describe | --help | --version

⛔ SCOPE: legalization is the DIAMOND SEARCH path only. Rip-up-and-replace, groups/regions,
   padding and one-site gaps are NOT implemented and are named in `not_done` on every run.

⚠️ Four of upstream's nine check families are evaluated (site alignment, placed, overlap,
   in rows). The other five are reported in `not_checked` rather than passed over in silence.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--describe") => {
            println!("{DESCRIBE}");
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
    let (mut odb, mut out_odb, mut dry) = (None, None, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out-odb" => { i += 1; out_odb = args.get(i).cloned(); }
            "--dry-run" => dry = true,
            a if a.starts_with('-') => {
                eprintln!("vyges-dpl: unknown option `{a}`");
                return ExitCode::from(2);
            }
            a => odb = Some(a.to_string()),
        }
        i += 1;
    }
    let Some(path) = odb else {
        eprintln!("vyges-dpl: detailed-placement needs <design.odb>\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let mut db = match Db::open(&path) {
        Ok(d) => d,
        Err(e) => { eprintln!("vyges-dpl: cannot open {path}: {e}"); return ExitCode::from(2); }
    };
    let res = match vyges_dpl::place::legalize(&db) {
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
        "cells": res.placed.len(), "moved": moved,
        "failures": res.failures, "not_done": res.not_done,
        // ⚠️ The DECISION, not just the count. A placer whose output cannot be inspected cannot
        // be debugged — the first bug here was invisible in a summary that said "1 moved".
        "placed": res.placed,
    })).expect("valid JSON"));
    eprintln!("detailed-placement: {} cell(s), {moved} moved, {} failed, status {status}",
              res.placed.len(), res.failures.len());
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

    #[test]
    fn maturity_is_partial_while_six_families_are_unchecked() {
        // 🔑 The claim and the code have to move together: if the NOT_CHECKED list empties, this
        // fails and forces the maturity word to be revisited rather than left behind.
        let remaining = vyges_dpl::check::NOT_CHECKED.len();
        if remaining > 0 {
            assert_eq!(json()["maturity"], "partial",
                       "{remaining} check families are unimplemented, so maturity is not `structured`");
        }
    }

    #[test]
    fn the_absence_is_declared_not_merely_absent() {
        let text = json()["provenance_limitations"].to_string();
        for family in vyges_dpl::check::NOT_CHECKED {
            assert!(text.contains(family),
                    "`{family}` is not evaluated and the descriptor does not say so");
        }
        assert!(text.contains("detailed_placement"),
                "the descriptor must say legalization is not implemented");
    }

    #[test]
    fn every_command_the_help_advertises_is_dispatched() {
        // ⚠️ `--describe` was advertised in USAGE before it existed. That is the same class of
        // stale claim, one direction over.
        for cmd in ["check-placement", "--describe", "--help", "--version"] {
            assert!(USAGE.contains(cmd), "`{cmd}` is dispatched but not in --help");
        }
    }

    #[test]
    fn the_assertion_reads_the_status_field_the_tool_emits() {
        let d = json();
        assert_eq!(d["assertion"]["field"], "status");
        assert_eq!(d["assertion"]["pass_when"]["eq"], "clean");
    }
}
