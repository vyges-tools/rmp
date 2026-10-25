// SPDX-License-Identifier: Apache-2.0
//! `restructure`: the design's extracted logic re-synthesised through ABC and written back, in the
//! reference's call sequence (`Restructure::run`):
//!
//! 1. [`remove_const_cells`] (area mode) — outputs the timer proves constant move onto tie cells;
//!    an instance whose every output was constant is destroyed;
//! 2. [`get_blob`] — the end points ([`get_end_points`]), the logic between them and the register
//!    outputs and input ports that feed them ([`find_fanin_fanouts`]), its instances;
//! 3. [`run_abc`] — the instances as BLIF, one ABC run per mode, the run with the fewest instances
//!    (area) read back.
//!
//! `postABC` re-estimates parasitics for the timer that follows; nothing it computes reaches the
//! database, so it has no stage here.
//!
//! Every container on the path from end points to instances is a SET in the reference (vertices,
//! pins, instances by id), so the order a cone is searched in decides nothing.

use std::collections::{BTreeMap, BTreeSet};

/// `Restructure::Mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Area1,
    Area2,
    Area3,
    Delay1,
    Delay2,
    Delay3,
    Delay4,
}

/// The modes `runABC` tries, in order: three for area, four for delay.
pub fn modes(area: bool) -> &'static [Mode] {
    if area {
        &[Mode::Area1, Mode::Area2, Mode::Area3]
    } else {
        &[Mode::Delay1, Mode::Delay2, Mode::Delay3, Mode::Delay4]
    }
}

/// What a `restructure` call is given (its Tcl arguments, resolved).
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `-target area` (the default) or `timing`.
    pub area: bool,
    pub slack_threshold: f32,
    pub depth_threshold: u32,
    /// `-liberty_file`, as given: ABC reads it with its own reader.
    pub liberty_file: String,
    pub work_dir: String,
    /// `-abc_logfile`; empty when not given.
    pub abc_logfile: String,
    /// `-tiehi_port` / `-tielo_port` as (cell, port); `None` when not given or not found.
    pub tie_hi: Option<(String, String)>,
    pub tie_lo: Option<(String, String)>,
}

/// The timer's view of the design that `get_blob` reads: one entry per graph vertex.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// The vertex's pin: `inst/term`, or a port's name.
    pub pin: Vec<String>,
    /// The instance a pin belongs to, `None` for a port.
    pub inst: Vec<Option<String>>,
    /// `Search::isEndpoint`.
    pub endpoint: Vec<bool>,
    /// `isTopLevelPort`.
    pub port: Vec<bool>,
    /// The resizer's `isRegOutput`: a register clock-to-Q arc ends here.
    pub reg_output: Vec<bool>,
    /// The resizer's `isRegister`: the pin's cell is sequential.
    pub register: Vec<bool>,
    /// `SearchPred0::searchFrom` / `searchTo` (not a constant).
    pub from_ok: Vec<bool>,
    pub to_ok: Vec<bool>,
    /// Edges `(from, to, SearchPredCombLogic::searchThru)`.
    pub edges: Vec<(usize, usize, bool)>,
    pub in_edges: Vec<Vec<usize>>,
    pub out_edges: Vec<Vec<usize>>,
    /// `check_timing -unconstrained_endpoints`: the end points no constraint reaches.
    pub unconstrained: Vec<usize>,
    /// `check_timing` reg_no_clks: register clock pins no clock reaches.
    pub unclocked: Vec<usize>,
    /// Timing mode: `PathExpanded(vertexWorstSlackPath(v, max)).size() / 2`; `None` without a
    /// worst-slack path (unconstrained).
    pub depth: Vec<Option<usize>>,
    /// The BLIF writer's `getArrivalTime` / `getRequiredTime` (rise, fall), in seconds: 0 where
    /// there is no path.
    pub arrival: Vec<[f32; 2]>,
    pub required: Vec<[f32; 2]>,
    /// `Sta::slack(vertex, max)`: the least max slack over the scenes (`INF` without a path).
    pub slack: Vec<f32>,
}

impl Facts {
    /// `graph_->pinLoadVertex(pin)`: a pin's first vertex (a bidirect pin's driver follows it).
    pub fn load_vertex(&self, pin: &str) -> Option<usize> {
        self.pin.iter().position(|p| p == pin)
    }
}

/// An instance terminal of the design as `removeConstCells` reads it.
#[derive(Clone, Debug)]
pub struct Term {
    pub mterm: String,
    pub sig_type: String,
    pub io_type: String,
    pub net: Option<String>,
}

/// One edit `removeConstCells` makes, in its order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstEdit {
    /// `dbInst::create(block, master, name)`, then the constant output `inst/mterm` disconnected
    /// and the new cell's `port` connected to `net` in its place.
    Tie { master: String, name: String, port: String, inst: String, mterm: String, net: String },
    /// `removeConstCell`: every terminal disconnected, the instance destroyed.
    Destroy(String),
}

