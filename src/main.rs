// SPDX-License-Identifier: Apache-2.0
//! `vyges-rmp` — logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.
//!
//! ## Exit codes
//!
//! ⛔ **"found something" and "could not run" never share a code.** A CI gate that cannot tell a
//! broken tool from a bad design will eventually read a missing engine as a clean run.
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | ok |
//! | 1 | a finding (the check ran and failed) |
//! | 2 | usage, or input that could not be read |
//! | 3 | the command is NOT IMPLEMENTED in this build |

use std::process::ExitCode;

const USAGE: &str = "\
vyges physical rmp — logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence

USAGE:
  vyges physical rmp run  <design.odb> <job> -o OUT.odb  re-synthesise a region for area or delay
  vyges physical rmp blif-write <design.odb> --liberty FILE... --inst NAME... -o OUT.blif
                         [--const0 CELL PORT] [--const1 CELL PORT]
                                                  the instances as a BLIF cut
  vyges physical rmp blif-read  <design.odb> --liberty FILE... --inst NAME... --blif FILE -o OUT.odb
                         [--const0 CELL PORT] [--const1 CELL PORT] [--call-id N]
                                                  replace the instances by the BLIF's gates
  vyges physical rmp --describe
  vyges physical rmp --help
  vyges physical rmp --version

OPTIONS:
  -o FILE               write the report (or the output database) to FILE
  --json                machine-readable output instead of the text report
  -q, --quiet           suppress non-essential output
  -v, --verbose         extra detail on stderr
  --describe            print a machine-readable JSON description of the command
  --bug-report          file a bug (central: vyges/community)
  --feature-request     request a feature (central)

EXIT STATUS:
  0  ok
  1  a finding — the check ran and failed
  2  usage or unreadable input
  3  refused — this build does not implement the command yet
";

/// The commands this engine will answer. Each is refused (exit 3) until its stage is built.
pub const COMMANDS: &[&str] = &["run", "blif-write", "blif-read"];

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "name": "rmp",
  "summary": "Logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.",
  "maturity": "discovered",
  "provenance_limitations": [
    "`blif-write` and `blif-read` are built (the cut's BLIF, arrivals and requireds not written); `run` refuses with exit 3."
  ],
  "invocation": { "args_template": ["run"], "emits_json": true },
  "inputs": { "type": "object", "properties": {} },
  "artifacts": [],
  "assertion": { "id": "rmp-ok", "field": "status", "pass_when": { "equals": "ok" } },
  "consumes": []
}"#;

fn refuse(what: &str) -> ExitCode {
    vyges_events::emit(
        &vyges_events::Event::new("vyges-rmp", vyges_events::Severity::Error, format!("{what} is not implemented in this build"))
            .with_code("RMP-REFUSED"),
    );
    ExitCode::from(3)
}

fn input_error(msg: &str) -> ExitCode {
    vyges_events::emit(&vyges_events::Event::new("vyges-rmp", vyges_events::Severity::Error, msg.to_string()).with_code("RMP-INPUT"));
    ExitCode::from(2)
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("vyges-rmp: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn refused(why: String) -> ExitCode {
    vyges_events::emit(&vyges_events::Event::new("vyges-rmp", vyges_events::Severity::Error, why).with_code("RMP-REFUSED"));
    ExitCode::from(3)
}

/// What the BLIF commands share: the design, its libraries, the instances, the constant cells.
struct BlifArgs {
    design: String,
    liberty: Vec<String>,
    insts: Vec<String>,
    consts: vyges_rmp::cut::blif::ConstCells,
    blif: Option<String>,
    call_id: i32,
    out: String,
}

fn blif_args(cmd: &str, args: &[String]) -> Result<BlifArgs, ExitCode> {
    let mut a = BlifArgs { design: String::new(), liberty: Vec::new(), insts: Vec::new(), consts: Default::default(), blif: None, call_id: 1, out: String::new() };
    let mut pos = Vec::new();
    let mut it = args.iter();
    while let Some(f) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| usage_error(&format!("{f} needs a value")));
        match f.as_str() {
            "--liberty" => a.liberty.push(value()?),
            "--inst" => a.insts.push(value()?),
            "--blif" => a.blif = Some(value()?),
            "-o" => a.out = value()?,
            "--call-id" => {
                let v = value()?;
                a.call_id = v.parse().map_err(|_| usage_error(&format!("--call-id needs a number, not `{v}`")))?;
            }
            "--const0" => {
                a.consts.const0_cell = value()?;
                a.consts.const0_port = value()?;
            }
            "--const1" => {
                a.consts.const1_cell = value()?;
                a.consts.const1_port = value()?;
            }
            "-q" | "--quiet" | "-v" | "--verbose" => {}
            o if o.starts_with('-') => return Err(usage_error(&format!("unknown option `{o}`"))),
            o => pos.push(o.to_string()),
        }
    }
    match <[String; 1]>::try_from(pos) {
        Ok([d]) if !a.out.is_empty() => a.design = d,
        _ => return Err(usage_error(&format!("{cmd} needs a design database and -o OUT"))),
    }
    Ok(a)
}

