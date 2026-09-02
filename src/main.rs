//! `vyges-dpl` — detailed placement.
use std::process::ExitCode;
use vyges_opendb::Db;

const USAGE: &str = "\
vyges physical dpl — detailed placement: legality checking over the design database

USAGE:
  vyges physical dpl check-placement <design.odb> [--json] [-o FILE]
  vyges physical dpl --describe | --help | --version

⛔ SCOPE: `check-placement` only. `detailed_placement` (legalization) is NOT implemented.
   The checker is first on purpose — it is the oracle, it needs no placement to be correct,
   and upstream's suite calls `check_placement` in 77 of 92 cases against 68 for
   `detailed_placement`.

⚠️ Three of upstream's nine check families are evaluated (site alignment, placed, overlap).
   The other six are reported in `not_checked` rather than passed over in silence.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!("vyges-dpl {} ({})", vyges_dpl::VERSION, env!("VYGES_GIT_SHA"));
            println!("{}", vyges_dpl::COPYRIGHT);
            ExitCode::SUCCESS
        }
        Some("check-placement") => check(&args[1..]),
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