/// `removeConstCells`, as a plan: `insts` in block order, each with its master, whether that
/// master's cell is sequential, its id, and its terminals; `value` is the timer's
/// `findLogicConstants` value of an output. Returns the edits in the reference's order and the
/// count it reports.
///
/// - Only the instances that existed when the loop started are visited: the tie cells it creates
///   have no simulated value (the edits never re-propagate constants, see `Sim::connectPinAfter`
///   under `findLogicConstants`), so they never count as constant.
/// - A constant output on no net counts toward the instance's removal, and spends a tie name,
///   but creates no tie cell.
/// - The instances whose every output was constant are destroyed in id order (a pointer set).
#[allow(clippy::type_complexity)]
pub fn const_cell_plan(
    insts: &[(String, String, bool, u32, Vec<Term>)],
    value: &dyn Fn(&str, &str) -> Option<bool>,
    hi: (&str, &str),
    lo: (&str, &str),
) -> (Vec<ConstEdit>, usize) {
    let mut edits = Vec::new();
    let mut remove: BTreeMap<u32, String> = BTreeMap::new();
    let mut const_cnt = 1usize;
    for (inst, _master, sequential, id, terms) in insts {
        if *sequential {
            continue;
        }
        let (mut outputs, mut const_outputs) = (0usize, 0usize);
        for t in terms {
            if t.sig_type == "POWER" || t.sig_type == "GROUND" || t.io_type != "OUTPUT" {
                continue;
            }
            outputs += 1;
            let Some(v) = value(inst, &t.mterm) else { continue };
            if let Some(net) = &t.net {
                let (master, port) = if v { hi } else { lo };
                edits.push(ConstEdit::Tie {
                    master: master.to_string(),
                    name: format!("rmp_const_{const_cnt}"),
                    port: port.to_string(),
                    inst: inst.clone(),
                    mterm: t.mterm.clone(),
                    net: net.clone(),
                });
            }
            const_outputs += 1;
            const_cnt += 1;
        }
        if outputs > 0 && outputs == const_outputs {
            remove.insert(*id, inst.clone());
        }
    }
    let removed = remove.len();
    edits.extend(remove.into_values().map(ConstEdit::Destroy));
    (edits, removed)
}

/// `getEndPoints` in area mode: every end point, then the `check_timing` lists — unconstrained end
/// points and unclocked register pins (each list's first line is its message, never a pin). Pins,
/// as a set. The count reported first is the end points' alone.
pub fn get_end_points(f: &Facts) -> (usize, BTreeSet<String>) {
    let mut ends = BTreeSet::new();
    let mut paths = 0usize;
    for v in 0..f.pin.len() {
        if f.endpoint[v] {
            paths += 1;
            ends.insert(f.pin[v].clone());
        }
    }
    for &v in f.unconstrained.iter().chain(&f.unclocked) {
        ends.insert(f.pin[v].clone());
    }
    (paths, ends)
}

/// `getEndPoints` in timing mode: the end points in vertex order, each with a worst-slack path
/// reporting its depth; the FIRST deeper than `max_depth` is the one end (the search stops
/// there). `(count, ends, depth lines)`.
pub fn get_end_points_timing(f: &Facts, max_depth: u32) -> (usize, BTreeSet<String>, Vec<String>) {
    let mut ends = BTreeSet::new();
    let mut lines = Vec::new();
    let paths = f.endpoint.iter().filter(|&&e| e).count();
    for v in (0..f.pin.len()).filter(|&v| f.endpoint[v]) {
        // Unconstrained end points have no worst-slack path.
        let Some(depth) = f.depth[v] else { continue };
        lines.push(format!("Found path of depth {depth}"));
        if depth > max_depth as usize {
            ends.insert(f.pin[v].clone());
            break;
        }
    }
    (paths, ends, lines)
}

/// `Resizer::findFanins(PinSet)`: back from the end pins' load vertices through combinational
/// logic; a register output or top-level port stops the search and is left out. The pins
/// reached, as a set.
pub fn find_fanins(f: &Facts, ends: &BTreeSet<String>) -> BTreeSet<String> {
    let seeds: Vec<usize> = ends.iter().filter_map(|p| f.load_vertex(p)).collect::<BTreeSet<_>>().into_iter().collect();
    let mut fanins = BTreeSet::new();
    dfs(f, true, &seeds, &mut |v| {
        if f.reg_output[v] || f.port[v] {
            return false;
        }
        fanins.insert(f.pin[v].clone());
        true
    });
    fanins
}

/// The resizer's `dfsSearch` with `visit_seeds = false`: from each seed's pred-filtered neighbours,
/// one visited set across all seeds; `visit` says whether to expand through a vertex.
fn dfs(f: &Facts, fanin: bool, seeds: &[usize], visit: &mut dyn FnMut(usize) -> bool) {
    let mut stack: Vec<usize> = Vec::new();
    let mut visited: BTreeSet<usize> = BTreeSet::new();
    let expand = |v: usize, stack: &mut Vec<usize>, visited: &mut BTreeSet<usize>| {
        if fanin {
            if f.to_ok[v] {
                for &e in &f.in_edges[v] {
                    let (from, _, thru) = f.edges[e];
                    if f.from_ok[from] && thru && visited.insert(from) {
                        stack.push(from);
                    }
                }
            }
        } else if f.from_ok[v] {
            for &e in &f.out_edges[v] {
                let (_, to, thru) = f.edges[e];
                if thru && f.to_ok[to] && visited.insert(to) {
                    stack.push(to);
                }
            }
        }
    };
    for &s in seeds {
        expand(s, &mut stack, &mut visited);
    }
    while let Some(v) = stack.pop() {
        if visit(v) {
            expand(v, &mut stack, &mut visited);
        }
    }
}

