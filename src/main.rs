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

use vyges_rmp::events;

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
  vyges physical rmp restructure <design.odb> --liberty FILE... --sdc FILE --liberty-file FILE -o OUT.odb
                         [--target area|timing] [--tielo-port CELL/PORT] [--tiehi-port CELL/PORT]
                         [--work-dir DIR] [--abc-logfile FILE] [--slack-threshold S] [--depth-threshold N]
                         [--set-wire-rc \"ARGS\"] [--routing-alpha A]
                                                  the design's logic re-synthesised through ABC
                                                  (--set-wire-rc: the placement estimate's wire RC)
  vyges physical rmp resynth <design.odb> --liberty [CORNER=]FILE... --sdc FILE -o OUT.odb
                         [--corners A,B,...] [--corner C] [--name-index N]
                         [--dont-use PAT] [--unset-dont-use PAT] [--reset-dont-use]
                         [--set-wire-rc \"ARGS\"] [--routing-alpha A]
                                                  the logic behind every negative-slack end point
                                                  re-mapped by ABC for delay (the dont-use options
                                                  apply in order after the Liberty dont_use cells)
  vyges physical rmp resynth-annealing <design.odb> (resynth's options)
                         [--iters N] [--revert-after N] [--seed S] [--temp T] [--initial-ops N]
                         [--slack-threshold S]
                                                  a simulated-annealing search over AIG rewrite
                                                  sequences for the corner's best worst slack
  vyges physical rmp resynth-genetic <design.odb> (resynth's options)
                         [--iters N] [--seed S] [--population-size N] [--mutation-probability P]
                         [--crossover-probability P] [--tournament-size N]
                         [--tournament-probability P] [--initial-ops N] [--slack-threshold S]
                                                  a genetic search over the same sequences
  vyges physical rmp --describe
  vyges physical rmp --help
  vyges physical rmp --version

OPTIONS:
  -o FILE               write the report (or the output database) to FILE
  --json                machine-readable output instead of the text report
  -q, --quiet           raise the causal trail's floor to error (VYGES_LOG)
  -v, --verbose         lower it to debug: each step of the command
  --describe            print a machine-readable JSON description of the command
  --bug-report          file a bug (central: vyges/community)
  --feature-request     request a feature (central)

EVENTS:
  The causal trail (vyges-events JSON lines) goes to stderr; reports and log lines go to stdout.
  RMP-INPUT, RMP-REFUSED, RMP-STEP (debug) and RMP-DONE (every command: its status and exit
  code). An explicit VYGES_LOG wins over -q and -v.

EXIT STATUS:
  0  ok
  1  a finding — the check ran and failed
  2  usage or unreadable input
  3  refused — this build does not implement the command yet
";

/// The commands this engine will answer. Each is refused (exit 3) until its stage is built.
pub const COMMANDS: &[&str] = &["run", "blif-write", "blif-read", "restructure", "resynth", "resynth-annealing", "resynth-genetic"];

const DESCRIBE: &str = r#"{
  "schema": "vyges-tool-descriptor/1.1",
  "name": "rmp",
  "summary": "Logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.",
  "maturity": "discovered",
  "provenance_limitations": [
    "`blif-write` and `blif-read` are built (the cut's BLIF, arrivals and requireds not written); `restructure` (both targets), `resynth`, `resynth-annealing` and `resynth-genetic` are built; `run` refuses with exit 3.",
    "restructure matches the reference at pin 27288f5 on 3 of 3 regression cases (area and timing targets): the database (Verilog and DEF), the cut BLIF with its arrival and required times, and the log, byte for byte.",
    "resynth matches the reference at pin 27288f5 on 10 of 10 regression calls, and resynth-annealing and resynth-genetic on 5 of 5: the database (Verilog and DEF) and the log, byte for byte."
  ],
  "invocation": { "args_template": ["run"], "emits_json": true },
  "inputs": { "type": "object", "properties": {} },
  "artifacts": [],
  "assertion": { "id": "rmp-ok", "field": "status", "pass_when": { "eq": "ok" } },
  "consumes": []
}"#;

fn refuse(what: &str) -> ExitCode {
    events::refused(&format!("{what} is not implemented in this build"));
    ExitCode::from(3)
}

fn input_error(msg: &str) -> ExitCode {
    events::input(msg);
    ExitCode::from(2)
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("vyges-rmp: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn refused(why: String) -> ExitCode {
    events::refused(&why);
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
    events::step("blif-write", &format!("read {} with {} instance(s) to cut", a.design, a.insts.len()));
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
    events::step("blif-read", &format!("read {} and {blif}; replacing {} instance(s)", a.design, a.insts.len()));
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

/// A `-tielo_port` / `-tiehi_port` as `get_lib_pins` resolves it: the cell and port must exist in
/// a library, else the port is not set (no tie cell, so no constant removal).
fn lib_port(lib: &vyges_sta::liberty::Library, spec: &str) -> Option<(String, String)> {
    let (cell, port) = spec.rsplit_once('/')?;
    lib.cells.get(cell)?.ports.iter().any(|p| p.name == port).then(|| (cell.to_string(), port.to_string()))
}

/// `restructure`: the Tcl proc's argument handling, then `Restructure::run`. Log lines are the reference's.
fn restructure(args: &[String]) -> ExitCode {
    let mut o = vyges_rmp::restructure::Options { area: true, slack_threshold: 0.0, depth_threshold: 16, work_dir: ".".into(), ..Default::default() };
    let (mut liberty, mut sdc, mut out, mut pos) = (Vec::new(), None, String::new(), Vec::new());
    let (mut tielo, mut tiehi) = (None, None);
    let (mut wire_rc, mut alpha): (Option<Vec<String>>, f32) = (None, 0.3);
    let mut it = args.iter();
    while let Some(f) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| usage_error(&format!("{f} needs a value")));
        let r: Result<(), ExitCode> = (|| {
            match f.as_str() {
                "--liberty" => liberty.push(value()?),
                "--sdc" => sdc = Some(value()?),
                "--liberty-file" => o.liberty_file = value()?,
                "--target" => match value()?.as_str() {
                    "area" => o.area = true,
                    "timing" => o.area = false,
                    t => return Err(usage_error(&format!("--target is area or timing, not `{t}`"))),
                },
                "--set-wire-rc" => wire_rc = Some(value()?.split_whitespace().map(String::from).collect()),
                "--routing-alpha" => {
                    let v = value()?;
                    alpha = v.parse().map_err(|_| usage_error(&format!("--routing-alpha needs a number, not `{v}`")))?;
                }
                "--tielo-port" => tielo = Some(value()?),
                "--tiehi-port" => tiehi = Some(value()?),
                "--work-dir" => o.work_dir = value()?,
                "--abc-logfile" => o.abc_logfile = value()?,
                "--slack-threshold" => {
                    let v = value()?;
                    o.slack_threshold = v.parse().map_err(|_| usage_error(&format!("--slack-threshold needs a number, not `{v}`")))?;
                }
                "--depth-threshold" => {
                    let v = value()?;
                    o.depth_threshold = v.parse().map_err(|_| usage_error(&format!("--depth-threshold needs a number, not `{v}`")))?;
                }
                "-o" => out = value()?,
                "-q" | "--quiet" | "-v" | "--verbose" => {}
                x if x.starts_with('-') => return Err(usage_error(&format!("unknown option `{x}`"))),
                x => pos.push(x.to_string()),
            }
            Ok(())
        })();
        if let Err(c) = r {
            return c;
        }
    }
    let design = match <[String; 1]>::try_from(pos) {
        Ok([d]) if !out.is_empty() => d,
        _ => return usage_error("restructure needs a design database and -o OUT"),
    };
    if o.liberty_file.is_empty() {
        println!("[ERROR RMP-0001] Missing argument -liberty_file");
        return ExitCode::from(1);
    }
    let Some(sdc) = sdc else { return usage_error("restructure needs --sdc FILE") };
    let lib = match read_libraries(&liberty) {
        Ok(l) => l,
        Err(c) => return c,
    };
    // The proc warns for a port not given, and quietly skips one `get_lib_pins` does not find.
    match &tielo {
        Some(p) => o.tie_lo = lib_port(&lib, p),
        None => println!("[WARNING RMP-0007] -tielo_port not specified"),
    }
    match &tiehi {
        Some(p) => o.tie_hi = lib_port(&lib, p),
        None => println!("[WARNING RMP-0008] -tiehi_port not specified"),
    }
    let mut texts = Vec::new();
    for f in &liberty {
        match std::fs::read_to_string(f) {
            Ok(t) => texts.push(t),
            Err(e) => return input_error(&format!("cannot read {f}: {e}")),
        }
    }
    let sdc = match std::fs::read_to_string(&sdc) {
        Ok(t) => t,
        Err(e) => return input_error(&format!("cannot read {sdc}: {e}")),
    };
    let mut db = match vyges_opendb::Db::open(&design) {
        Ok(db) => db,
        Err(e) => return input_error(&format!("cannot read {design}: {e}")),
    };
    let timer = vyges_rmp::timer::Timer { liberties: texts, corners: Vec::new(), liberty_corners: Vec::new(), sdc, wire_rc, alpha };
    let mut say = |l: &str| {
        use std::io::Write;
        println!("{l}");
        let _ = std::io::stdout().flush();
    };
    events::step("restructure", &format!("read {design}; restructuring"));
    match vyges_rmp::restructure::run(&mut db, &timer, &lib, &o, &mut say) {
        Ok(()) => {}
        Err(vyges_rmp::restructure::RunError::Error(l)) => {
            println!("{l}");
            return ExitCode::from(1);
        }
        Err(vyges_rmp::restructure::RunError::Refused(why)) => return refused(why),
    }
    if let Err(e) = db.write(&out) {
        return input_error(&format!("cannot write {out}: {e}"));
    }
    events::step("restructure", &format!("wrote {out}"));
    ExitCode::SUCCESS
}

/// One `set_dont_use` / `unset_dont_use` / `reset_dont_use`, in script order.
enum DontUseCmd {
    Set(String),
    Unset(String),
    Reset,
}

/// The tuning strategies' own options (the Tcl procs' keys), per command.
const ANNEALING_KEYS: &[&str] = &["--iters", "--revert-after", "--seed", "--temp", "--initial-ops", "--slack-threshold"];
const GENETIC_KEYS: &[&str] = &[
    "--iters",
    "--seed",
    "--population-size",
    "--mutation-probability",
    "--crossover-probability",
    "--tournament-size",
    "--tournament-probability",
    "--initial-ops",
    "--slack-threshold",
];

/// `resynth`, `resynth_annealing`, `resynth_genetic`: the Tcl procs' argument handling, then
/// `Restructure::resynth` (the zero-slack strategy), `resynthAnnealing` or `resynthGenetic`. Log
/// lines are the reference's.
fn resynth(args: &[String], command: &str) -> ExitCode {
    let keys: &[&str] = match command {
        "resynth-annealing" => ANNEALING_KEYS,
        "resynth-genetic" => GENETIC_KEYS,
        _ => &[],
    };
    let mut extra: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let (mut liberty, mut sdc, mut out, mut pos) = (Vec::<(Option<String>, String)>::new(), None, String::new(), Vec::new());
    let (mut corners, mut corner, mut name_index): (Vec<String>, Option<String>, i64) = (Vec::new(), None, 0);
    let (mut wire_rc, mut alpha): (Option<Vec<String>>, f32) = (None, 0.3);
    let mut dont_use_cmds = Vec::new();
    let mut it = args.iter();
    while let Some(f) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| usage_error(&format!("{f} needs a value")));
        let r: Result<(), ExitCode> = (|| {
            match f.as_str() {
                "--liberty" => {
                    let v = value()?;
                    match v.split_once('=') {
                        Some((c, p)) => liberty.push((Some(c.to_string()), p.to_string())),
                        None => liberty.push((None, v)),
                    }
                }
                "--sdc" => sdc = Some(value()?),
                "--corners" => corners = value()?.split(',').map(String::from).collect(),
                "--corner" => corner = Some(value()?),
                "--name-index" => {
                    let v = value()?;
                    name_index = v.parse().map_err(|_| usage_error(&format!("--name-index needs a number, not `{v}`")))?;
                }
                "--dont-use" => dont_use_cmds.push(DontUseCmd::Set(value()?)),
                "--unset-dont-use" => dont_use_cmds.push(DontUseCmd::Unset(value()?)),
                "--reset-dont-use" => dont_use_cmds.push(DontUseCmd::Reset),
                "--set-wire-rc" => wire_rc = Some(value()?.split_whitespace().map(String::from).collect()),
                "--routing-alpha" => {
                    let v = value()?;
                    alpha = v.parse().map_err(|_| usage_error(&format!("--routing-alpha needs a number, not `{v}`")))?;
                }
                "-o" => out = value()?,
                "-q" | "--quiet" | "-v" | "--verbose" => {}
                x if keys.contains(&x) => {
                    let v = value()?;
                    extra.insert(x.to_string(), v);
                }
                x if x.starts_with('-') => return Err(usage_error(&format!("unknown option `{x}`"))),
                x => pos.push(x.to_string()),
            }
            Ok(())
        })();
        if let Err(c) = r {
            return c;
        }
    }
    let design = match <[String; 1]>::try_from(pos) {
        Ok([d]) if !out.is_empty() => d,
        _ => return usage_error(&format!("{command} needs a design database and -o OUT")),
    };
    // The strategy's settings, the reference's defaults where an option is not given.
    fn num<T: std::str::FromStr>(extra: &std::collections::HashMap<String, String>, key: &str, default: T) -> Result<T, ExitCode> {
        match extra.get(key) {
            None => Ok(default),
            Some(v) => v.parse().map_err(|_| usage_error(&format!("{key} needs a number, not `{v}`"))),
        }
    }
    let settings = (|| -> Result<(f32, Option<Box<dyn vyges_rmp::slack_tuning::Strategy>>), ExitCode> {
        let threshold = num(&extra, "--slack-threshold", 0.0f32)?;
        // `int seed` given to `mt19937::seed`: taken modulo 2^32.
        let seed = num(&extra, "--seed", 0i64)? as u32;
        let strategy: Option<Box<dyn vyges_rmp::slack_tuning::Strategy>> = match command {
            "resynth-annealing" => Some(Box::new(vyges_rmp::annealing::Annealing {
                rng: vyges_rmp::rng::Mt19937::new(seed),
                temperature: match extra.get("--temp") {
                    None => None,
                    Some(_) => Some(num(&extra, "--temp", 0.0f32)?),
                },
                iterations: num(&extra, "--iters", 100u32)?,
                revert_after: match extra.get("--revert-after") {
                    None => None,
                    Some(_) => Some(num(&extra, "--revert-after", 0u32)?),
                },
                initial_ops: num(&extra, "--initial-ops", 10u32)?,
            })),
            "resynth-genetic" => Some(Box::new(vyges_rmp::genetic::Genetic {
                rng: vyges_rmp::rng::Mt19937::new(seed),
                population_size: num(&extra, "--population-size", 4u32)?,
                mutation_probability: num(&extra, "--mutation-probability", 0.5f32)?,
                crossover_probability: num(&extra, "--crossover-probability", 0.5f32)?,
                tournament_size: num(&extra, "--tournament-size", 4u32)?,
                tournament_probability: num(&extra, "--tournament-probability", 0.8f32)?,
                iterations: num(&extra, "--iters", 10u32)?,
                initial_ops: num(&extra, "--initial-ops", 10u32)?,
            })),
            _ => None,
        };
        Ok((threshold, strategy))
    })();
    let (slack_threshold, mut strategy) = match settings {
        Ok(s) => s,
        Err(c) => return c,
    };
    let Some(sdc) = sdc else { return usage_error("resynth needs --sdc FILE") };
    if liberty.is_empty() {
        return usage_error("resynth needs --liberty FILE");
    }
    let mut texts = Vec::new();
    let mut libs = Vec::new();
    for (_, f) in &liberty {
        let text = match vyges_rsz::job::read_text(f) {
            Ok(t) => t,
            Err(e) => return input_error(&e),
        };
        match vyges_sta::liberty_parse::parse(&text).and_then(|g| vyges_sta::liberty::Library::read(&g)) {
            Ok(l) => libs.push(l),
            Err(e) => return input_error(&format!("cannot read {f}: {e}")),
        }
        texts.push(text);
    }
    // `sta::parse_scene`: the named corner, else the command scene (the first).
    let scene = match &corner {
        Some(c) if !corners.contains(c) => return usage_error(&format!("--corner {c}: not among --corners")),
        Some(c) => Some(c.clone()),
        None => corners.first().cloned(),
    };
    // `dont_use_`: the Liberty dont_use cells (`copyDontUseFromLiberty`), then each command in order
    // over every library's cells matching it (`reset_dont_use` clears and copies again).
    let liberty_dont_use = |set: &mut std::collections::BTreeSet<(usize, String)>| {
        for (k, l) in libs.iter().enumerate() {
            set.extend(l.cells.values().filter(|c| c.dont_use).map(|c| (k, c.name.clone())));
        }
    };
    let mut dont_use = std::collections::BTreeSet::new();
    liberty_dont_use(&mut dont_use);
    for c in &dont_use_cmds {
        match c {
            DontUseCmd::Reset => {
                dont_use.clear();
                liberty_dont_use(&mut dont_use);
            }
            DontUseCmd::Set(p) | DontUseCmd::Unset(p) => {
                for (k, l) in libs.iter().enumerate() {
                    for name in l.cells.keys().filter(|n| p.split_whitespace().any(|q| vyges_rsz::job::glob_match(q, n))) {
                        if matches!(c, DontUseCmd::Set(_)) {
                            dont_use.insert((k, name.clone()));
                        } else {
                            dont_use.remove(&(k, name.clone()));
                        }
                    }
                }
            }
        }
    }
    let sdc = match std::fs::read_to_string(&sdc) {
        Ok(t) => t,
        Err(e) => return input_error(&format!("cannot read {sdc}: {e}")),
    };
    let mut db = match vyges_opendb::Db::open(&design) {
        Ok(db) => db,
        Err(e) => return input_error(&format!("cannot read {design}: {e}")),
    };
    let timer = vyges_rmp::timer::Timer {
        liberties: texts,
        corners: corners.clone(),
        liberty_corners: liberty.iter().map(|(c, _)| c.clone()).collect(),
        sdc,
        wire_rc,
        alpha,
    };
    let facts = match vyges_rmp::restructure::TimerEnv::facts(&timer, &db) {
        Ok(f) => f,
        Err(e) => return refused(format!("resynth: timer: {e}")),
    };
    let corner_libs: Vec<vyges_rmp::cut::abc_library::CornerLib> = libs
        .iter()
        .enumerate()
        .filter(|(k, _)| liberty[*k].0.is_none() || liberty[*k].0 == scene)
        .map(|(index, lib)| vyges_rmp::cut::abc_library::CornerLib { index, lib })
        .collect();
    let is_dont_use = |k: usize, name: &str| dont_use.contains(&(k, name.to_string()));
    let l = vyges_rmp::resynth::Libraries { default: &libs[0], default_path: &liberty[0].1, corner: corner_libs, dont_use: &is_dont_use };
    let mut names = vyges_rmp::cut::logic_cut::UniqueName { counter: name_index };
    let mut say = |l: &str| {
        use std::io::Write;
        println!("{l}");
        let _ = std::io::stdout().flush();
    };
    let scene_index = scene.as_ref().and_then(|c| corners.iter().position(|x| x == c)).unwrap_or(0);
    let time = |d: &vyges_opendb::Db| vyges_rmp::restructure::TimerEnv::facts(&timer, d);
    events::step(command, &format!("read {design}; timed it at {} corner(s)", corners.len().max(1)));
    let ran = match strategy.as_mut() {
        None => vyges_rmp::resynth::zero_slack(&mut db, &facts, &l, &mut names, &mut say),
        Some(s) => vyges_rmp::slack_tuning::optimize_design(&mut db, &facts, &l, &mut names, &time, scene_index, slack_threshold, s.as_mut(), &mut say),
    };
    match ran {
        Ok(()) => {}
        Err(vyges_rmp::restructure::RunError::Error(l)) => {
            println!("{l}");
            return ExitCode::from(1);
        }
        Err(vyges_rmp::restructure::RunError::Refused(why)) => return refused(why),
    }
    if let Err(e) = db.write(&out) {
        return input_error(&format!("cannot write {out}: {e}"));
    }
    events::step(command, &format!("wrote {out}"));
    ExitCode::SUCCESS
}

/// The exit code's number, for the trail.
fn code_of(c: ExitCode) -> u8 {
    (0..=u8::MAX).find(|&n| ExitCode::from(n) == c).unwrap_or(u8::MAX)
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // ⛔ Both flags SET SOMETHING: `vyges-events` reads its floor from `VYGES_LOG` once, so they
    // land there before the first event. An explicit `VYGES_LOG` in the environment wins.
    if std::env::var_os("VYGES_LOG").is_none() {
        if args.iter().any(|a| a == "-q" || a == "--quiet") {
            std::env::set_var("VYGES_LOG", "error");
        } else if args.iter().any(|a| a == "-v" || a == "--verbose") {
            std::env::set_var("VYGES_LOG", "debug");
        }
    }
    let args: Vec<String> = args.into_iter().skip_while(|a| ["-q", "--quiet", "-v", "--verbose"].contains(&a.as_str())).collect();
    let code = run(&args);
    if let Some(cmd) = args.first().filter(|c| COMMANDS.contains(&c.as_str())) {
        events::done(cmd, code_of(code));
    }
    code
}

fn run(args: &[String]) -> ExitCode {
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
        Some("restructure") => restructure(&args[1..]),
        Some(c @ ("resynth" | "resynth-annealing" | "resynth-genetic")) => resynth(&args[1..], c),
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

    // Rule: the trail's DONE event reports the number the process exits with.
    #[test]
    fn the_trail_reports_the_exit_code_given() {
        for n in [0, 1, 2, 3] {
            assert_eq!(code_of(ExitCode::from(n)), n);
        }
    }

    #[test]
    fn the_descriptor_is_valid_json_with_a_legal_maturity() {
        let d: serde_json::Value = serde_json::from_str(DESCRIBE).expect("descriptor is valid JSON");
        assert_eq!(d["schema"], "vyges-tool-descriptor/1.1");
        assert_eq!(d["name"], "rmp");
        // Anything else is read as `discovered` and every verdict becomes `unknown`.
        let m = d["maturity"].as_str().unwrap();
        assert!(["discovered", "structured", "workflow-validated"].contains(&m), "{m}");
        // `eq` is the schema's predicate; `equals` is dropped at runtime and the verdict is unknown.
        assert_eq!(d["assertion"]["pass_when"]["eq"], "ok");
    }

    #[test]
    fn the_usage_spells_the_engine_under_its_group() {
        // The dispatcher REFUSES a wrong group prefix, so the help must spell the right one.
        for c in COMMANDS {
            assert!(USAGE.contains(&format!("vyges physical rmp {c}")), "{c}");
        }
    }
}