/// The libraries as the design sees them: a cell resolves to the FIRST library that has it.
fn read_libraries(files: &[String]) -> Result<vyges_sta::liberty::Library, ExitCode> {
    let mut merged: Option<vyges_sta::liberty::Library> = None;
    for f in files {
        let lib = std::fs::read_to_string(f)
            .map_err(|e| e.to_string())
            .and_then(|text| vyges_sta::liberty_parse::parse(&text))
            .and_then(|g| vyges_sta::liberty::Library::read(&g))
            .map_err(|e| input_error(&format!("cannot read {f}: {e}")))?;
        match &mut merged {
            None => merged = Some(lib),
            Some(m) => {
                for (name, cell) in lib.cells {
                    m.cells.entry(name).or_insert(cell);
                }
            }
        }
    }
    Ok(merged.unwrap_or_default())
}

fn blif_error(e: vyges_rmp::cut::blif::BlifError) -> ExitCode {
    match e {
        vyges_rmp::cut::blif::BlifError::Error(line) => {
            println!("{line}");
            ExitCode::from(1)
        }
        vyges_rmp::cut::blif::BlifError::Refused(why) => refused(why),
    }
}

/// `writeBlif`: the instances as a BLIF file. Log lines are the reference's.
fn blif_write(args: &[String]) -> ExitCode {
    let mut a = match blif_args("blif-write", args) {
        Ok(a) => a,
        Err(c) => return c,
    };
    let lib = match read_libraries(&a.liberty) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let mut db = match vyges_opendb::Db::open(&a.design) {
        Ok(db) => db,
        Err(e) => return input_error(&format!("cannot read {}: {e}", a.design)),
    };
    if let Some(i) = a.insts.iter().find(|i| db.inst_master(i).is_empty()) {
        return input_error(&format!("no instance {i}"));
    }
    let d = vyges_rmp::design::OdbBlif { db: &mut db };
    let w = match vyges_rmp::cut::blif::write_blif(&d, &lib, &a.insts, &mut a.consts) {
        Ok(w) => w,
        Err(e) => return blif_error(e),
    };
    if let Err(e) = std::fs::write(&a.out, &w.text) {
        return input_error(&format!("cannot write {}: {e}", a.out));
    }
    println!("[INFO CUT-0002] Blif writer successfully dumped file with {} instances.", w.instances);
    ExitCode::SUCCESS
}

/// `readBlif`: the instances replaced by the file's gates. Log lines are the reference's.
fn blif_read(args: &[String]) -> ExitCode {
    let a = match blif_args("blif-read", args) {
        Ok(a) => a,
        Err(c) => return c,
    };
    let Some(blif) = &a.blif else { return usage_error("blif-read needs --blif FILE") };
    let text = match std::fs::read(blif) {
        Ok(t) => t,
        Err(e) => return input_error(&format!("cannot read {blif}: {e}")),
    };
    let lib = match read_libraries(&a.liberty) {
        Ok(l) => l,
        Err(c) => return c,
    };
    let mut db = match vyges_opendb::Db::open(&a.design) {
        Ok(db) => db,
        Err(e) => return input_error(&format!("cannot read {}: {e}", a.design)),
    };
    if let Some(i) = a.insts.iter().find(|i| db.inst_master(i).is_empty()) {
        return input_error(&format!("no instance {i}"));
    }
    let mut log = Vec::new();
    let r = {
        let mut d = vyges_rmp::design::OdbBlif { db: &mut db };
        vyges_rmp::cut::blif::read_blif(&mut d, &lib, &a.insts, &a.consts, a.call_id, &text, &mut log)
    };
    for l in &log {
        println!("{l}");
    }
    if let Err(e) = r {
        return blif_error(e);
    }
    if let Err(e) = db.write(&a.out) {
        return input_error(&format!("cannot write {}: {e}", a.out));
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("-V") | Some("--version") => {
            println!("vyges-rmp {} ({})", env!("CARGO_PKG_VERSION"), env!("VYGES_GIT_SHA"));
            println!("Copyright 2026 Vyges. Apache-2.0.");
            ExitCode::SUCCESS
        }
        Some("--describe") => {
            println!("{DESCRIBE}");
            ExitCode::SUCCESS
        }
        Some("blif-write") => blif_write(&args[1..]),
        Some("blif-read") => blif_read(&args[1..]),
        Some(cmd) if COMMANDS.contains(&cmd) => refuse(cmd),
        Some(other) => {
            eprintln!("vyges-rmp: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_is_valid_json_with_a_legal_maturity() {
        let d: serde_json::Value = serde_json::from_str(DESCRIBE).expect("descriptor is valid JSON");
        assert_eq!(d["schema"], "vyges-tool-descriptor/1.1");
        assert_eq!(d["name"], "rmp");
        // Anything else is read as `discovered` and every verdict becomes `unknown`.
        let m = d["maturity"].as_str().unwrap();
        assert!(["discovered", "structured", "workflow-validated"].contains(&m), "{m}");
    }

    #[test]
    fn the_usage_spells_the_engine_under_its_group() {
        // The dispatcher REFUSES a wrong group prefix, so the help must spell the right one.
        for c in COMMANDS {
            assert!(USAGE.contains(&format!("vyges physical rmp {c}")), "{c}");
        }
    }
}