/// `Resizer::findFaninFanouts(PinSet)`: the end pins' load vertices; back to the fan-in ROOTS
/// (register outputs and top-level ports stop the search and are roots); forward from those roots
/// through everything that is not a register. The pins reached forward, as a set — neither the
/// roots nor the ends themselves unless reached again.
pub fn find_fanin_fanouts(f: &Facts, ends: &BTreeSet<String>) -> BTreeSet<String> {
    let seeds: Vec<usize> = ends.iter().filter_map(|p| f.load_vertex(p)).collect::<BTreeSet<_>>().into_iter().collect();
    let mut roots = BTreeSet::new();
    dfs(f, true, &seeds, &mut |v| {
        if f.reg_output[v] || f.port[v] {
            roots.insert(v);
            return false;
        }
        true
    });
    let roots: Vec<usize> = roots.into_iter().collect();
    let mut fanouts = BTreeSet::new();
    dfs(f, false, &roots, &mut |v| {
        if f.register[v] {
            return false;
        }
        fanouts.insert(v);
        true
    });
    fanouts.into_iter().map(|v| f.pin[v].clone()).collect()
}

/// `getBlob`'s instances: each extracted pin that is an instance terminal (`staToDb` gives an
/// iterm) whose master is not a block.
pub fn blob_instances(f: &Facts, pins: &BTreeSet<String>, is_block: &dyn Fn(&str) -> bool) -> BTreeSet<String> {
    let inst_of: BTreeMap<&str, &str> = f.pin.iter().zip(&f.inst).filter_map(|(p, i)| i.as_deref().map(|i| (p.as_str(), i))).collect();
    pins.iter().filter_map(|p| inst_of.get(p.as_str())).filter(|i| !is_block(i)).map(|i| i.to_string()).collect()
}

/// `writeOptCommands`: the aliases, then the mode's script.
pub fn write_opt_commands(mode: Mode) -> String {
    let mut s = String::new();
    s.push_str("bdd; sop\n");
    s.push_str("alias resyn2 \"balance; rewrite; refactor; balance; rewrite; rewrite -z; balance; refactor -z; rewrite -z; balance\"\n");
    s.push_str("alias choice \"fraig_store; resyn2; fraig_store; resyn2; fraig_store; fraig_restore\"\n");
    s.push_str("alias choice2 \"fraig_store; balance; fraig_store; resyn2; fraig_store; resyn2; fraig_store; resyn2; fraig_store; fraig_restore\"\n");
    s.push_str(if mode == Mode::Area3 { "choice2\n" } else { "resyn2\n" });
    match mode {
        Mode::Delay1 => s.push_str("map -D 0.01 -A 0.9 -B 0.2 -M 0 -p\nbuffer -p -c\n"),
        Mode::Delay2 => s.push_str("choice\nmap -D 0.01 -A 0.9 -B 0.2 -M 0 -p\nchoice\nmap -D 0.01\nbuffer -p -c\ntopo\n"),
        Mode::Delay3 => s.push_str("choice2\nmap -D 0.01 -A 0.9 -B 0.2 -M 0 -p\nchoice2\nmap -D 0.01\nbuffer -p -c\ntopo\n"),
        Mode::Delay4 => s.push_str("choice2\namap -F 20 -A 20 -C 5000 -Q 0.1 -m\nchoice2\nmap -D 0.01 -A 0.9 -B 0.2 -M 0 -p\nbuffer -p -c\n"),
        Mode::Area2 | Mode::Area3 => s.push_str("choice2\namap -m -Q 0.1 -F 20 -A 20 -C 5000\nchoice2\namap -m -Q 0.1 -F 20 -A 20 -C 5000\n"),
        Mode::Area1 => s.push_str("choice2\namap -m -Q 0.1 -F 20 -A 20 -C 5000\n"),
    }
    s
}

/// `writeAbcScript` (no debug level, so no `write_verilog`): read the library and the cut, the
/// mode's commands, write the result.
pub fn write_abc_script(libs: &[String], input_blif: &str, output_blif: &str, mode: Mode) -> String {
    let mut s = String::new();
    for l in libs {
        s.push_str(&format!("read_lib -v {l}\n"));
    }
    s.push_str(&format!("read_blif -n {input_blif}\n"));
    s.push_str(&write_opt_commands(mode));
    s.push_str(&format!("write_blif {output_blif}\n"));
    s
}

/// `readAbcLog`: `(ok, level_gain, delay)`. Nothing in the script writes the file, so it is
/// usually ABSENT — and an absent file reads as no lines (`ifstream::bad()` is false): ok, gain 0,
/// delay `FLT_MAX`. Lines are split on single spaces (consecutive spaces give empty tokens); an
/// `Error:` first token fails; a line of more than 7 tokens ending `lev = N` is a level, and its
/// first `delay` token followed by `=<v>` a delay. The gain is first level minus last, the delay
/// the last.
pub fn read_abc_log(text: Option<&str>) -> Result<(bool, i32, f32), String> {
    let (mut level, mut delay): (Vec<f64>, Vec<f32>) = (Vec::new(), Vec::new());
    let mut ok = true;
    for line in text.unwrap_or("").lines() {
        let tokens: Vec<&str> = line.split(' ').collect();
        // `std::getline(ss, buf, ' ')` gives no token after a trailing delimiter.
        let tokens: Vec<&str> = if tokens.last() == Some(&"") { tokens[..tokens.len() - 1].to_vec() } else { tokens };
        if tokens.first() == Some(&"Error:") {
            ok = false;
            break;
        }
        let n = tokens.len();
        if n > 7 && tokens[n - 3] == "lev" && tokens[n - 2] == "=" {
            let v: i32 = tokens[n - 1].parse().map_err(|_| format!("ABC log level `{}` is not an integer", tokens[n - 1]))?;
            level.push(v as f64);
        }
        if n > 7 {
            let mut prev = "";
            for t in &tokens {
                if prev == "delay" && t.starts_with('=') {
                    if t.len() > 1 {
                        delay.push(t[1..].parse().map_err(|_| format!("ABC log delay `{t}` is not a number"))?);
                    }
                    break;
                }
                prev = t;
            }
        }
    }
    let gain = if level.len() > 1 { (level[0] - level[level.len() - 1]) as i32 } else { 0 };
    Ok((ok, gain, delay.last().copied().unwrap_or(f32::MAX)))
}

/// A float as the reference's logger prints `{}`: the shortest digits that read back, fixed
/// between 1e-4 and 1e16, else `d.ddde±XX`.
pub fn fmt_float(v: f32) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    let e = format!("{v:e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        format!("{v}")
    } else {
        format!("{mant}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// One ABC run's outcome as `runABC` inspects it.
#[derive(Clone, Debug)]
pub struct ModeResult {
    /// The run executed (`Cmd_CommandExecute` returned 0).
    pub ran: bool,
    pub log_ok: bool,
    pub level_gain: i32,
    pub delay: f32,
    /// `inspectBlif`: the output parsed, and its gate count.
    pub blif_ok: bool,
    pub instances: usize,
}

/// `runABC`'s choice: area — the FEWEST instances, the first on ties; delay — the `Delay4` run
/// that read back, kept only when its log gave a delay (below `FLT_MAX`): with none, every run is
/// discarded. `None` keeps the original netlist.
pub fn pick_best(results: &[ModeResult], modes: &[Mode], area: bool) -> Option<usize> {
    let mut best = None;
    let mut best_count = usize::MAX;
    let mut best_delay = f32::MAX;
    for (k, r) in results.iter().enumerate() {
        if !r.ran || !r.log_ok || !r.blif_ok {
            continue;
        }
        if area {
            if r.instances < best_count {
                best_count = r.instances;
                best = Some(k);
            }
        } else if modes[k] == Mode::Delay4 {
            best_delay = r.delay;
            best = Some(k);
        }
    }
    if best_count < usize::MAX || best_delay < f32::MAX {
        best
    } else {
        None
    }
}

// ---- the database side: the stages in the reference's order ----------------------------------

use vyges_opendb::Db;
use vyges_sta::liberty::Library;

use crate::cut::blif::{self, BlifError, ConstCells};
use crate::design::OdbBlif;

/// Why `restructure` stopped: the reference's error line (it throws), or a refusal where the
/// reference's behaviour is not transcribed.
#[derive(Debug)]
pub enum RunError {
    Error(String),
    Refused(String),
}

impl From<BlifError> for RunError {
    fn from(e: BlifError) -> RunError {
        match e {
            BlifError::Error(l) => RunError::Error(l),
            BlifError::Refused(w) => RunError::Refused(w),
        }
    }
}

/// The timer, asked about the design as it stands.
pub trait TimerEnv {
    /// `Sta::findLogicConstants`: each instance output proven constant, by `(inst, mterm)`.
    fn logic_constants(&self, db: &Db) -> Result<BTreeMap<(String, String), bool>, String>;
    /// The graph, its constants (`ensureConstantsPropagated`), end points and `check_timing` lists.
    fn facts(&self, db: &Db) -> Result<Facts, String>;
}

/// `Restructure::run`.
pub fn run(db: &mut Db, timer: &dyn TimerEnv, lib: &Library, o: &Options, out: &mut dyn FnMut(&str)) -> Result<(), RunError> {
    if o.area {
        remove_const_cells(db, timer, lib, o, out)?;
    }
    let (insts, facts) = get_blob(db, timer, o, out)?;
    if !insts.is_empty() {
        run_abc(db, lib, o, &insts, &facts, out)?;
    }
    Ok(())
}

/// `removeConstCells`: nothing without both tie cells, or when either master or port is missing
/// (then no report line either).
pub fn remove_const_cells(db: &mut Db, timer: &dyn TimerEnv, lib: &Library, o: &Options, out: &mut dyn FnMut(&str)) -> Result<(), RunError> {
    let (Some(hi), Some(lo)) = (&o.tie_hi, &o.tie_lo) else { return Ok(()) };
    let has = |db: &Db, m: &str, t: &str| db.master_mterm_io(m).is_ok_and(|v| v.iter().any(|(n, _)| n == t));
    if !has(db, &hi.0, &hi.1) || !has(db, &lo.0, &lo.1) {
        return Ok(());
    }
    let consts = timer.logic_constants(db).map_err(RunError::Refused)?;
    let mut insts = Vec::new();
    for inst in db.inst_names() {
        let master = db.inst_master(&inst);
        let cell = lib.cells.get(&master).ok_or_else(|| RunError::Refused(format!("{inst}: master {master} has no Liberty cell")))?;
        let id = db.inst_id(&inst).map_err(|e| RunError::Refused(e.to_string()))?;
        let terms = db
            .iterm_names(&inst)
            .into_iter()
            .map(|m| Term {
                sig_type: db.iterm_get_sig_type(&inst, &m),
                io_type: db.iterm_get_io_type(&inst, &m),
                net: Some(db.iterm_get_net(&inst, &m)).filter(|n| !n.is_empty()),
                mterm: m,
            })
            .collect();
        insts.push((inst, master, cell.is_sequential(), id, terms));
    }
    let value = |i: &str, t: &str| consts.get(&(i.to_string(), t.to_string())).copied();
    let (edits, removed) = const_cell_plan(&insts, &value, (&hi.0, &hi.1), (&lo.0, &lo.1));
    let db_err = |e: vyges_opendb::Error| RunError::Refused(e.to_string());
    for e in edits {
        match e {
            ConstEdit::Tie { master, name, port, inst, mterm, net } => {
                if db.inst_try_create(&master, &name).map_err(db_err)? {
                    db.disconnect(&inst, &mterm).map_err(db_err)?;
                    db.connect(&name, &port, &net).map_err(db_err)?;
                } else {
                    out(&format!("[WARNING RMP-0009] Could not create instance {name}."));
                }
            }
            ConstEdit::Destroy(inst) => {
                for m in db.iterm_names(&inst) {
                    db.disconnect(&inst, &m).map_err(db_err)?;
                }
                db.destroy_inst(&inst).map_err(db_err)?;
            }
        }
    }
    out(&format!("Removed {removed} instances with constant outputs."));
    Ok(())
}

/// `getBlob`: the end points, their fan-in/fan-out logic, its instances (by name; the BLIF
/// writer takes them in id order).
pub fn get_blob(db: &Db, timer: &dyn TimerEnv, o: &Options, out: &mut dyn FnMut(&str)) -> Result<(Vec<String>, Facts), RunError> {
    let f = timer.facts(db).map_err(RunError::Refused)?;
    let (paths, ends, lines) = if o.area {
        let (p, e) = get_end_points(&f);
        (p, e, Vec::new())
    } else {
        get_end_points_timing(&f, o.depth_threshold)
    };
    out(&format!("Number of paths for restructure are {paths}"));
    for l in &lines {
        out(l);
    }
    out(&format!("Found {} end points for restructure", ends.len()));
    if ends.is_empty() {
        return Ok((Vec::new(), f));
    }
    let pins = if o.area { find_fanin_fanouts(&f, &ends) } else { find_fanins(&f, &ends) };
    // A probe's view of this stage: the database it read, its end points and extracted pins.
    if let Some(dir) = std::env::var_os("VYG_RMP_DUMP") {
        let dir = std::path::PathBuf::from(dir);
        let lines = |set: &BTreeSet<String>| set.iter().map(|p| format!("{p}\n")).collect::<String>();
        let unclocked: BTreeSet<String> = f.unclocked.iter().map(|&v| f.pin[v].clone()).collect();
        let written = std::fs::write(dir.join("ends.txt"), lines(&ends))
            .and_then(|_| std::fs::write(dir.join("unclocked.txt"), lines(&unclocked)))
            .and_then(|_| std::fs::write(dir.join("pins.txt"), lines(&pins)));
        written.map_err(|e| RunError::Error(format!("VYG_RMP_DUMP: {e}")))?;
        db.write(dir.join("blob.odb")).map_err(|e| RunError::Error(format!("VYG_RMP_DUMP: {e}")))?;
    }
    out(&format!("Found {} pins in extracted logic.", pins.len()));
    let insts = blob_instances(&f, &pins, &|i| db.master_is_block(&db.inst_master(i)));
    out(&format!("Found {} instances for restructuring.", insts.len()));
    Ok((insts.into_iter().collect(), f))
}

extern "C" {
    fn fflush(stream: *mut std::ffi::c_void) -> i32;
}

/// ABC prints through C's stdout, our lines through Rust's: both are flushed around every ABC
/// call so the log keeps the reference's order (where both share one stream).
fn flush_all() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // SAFETY: `fflush(NULL)` flushes every C output stream.
    unsafe {
        fflush(std::ptr::null_mut());
    }
}

/// `runABC`: the cut as BLIF, one ABC session per mode on its own script, each result inspected,
/// the best read back, the scratch files removed.
pub fn run_abc(db: &mut Db, lib: &Library, o: &Options, insts: &[String], f: &Facts, out: &mut dyn FnMut(&str)) -> Result<(), RunError> {
    let prefix = format!("{}/{}", o.work_dir, db.block_name());
    let input = format!("{prefix}_crit_path.blif");
    let mut remove = vec![input.clone()];
    let (lo, hi) = (o.tie_lo.clone().unwrap_or_default(), o.tie_hi.clone().unwrap_or_default());
    let mut consts = ConstCells { const0_cell: lo.0, const0_port: lo.1, const1_cell: hi.0, const1_port: hi.1 };
    // Timing mode writes each boundary net's arrival or required time (`write_arrival_requireds`).
    let at = |pin: &str, of: &Vec<[f32; 2]>| f.load_vertex(pin).map_or([0.0, 0.0], |v| of[v]);
    let arrival = |pin: &str| at(pin, &f.arrival);
    let required = |pin: &str| at(pin, &f.required);
    let times = blif::Times { arrival: &arrival, required: &required };
    let written = blif::write_blif_timed(&OdbBlif { db }, lib, insts, &mut consts, (!o.area).then_some(&times))?;
    std::fs::write(&input, &written.text).map_err(|e| RunError::Error(format!("[ERROR CUT-0001] Cannot open file {input} for writing: {e}")))?;
    out(&format!("[INFO CUT-0002] Blif writer successfully dumped file with {} instances.", written.instances));
    if let Some(dir) = std::env::var_os("VYG_RMP_DUMP") {
        std::fs::write(std::path::Path::new(&dir).join("crit_path.blif"), &written.text).map_err(|e| RunError::Error(format!("VYG_RMP_DUMP: {e}")))?;
    }
    let modes = modes(o.area);
    let logfile = if o.abc_logfile.is_empty() { format!("{prefix}abc.log") } else { o.abc_logfile.clone() };
    let output = |k: usize| format!("{prefix}{k}_crit_path_out.blif");
    for (k, &mode) in modes.iter().enumerate() {
        let script = format!("{prefix}{k}ord_abc_script.tcl");
        std::fs::write(&script, write_abc_script(std::slice::from_ref(&o.liberty_file), &input, &output(k), mode))
            .map_err(|_| RunError::Error(format!("[ERROR RMP-0003] Cannot open file {script} for writing.")))?;
        let command = format!("source {script}");
        flush_all();
        let ran = vyges_abc_lib::Abc::start().and_then(|mut abc| abc.execute(&command));
        flush_all();
        if ran.is_err() {
            return Err(RunError::Error(format!("[ERROR RMP-0006] Error executing ABC command {command}.")));
        }
        remove.push(script);
    }
    let mut results = Vec::new();
    for k in 0..modes.len() {
        let log = std::fs::read_to_string(format!("{logfile}{k}")).ok();
        let (log_ok, level_gain, delay) = read_abc_log(log.as_deref()).map_err(RunError::Refused)?;
        let mut r = ModeResult { ran: true, log_ok, level_gain, delay, blif_ok: false, instances: 0 };
        if log_ok {
            // `inspectBlif`: the output parsed (comments removed first), and its gate count.
            let mut text = std::fs::read(output(k)).unwrap_or_default();
            blif::preprocess_string(&mut text);
            if let Some(p) = blif::parse(&text) {
                r.blif_ok = true;
                r.instances = p.gates.len();
            }
            out(&format!(
                "Optimized to {} instances in iteration {k} with max path depth decrease of {level_gain}, delay of {}.",
                r.instances,
                fmt_float(delay)
            ));
        }
        remove.push(output(k));
        results.push(r);
    }
    match pick_best(&results, modes, o.area) {
        Some(k) => {
            let text = std::fs::read(output(k)).unwrap_or_default();
            let mut log = Vec::new();
            let r = blif::read_blif(&mut OdbBlif { db }, lib, insts, &consts, 1, &text, &mut log);
            for l in &log {
                out(l);
            }
            r?;
        }
        None => out("[INFO RMP-0004] All re-synthesis runs discarded, keeping original netlist."),
    }
    for f in &remove {
        match std::fs::remove_file(f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(RunError::Error(format!("[ERROR RMP-0011] Fail to remove file {f}"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(m: &str, io: &str, net: Option<&str>) -> Term {
        Term { mterm: m.into(), sig_type: "SIGNAL".into(), io_type: io.into(), net: net.map(String::from) }
    }

    /// Rule (`removeConstCells`): a constant output moves to a new tie cell `rmp_const_<n>` on its
    /// net; the counter advances per constant OUTPUT (also one on no net, which gets no cell); an
    /// instance whose every output was constant is destroyed after the loop, in id order;
    /// sequential cells are skipped.
    #[test]
    fn constant_outputs_move_to_tie_cells_and_all_constant_instances_go() {
        let insts = vec![
            ("b".to_string(), "INV".to_string(), false, 7, vec![term("A", "INPUT", Some("n0")), term("ZN", "OUTPUT", Some("n1"))]),
            ("ff".to_string(), "DFF".to_string(), true, 2, vec![term("Q", "OUTPUT", Some("q"))]),
            ("a".to_string(), "HA".to_string(), false, 3, vec![term("CO", "OUTPUT", None), term("S", "OUTPUT", Some("s"))]),
            ("c".to_string(), "FA".to_string(), false, 1, vec![term("CO", "OUTPUT", Some("co")), term("S", "OUTPUT", Some("s2"))]),
        ];
        let value = |i: &str, t: &str| match (i, t) {
            ("b", "ZN") => Some(true),
            ("ff", "Q") => Some(false),
            ("a", _) => Some(false),
            ("c", "CO") => Some(false),
            _ => None,
        };
        let (edits, removed) = const_cell_plan(&insts, &value, ("HI", "Z"), ("LO", "Z"));
        let tie = |name: &str, master: &str, inst: &str, mterm: &str, net: &str| ConstEdit::Tie {
            master: master.into(), name: name.into(), port: "Z".into(), inst: inst.into(), mterm: mterm.into(), net: net.into(),
        };
        assert_eq!(
            edits,
            vec![
                tie("rmp_const_1", "HI", "b", "ZN", "n1"),
                // a/CO is constant on no net: rmp_const_2 is spent, no cell is made.
                tie("rmp_const_3", "LO", "a", "S", "s"),
                tie("rmp_const_4", "LO", "c", "CO", "co"),
                // a (id 3) and b (id 7) were wholly constant; c kept S. Id order.
                ConstEdit::Destroy("a".into()),
                ConstEdit::Destroy("b".into()),
            ]
        );
        assert_eq!(removed, 2);
    }

    fn chain() -> Facts {
        // in -> u1/A u1/Z -> r/D (register) ; r/Q -> u2/A u2/Z -> out ; tie/Z (constant) -> u2/B
        let pins = ["in", "u1/A", "u1/Z", "r/D", "r/CK", "r/Q", "u2/A", "u2/B", "u2/Z", "out", "tie/Z"];
        let n = pins.len();
        let mut f = Facts {
            pin: pins.iter().map(|s| s.to_string()).collect(),
            inst: pins.iter().map(|p| p.split_once('/').map(|(i, _)| i.to_string())).collect(),
            endpoint: vec![false; n],
            port: vec![false; n],
            reg_output: vec![false; n],
            register: vec![false; n],
            from_ok: vec![true; n],
            to_ok: vec![true; n],
            ..Facts::default()
        };
        f.port[0] = true;
        f.port[9] = true;
        f.endpoint[3] = true;
        f.endpoint[9] = true;
        for v in [3, 4, 5] {
            f.register[v] = true;
        }
        f.reg_output[5] = true;
        f.from_ok[10] = false;
        f.to_ok[10] = false;
        // (from, to, comb-logic thru): the clock-to-Q edge is not crossed.
        let edges = [(0, 1, true), (1, 2, true), (2, 3, true), (4, 5, false), (5, 6, true), (6, 8, true), (7, 8, true), (8, 9, true), (10, 7, true)];
        f.in_edges = vec![Vec::new(); n];
        f.out_edges = vec![Vec::new(); n];
        for (k, &(a, b, t)) in edges.iter().enumerate() {
            f.edges.push((a, b, t));
            f.out_edges[a].push(k);
            f.in_edges[b].push(k);
        }
        f
    }

    /// Rule (`findFaninRoots` then `findFanouts`): back from the ends to register outputs and ports
    /// (roots), forward from the roots through everything but registers; constants neither start
    /// nor end a step (`SearchPred0`); the roots themselves are not in the result.
    #[test]
    fn the_cone_runs_back_to_roots_then_forward_to_registers() {
        let f = chain();
        let (paths, ends) = get_end_points(&f);
        assert_eq!(paths, 2);
        let pins = find_fanin_fanouts(&f, &ends);
        let want: BTreeSet<String> = ["u1/A", "u1/Z", "u2/A", "u2/Z", "out"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pins, want);
        let insts = blob_instances(&f, &pins, &|i| i == "u2");
        assert_eq!(insts, ["u1".to_string()].into_iter().collect());
    }

    /// Rule (`getEndPoints`, area): the `check_timing` lists join the end points as pins.
    #[test]
    fn unclocked_register_pins_join_the_end_points() {
        let mut f = chain();
        f.unclocked.push(4);
        let (paths, ends) = get_end_points(&f);
        assert_eq!(paths, 2);
        assert!(ends.contains("r/CK"));
        assert_eq!(ends.len(), 3);
    }

    /// Rule (`writeAbcScript` / `writeOptCommands`): the reference's area-1 script, byte for byte.
    #[test]
    fn the_area_script_is_the_reference_text() {
        let s = write_abc_script(&["L.lib".into()], "in.blif", "out.blif", Mode::Area1);
        assert_eq!(
            s,
            "read_lib -v L.lib\nread_blif -n in.blif\nbdd; sop\n\
             alias resyn2 \"balance; rewrite; refactor; balance; rewrite; rewrite -z; balance; refactor -z; rewrite -z; balance\"\n\
             alias choice \"fraig_store; resyn2; fraig_store; resyn2; fraig_store; fraig_restore\"\n\
             alias choice2 \"fraig_store; balance; fraig_store; resyn2; fraig_store; resyn2; fraig_store; resyn2; fraig_store; fraig_restore\"\n\
             resyn2\nchoice2\namap -m -Q 0.1 -F 20 -A 20 -C 5000\nwrite_blif out.blif\n"
        );
        assert!(write_opt_commands(Mode::Area3).contains("\nchoice2\nchoice2\namap"));
        assert!(write_opt_commands(Mode::Area2).contains("\nresyn2\nchoice2\namap"));
    }

    /// Rule (`readAbcLog`): an absent log is a success with no gain and `FLT_MAX` delay; `Error:`
    /// fails; levels and delays come from lines of more than 7 space-split tokens.
    #[test]
    fn the_abc_log_reads_as_the_reference_reads_it() {
        assert_eq!(read_abc_log(None).unwrap(), (true, 0, f32::MAX));
        assert!(!read_abc_log(Some("Error: no lib\n")).unwrap().0);
        let t = "net  : i/o = 8/14 nd = 36 lev = 9\nnet  : i/o = 8/14 nd = 30 lev = 6\nx y z w v u delay =12.5 area = 3\n";
        assert_eq!(read_abc_log(Some(t)).unwrap(), (true, 3, 12.5));
    }

    /// Rule: the logger's `{}` of a float — `FLT_MAX` is `3.4028235e+38`.
    #[test]
    fn floats_print_as_the_reference_logger_prints_them() {
        assert_eq!(fmt_float(f32::MAX), "3.4028235e+38");
        assert_eq!(fmt_float(12.5), "12.5");
        assert_eq!(fmt_float(0.00001), "1e-05");
    }

    /// Rule (`runABC`): area keeps the FIRST run with the fewest instances; a run whose log or
    /// BLIF failed is skipped; delay keeps `Delay4`.
    #[test]
    fn the_least_instances_win_and_the_first_breaks_a_tie() {
        let r = |n: usize| ModeResult { ran: true, log_ok: true, level_gain: 0, delay: f32::MAX, blif_ok: true, instances: n };
        assert_eq!(pick_best(&[r(36), r(37), r(36)], modes(true), true), Some(0));
        let mut bad = r(10);
        bad.blif_ok = false;
        assert_eq!(pick_best(&[r(36), bad, r(35)], modes(true), true), Some(2));
        // Delay: only the Delay4 run, and only when its log gave a delay — with no log (the
        // reference's scripts write none) every run is discarded.
        assert_eq!(pick_best(&[r(1), r(1), r(1), r(9)], modes(false), false), None);
        let mut timed = r(9);
        timed.delay = 5.0;
        assert_eq!(pick_best(&[r(1), r(1), r(1), timed.clone()], modes(false), false), Some(3));
        assert_eq!(pick_best(&[timed.clone(), r(1), r(1), r(9)], modes(false), false), None);
    }

    /// Rule (`getEndPoints`, timing): end points in vertex order; one with no worst-slack path is
    /// skipped silently; each other reports its depth; the first deeper than the threshold is the
    /// only end, and the search stops there. The count is every end point.
    #[test]
    fn the_first_end_deeper_than_the_threshold_is_the_one_end() {
        let mut f = chain();
        f.depth = vec![None; f.pin.len()];
        f.endpoint[1] = true;
        f.depth[3] = Some(2);
        f.depth[9] = Some(5);
        let (paths, ends, lines) = get_end_points_timing(&f, 2);
        assert_eq!((paths, ends.into_iter().collect::<Vec<_>>(), lines), (3, vec!["out".to_string()], vec!["Found path of depth 2".to_string(), "Found path of depth 5".to_string()]));
        f.depth[3] = Some(3);
        let (_, ends, lines) = get_end_points_timing(&f, 2);
        assert_eq!((ends.into_iter().collect::<Vec<_>>(), lines.len()), (vec!["r/D".to_string()], 1));
    }

    /// Rule (`findFanins`): back from the ends through combinational logic; register outputs and
    /// ports stop it and are left out; constants are not crossed; the ends themselves are not in it.
    #[test]
    fn fanins_stop_at_register_outputs_and_ports() {
        let f = chain();
        let pins = |ends: &[&str]| find_fanins(&f, &ends.iter().map(|e| e.to_string()).collect()).into_iter().collect::<Vec<_>>();
        assert_eq!(pins(&["out"]), vec!["u2/A", "u2/B", "u2/Z"]);
        assert_eq!(pins(&["r/D"]), vec!["u1/A", "u1/Z"]);
    }
}
