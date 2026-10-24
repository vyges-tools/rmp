// SPDX-License-Identifier: Apache-2.0
//! BLIF out of and back into the design: the cut a re-synthesis runs on.
//!
//! - [`write_blif`] — the instances to optimise (in database id order) as `.gate` / `.mlatch` lines,
//!   the nets that cross the cut as inputs and outputs, constant drivers as `_const0_` / `_const1_`;
//! - [`read_blif`] — the instances destroyed, then the file's gates created and connected;
//! - [`preprocess_string`] and [`parse`] — comment removal and the grammar, as the reference reads.
//!
//! The design is read and edited through [`BlifDesign`]; database edits happen in the reference's
//! order, because instance and net ids are what the written design shows.

use std::collections::{BTreeMap, BTreeSet};

use vyges_sta::func_expr::FuncExpr;
use vyges_sta::liberty::{Library, Port};

/// One instance terminal as the writer reads it.
#[derive(Debug, Clone)]
pub struct ITerm {
    pub mterm: String,
    /// `dbSigType` as odb spells it (`SIGNAL`, `POWER`, `GROUND`, `CLOCK`, …).
    pub sig_type: String,
    /// `dbIoType` as odb spells it (`INPUT`, `OUTPUT`, `INOUT`, `FEEDTHRU`).
    pub io_type: String,
    pub net: Option<String>,
}

/// What the BLIF writer and reader read and edit.
pub trait BlifDesign {
    fn inst_id(&self, inst: &str) -> u32;
    fn inst_master(&self, inst: &str) -> String;
    /// The instance's terminals in `getITerms()` order.
    fn inst_iterms(&self, inst: &str) -> Vec<ITerm>;
    /// The net's instance terminals in `getITerms()` order, as `(instance, mterm)`.
    fn net_iterms(&self, net: &str) -> Vec<(String, String)>;
    /// The net's ports in `getBTerms()` order, as `(name, io type)`.
    fn net_bterms(&self, net: &str) -> Vec<(String, String)>;
    /// `findMaster` over the database's libraries in order.
    fn master_exists(&self, master: &str) -> bool;
    /// The master's terminals in `getMTerms()` order.
    fn master_mterms(&self, master: &str) -> Vec<String>;
    fn net_exists(&self, net: &str) -> bool;
    fn inst_exists(&self, inst: &str) -> bool;
    fn iterm_exists(&self, inst: &str, mterm: &str) -> bool;
    fn disconnect(&mut self, inst: &str, mterm: &str) -> Result<(), String>;
    fn destroy_net(&mut self, net: &str) -> Result<(), String>;
    fn destroy_inst(&mut self, inst: &str) -> Result<(), String>;
    /// `dbNet::create`; `false` where odb returns nullptr.
    fn create_net(&mut self, net: &str) -> Result<bool, String>;
    /// `dbInst::create`; `false` where odb returns nullptr.
    fn create_inst(&mut self, master: &str, name: &str) -> Result<bool, String>;
    fn connect(&mut self, inst: &str, mterm: &str, net: &str) -> Result<(), String>;
}

/// The constant cells, as the constructor takes them (and as the writer may replace them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConstCells {
    pub const0_cell: String,
    pub const0_port: String,
    pub const1_cell: String,
    pub const1_port: String,
}

/// Why the writer or reader stops: the reference's error line, or a refusal where the reference
/// would dereference a null (no Liberty cell or port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlifError {
    Error(String),
    Refused(String),
}

fn refused(e: String) -> BlifError {
    BlifError::Refused(e)
}

/// `libertyPort(master/port)->isClock()`. A `pg_pin` is a Liberty port too (never a clock); a
/// terminal with no Liberty port is refused (the reference dereferences it).
fn port_is_clock(lib: &Library, master: &str, port: &str) -> Result<bool, BlifError> {
    let cell = lib.cells.get(master).ok_or_else(|| refused(format!("master {master} has no Liberty cell")))?;
    if let Some(p) = cell.top_level_ports().into_iter().find(|p| p.name == port) {
        return Ok(p.is_clock);
    }
    if cell.pg_pins.iter().any(|(n, _)| n == port) {
        return Ok(false);
    }
    Err(refused(format!("{master}/{port} has no Liberty port")))
}

/// A port whose function is the constant 0 or 1 (a tristate output is not constant).
fn constant_of(port: &Port) -> Option<bool> {
    if port.three_state.is_some() {
        return None;
    }
    match port.function.as_deref().map(FuncExpr::parse) {
        Some(Ok(FuncExpr::Zero)) => Some(false),
        Some(Ok(FuncExpr::One)) => Some(true),
        _ => None,
    }
}

/// The file text and the CUT-0002 count.
pub struct Written {
    pub text: String,
    pub instances: usize,
}

/// `writeBlif` (arrivals and requireds not written). `insts` in any order: they are taken in id
/// order, as the reference's pointer set holds them.
pub fn write_blif(d: &dyn BlifDesign, lib: &Library, insts: &[String], consts: &mut ConstCells) -> Result<Written, BlifError> {
    let mut insts: Vec<&String> = insts.iter().collect::<BTreeSet<_>>().into_iter().collect();
    insts.sort_by_key(|i| d.inst_id(i));
    let in_cut: BTreeSet<&str> = insts.iter().map(|i| i.as_str()).collect();
    let mut dummy_nets = 0usize;
    let mut subckts = vec![String::new(); insts.len()];
    let mut inst_index = 0usize;
    let (mut inputs, mut outputs, mut const0, mut const1, mut clocks) =
        (BTreeSet::<String>::new(), BTreeSet::<String>::new(), BTreeSet::<String>::new(), BTreeSet::<String>::new(), BTreeSet::<String>::new());
    for inst in &insts {
        let master = d.inst_master(inst);
        let cell = lib.cells.get(&master).ok_or_else(|| refused(format!("{inst}: master {master} has no Liberty cell")))?;
        let mut current_gate = format!("{}{master}", if cell.is_sequential() { ".mlatch " } else { ".gate " });
        let (mut current_connections, mut current_clock) = (String::new(), String::new());
        let mut current_clocks = BTreeSet::new();
        for iterm in d.inst_iterms(inst) {
            if iterm.sig_type == "POWER" || iterm.sig_type == "GROUND" {
                continue;
            }
            if port_is_clock(lib, &master, &iterm.mterm)? {
                let Some(net) = &iterm.net else { continue };
                clocks.insert(net.clone());
                current_clocks.insert(net.clone());
                current_clock = net.clone();
                continue;
            }
            let net_name = match &iterm.net {
                None => {
                    let n = format!("dummy_{dummy_nets}");
                    dummy_nets += 1;
                    n
                }
                Some(n) => n.clone(),
            };
            current_connections += &format!(" {}={net_name}", iterm.mterm);
            let Some(net) = &iterm.net else { continue };
            // An input or output when the net reaches only this terminal, or a terminal outside.
            let connected = d.net_iterms(net);
            if connected.len() == 1 {
                if iterm.io_type == "INPUT" {
                    inputs.insert(net_name.clone());
                } else if iterm.io_type == "OUTPUT" {
                    outputs.insert(net_name.clone());
                }
            } else {
                let mut add_as_input = false;
                for (other, other_mterm) in &connected {
                    if in_cut.contains(other.as_str()) {
                        continue;
                    }
                    if iterm.io_type == "INPUT" {
                        // Driven from outside: a constant driver becomes a constant gate.
                        let other_master = d.inst_master(other);
                        let other_port = lib.cells.get(&other_master).and_then(|c| c.top_level_ports().into_iter().find(|p| &p.name == other_mterm));
                        match other_port.and_then(|p| constant_of(p).map(|v| (p, v))) {
                            Some((p, false)) => {
                                if const0.is_empty() {
                                    consts.const0_cell = other_master.clone();
                                    consts.const0_port = p.name.clone();
                                }
                                const0.insert(net_name.clone());
                            }
                            Some((p, true)) => {
                                if const1.is_empty() {
                                    consts.const1_cell = other_master.clone();
                                    consts.const1_port = p.name.clone();
                                }
                                const1.insert(net_name.clone());
                            }
                            None => add_as_input = true,
                        }
                    } else if iterm.io_type == "OUTPUT" {
                        outputs.insert(net_name.clone());
                    }
                }
                if add_as_input && !const0.contains(&net_name) && !const1.contains(&net_name) {
                    inputs.insert(net_name.clone());
                }
            }
            // The design's own ports, where the net is none of these yet. (A top port has no
            // Liberty port, so an input port is an input.)
            if !inputs.contains(&net_name) && !outputs.contains(&net_name) && !const0.contains(&net_name) && !const1.contains(&net_name) {
                for (_, io) in d.net_bterms(net) {
                    if io == "INPUT" {
                        inputs.insert(net_name.clone());
                    } else if io == "OUTPUT" {
                        outputs.insert(net_name.clone());
                    }
                }
            }
        }
        current_gate += &current_connections;
        if cell.is_sequential() && current_clocks.len() != 1 {
            continue;
        }
        if cell.is_sequential() {
            current_gate += &format!(" {current_clock}");
        }
        subckts[inst_index] = current_gate;
        inst_index += 1;
    }
    // A net that is both is driven inside: not an input.
    let common: Vec<String> = inputs.intersection(&outputs).cloned().collect();
    for p in common {
        inputs.remove(&p);
    }
    let mut f = String::from(".model tmp_circuit\n.inputs");
    for input in &inputs {
        if const0.contains(input) || const1.contains(input) {
            continue;
        }
        f += &format!(" {input}");
    }
    f += "\n.outputs";
    for output in &outputs {
        f += &format!(" {output}");
    }
    f += "\n";
    if !clocks.is_empty() {
        // (no line break of its own)
        f += ".clock";
        for clock in &clocks {
            f += &format!(" {clock}");
        }
    }
    f += "\n\n";
    for zero in &const0 {
        f += &format!(".gate _const0_ z={zero}\n");
    }
    for one in &const1 {
        f += &format!(".gate _const1_ z={one}\n");
    }
    // Every slot is written, the unfilled ones (a register without exactly one clock) as blank lines.
    for s in &subckts {
        f += s;
        f += "\n";
    }
    f += ".end\n";
    Ok(Written { text: f, instances: inst_index })
}

/// `preprocessString`: a line starting with `#` is removed. ⚠️ After a removal the scan resumes
/// from the removed line's END as it stood, so the line that followed it is not examined: of two
/// comment lines in a row, the second stays. The last line, with no line break after it, is never
/// examined.
pub fn preprocess_string(s: &mut Vec<u8>) {
    let mut old: isize = -1;
    loop {
        let start = (old + 1) as usize;
        let Some(ind) = (start < s.len()).then(|| s[start..].iter().position(|&c| c == b'\n').map(|k| start + k)).flatten() else { break };
        if s[start] == b'#' {
            s.drain(start..=ind);
        }
        old = ind as isize;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateType {
    Mlatch,
    Gate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gate {
    pub kind: GateType,
    pub master: String,
    pub connections: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub clocks: Vec<String>,
    pub gates: Vec<Gate>,
    pub comb_count: usize,
    pub flop_count: usize,
}

/// The grammar, as a PEG over bytes with the reference parser's semantics:
/// - a token is a run of ASCII bytes other than space, CR and LF; whitespace (ASCII `isspace`) is
///   skipped before every token and keyword;
/// - a keyword matches a PREFIX of what follows (`.outputsX` is `.outputs` then `X`);
/// - inputs end at `.outputs`; outputs end at `.gate` or `.clock` (NOT `.mlatch`); clocks end at
///   `.gate` (NOT `.mlatch`); a gate's connections end at `.gate`, `.mlatch` or `.end`;
/// - each list needs one item; nothing is backtracked once consumed; text after `.end` is ignored.
///
/// `None` when the file does not parse.
pub fn parse(text: &[u8]) -> Option<Parsed> {
    let mut p = Parser { s: text, at: 0 };
    let mut out = Parsed::default();
    p.lit(b".model")?;
    p.token()?;
    p.lit(b".inputs")?;
    let inputs = p.list(&[b".outputs"])?;
    out.inputs = inputs.into_iter().filter(|t| t != "\\").collect();
    p.lit(b".outputs")?;
    out.outputs = p.list(&[b".gate", b".clock"])?.into_iter().filter(|t| t != "\\").collect();
    // Optional: tried, and abandoned whole when it does not complete.
    let save = p.at;
    match p.lit(b".clock").and_then(|_| p.list(&[b".gate"])) {
        Some(clocks) => out.clocks = clocks.into_iter().filter(|t| t != "\\").collect(),
        None => p.at = save,
    }
    // One or more gates.
    let mut current: Option<(GateType, String, Vec<String>)> = None;
    let mut gates = 0;
    loop {
        let save = p.at;
        let kind = if p.lit(b".gate").is_some() {
            GateType::Gate
        } else if p.lit(b".mlatch").is_some() {
            GateType::Mlatch
        } else {
            break;
        };
        // The keyword's action runs as soon as it matches: the previous gate is closed.
        if let Some((k, m, c)) = current.take() {
            out.gates.push(Gate { kind: k, master: m, connections: c });
        }
        match kind {
            GateType::Mlatch => out.flop_count += 1,
            GateType::Gate => out.comb_count += 1,
        }
        let rest = p.token().and_then(|master| p.list(&[b".gate", b".mlatch", b".end"]).map(|c| (master, c)));
        match rest {
            Some((master, connections)) => {
                current = Some((kind, master, connections));
                gates += 1;
            }
            None => {
                p.at = save;
                break;
            }
        }
    }
    if gates == 0 {
        return None;
    }
    p.lit(b".end")?;
    if let Some((k, m, c)) = current {
        out.gates.push(Gate { kind: k, master: m, connections: c });
    }
    Some(out)
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn skip(&mut self) {
        while self.at < self.s.len() && matches!(self.s[self.at], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            self.at += 1;
        }
    }

    fn lit(&mut self, k: &[u8]) -> Option<()> {
        let save = self.at;
        self.skip();
        if self.s[self.at..].starts_with(k) {
            self.at += k.len();
            Some(())
        } else {
            self.at = save;
            None
        }
    }

    fn peek(&mut self, k: &[u8]) -> bool {
        let save = self.at;
        let r = self.lit(k).is_some();
        self.at = save;
        r
    }

    fn token(&mut self) -> Option<String> {
        let save = self.at;
        self.skip();
        let start = self.at;
        while self.at < self.s.len() && self.s[self.at] < 128 && !matches!(self.s[self.at], b' ' | b'\n' | b'\r') {
            self.at += 1;
        }
        if self.at == start {
            self.at = save;
            return None;
        }
        Some(String::from_utf8_lossy(&self.s[start..self.at]).into_owned())
    }

    /// One or more tokens, each where none of `stops` follows.
    fn list(&mut self, stops: &[&[u8]]) -> Option<Vec<String>> {
        let mut v = Vec::new();
        loop {
            if stops.iter().any(|k| self.peek(k)) {
                break;
            }
            match self.token() {
                Some(t) => v.push(t),
                None => break,
            }
        }
        (!v.is_empty()).then_some(v)
    }
}

/// What `readBlif` logs, in order.
pub type Log = Vec<String>;

/// `readBlif`: the file parsed, the instances to optimise destroyed (id order; a net left with no
/// terminal destroyed with them), then each gate created (named `<master>_<call>_<n>`) and
/// connected, a constant as a tie cell (named `<cell>_<call><n>`, no separator).
pub fn read_blif(
    d: &mut dyn BlifDesign,
    lib: &Library,
    insts: &[String],
    consts: &ConstCells,
    call_id: i32,
    file_text: &[u8],
    log: &mut Log,
) -> Result<(), BlifError> {
    let mut text = file_text.to_vec();
    preprocess_string(&mut text);
    let Some(blif) = parse(&text) else {
        return Err(BlifError::Error("[ERROR CUT-0005] Blif parser failed. File doesn't follow blif spec.".into()));
    };
    let mut insts: Vec<&String> = insts.iter().collect::<BTreeSet<_>>().into_iter().collect();
    insts.sort_by_key(|i| d.inst_id(i));
    log.push(format!("[INFO CUT-0006] Blif parsed successfully, will destroy {} existing instances.", insts.len()));
    log.push(format!(
        "[INFO CUT-0007] Found {} inputs, {} outputs, {} clocks, {} combinational gates, {} registers after parsing the blif file.",
        blif.inputs.len(),
        blif.outputs.len(),
        blif.clocks.len(),
        blif.comb_count,
        blif.flop_count
    ));
    for inst in &insts {
        for iterm in d.inst_iterms(inst) {
            d.disconnect(inst, &iterm.mterm).map_err(refused)?;
            if let Some(net) = &iterm.net {
                if d.net_iterms(net).is_empty() && d.net_bterms(net).is_empty() {
                    d.destroy_net(net).map_err(refused)?;
                }
            }
        }
        d.destroy_inst(inst).map_err(refused)?;
    }
    log.push(format!("[INFO CUT-0008] Inserting {} new instances.", blif.gates.len()));
    let mut inst_ids: BTreeMap<String, i64> = BTreeMap::new();
    let call = call_id.to_string();
    for gate in &blif.gates {
        let master_found = d.master_exists(&gate.master);
        if !master_found && (gate.master == "_const0_" || gate.master == "_const1_") {
            let Some(first) = gate.connections.first() else {
                log.push(format!("[INFO CUT-0009] Const driver {} doesn't have any connected nets.", gate.master));
                continue;
            };
            let const_net_name = &first[first.find('=').map_or(0, |k| k + 1)..];
            let mut net = const_net_name.to_string();
            if !d.net_exists(&net) {
                net = format!("or_{call}{const_net_name}");
                if !d.create_net(&net).map_err(refused)? {
                    return Err(refused(format!("net {net} could not be created")));
                }
            }
            let (cell, port) = if gate.master == "_const0_" { (&consts.const0_cell, &consts.const0_port) } else { (&consts.const1_cell, &consts.const1_port) };
            let n = inst_ids.entry(cell.clone()).or_insert(0);
            *n += 1;
            let mut name = format!("{cell}_{call}{n}");
            if d.master_exists(cell) {
                while d.inst_exists(&name) {
                    *n += 1;
                    name = format!("{cell}_{call}{n}");
                }
                if !d.create_inst(cell, &name).map_err(refused)? {
                    return Err(refused(format!("tie cell {name} could not be created")));
                }
                if !d.iterm_exists(&name, port) {
                    return Err(refused(format!("tie cell {cell} has no port {port}")));
                }
                d.connect(&name, port, &net).map_err(refused)?;
            }
            continue;
        }
        if !master_found {
            log.push(format!("[INFO CUT-0010] Master ({}) not found while stitching back instances.", gate.master));
            continue;
        }
        let n = inst_ids.entry(gate.master.clone()).or_insert(0);
        *n += 1;
        let mut name = format!("{}_{call}_{n}", gate.master);
        while d.inst_exists(&name) {
            *n += 1;
            name = format!("{}_{call}_{n}", gate.master);
        }
        if !d.create_inst(&gate.master, &name).map_err(refused)? {
            return Err(BlifError::Error(format!("[ERROR CUT-0011] Could not create new instance of type {} with name {name}.", gate.master)));
        }
        for connection in &gate.connections {
            let (mterm, net_name) = match connection.find('=') {
                None if gate.kind == GateType::Mlatch => {
                    // The clock: the master's first Liberty clock port.
                    let mut found = (String::new(), String::new());
                    for m in d.master_mterms(&gate.master) {
                        if port_is_clock(lib, &gate.master, &m)? {
                            found = (m, connection.clone());
                            break;
                        }
                    }
                    found
                }
                None => continue,
                Some(k) if k == connection.len() - 1 => {
                    log.push(format!("[INFO CUT-0012] Connection {connection} parsing failed for {} instance.", gate.master));
                    continue;
                }
                Some(k) => (connection[..k].to_string(), connection[k + 1..].to_string()),
            };
            // The net is found or made before the terminal is checked.
            let mut net = net_name.clone();
            if !d.net_exists(&net) {
                net = format!("or_{call}{net_name}");
                if !d.net_exists(&net) && !d.create_net(&net).map_err(refused)? {
                    return Err(refused(format!("net {net} could not be created")));
                }
            }
            if mterm.is_empty() {
                log.push(format!(
                    "[INFO CUT-0013] Could not connect instance of cell type {} to {net_name} net due to unknown mterm in blif.",
                    gate.master
                ));
                continue;
            }
            if !d.iterm_exists(&name, &mterm) {
                return Err(refused(format!("{} has no terminal {mterm}", gate.master)));
            }
            d.connect(&name, &mterm, &net).map_err(refused)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn of_two_comment_lines_in_a_row_the_second_stays() {
        // Rule (preprocessString): after erasing a line the scan resumes past where it ended.
        let mut s = b"#a\n#b\nc\n#d\ne\n#f".to_vec();
        preprocess_string(&mut s);
        assert_eq!(String::from_utf8(s).unwrap(), "#b\nc\ne\n#f");
    }

    fn gates(p: &Parsed) -> Vec<(GateType, &str, Vec<&str>)> {
        p.gates.iter().map(|g| (g.kind, g.master.as_str(), g.connections.iter().map(String::as_str).collect())).collect()
    }

    #[test]
    fn the_grammar_reads_the_writers_files() {
        let p = parse(b".model tmp_circuit\n.inputs n1 n2\n.outputs n8\n.clock clk\n\n.gate AND2_X1 A1=n1 A2=n2 ZN=n5\n.mlatch DFF_X1 D=n5 Q=n8 clk\n.end\n").unwrap();
        assert_eq!((p.inputs.clone(), p.outputs.clone(), p.clocks.clone()), (vec!["n1".into(), "n2".into()], vec!["n8".into()], vec!["clk".into()]));
        assert_eq!(gates(&p), vec![(GateType::Gate, "AND2_X1", vec!["A1=n1", "A2=n2", "ZN=n5"]), (GateType::Mlatch, "DFF_X1", vec!["D=n5", "Q=n8", "clk"])]);
        assert_eq!((p.comb_count, p.flop_count), (1, 1));
    }

    #[test]
    fn keywords_match_a_prefix_and_lists_stop_where_the_reference_stops() {
        // `.outputsX`: the keyword, then the output `X`.
        let p = parse(b".model m .inputs a .outputsX .gate G A=a .end").unwrap();
        assert_eq!(p.outputs, vec!["X".to_string()]);
        // Outputs do not stop at `.mlatch`: with no `.clock` and no `.gate`, the latch is read as
        // outputs and no gate is left: invalid.
        assert!(parse(b".model m .inputs a .outputs y .mlatch D Q=y c .end").is_none());
        // Clocks do not stop at `.mlatch` either: everything up to the `.gate` is a clock.
        let p = parse(b".model m .inputs a .outputs y .clock c .mlatch D Q=y c .gate G A=a .end").unwrap();
        assert_eq!(p.clocks, vec!["c", ".mlatch", "D", "Q=y", "c"]);
        assert_eq!(p.flop_count, 0);
        // A gate needs a connection; every list needs one item.
        assert!(parse(b".model m .inputs a .outputs y .gate G .end").is_none());
        assert!(parse(b".model m .inputs .outputs y .gate G A=a .end").is_none());
        // A line continuation is consumed but not kept; text after `.end` is ignored.
        let p = parse(b".model m .inputs a \\\n b .outputs y .gate G A=a .endfoo trailing").unwrap();
        assert_eq!(p.inputs, vec!["a", "b"]);
        // Tabs are skipped before a token but are part of one; a byte past ASCII ends it.
        let p = parse(b".model m .inputs\ta\tb .outputs y .gate G A=a .end").unwrap();
        assert_eq!(p.inputs, vec!["a\tb"]);
    }

    // ---- the writer and reader on a design held in memory ----

    use vyges_sta::liberty_parse::parse as parse_lib;

    const LIB: &str = r#"library (t) {
      cell (AND2) { pin (A) { direction : input; } pin (B) { direction : input; }
        pin (Z) { direction : output; function : "A&B"; } }
      cell (DFF) { ff (IQ, IQN) { clocked_on : "CK"; next_state : "D"; }
        pg_pin (VDD) { pg_type : primary_power; }
        pin (D) { direction : input; } pin (CK) { direction : input; clock : true; }
        pin (Q) { direction : output; function : "IQ"; } }
      cell (TIE0) { pin (Z) { direction : output; function : "0"; } }
      cell (TIE1) { pin (Z) { direction : output; function : "1"; } }
      cell (TIE0B) { pin (Z) { direction : output; function : "0"; } }
      cell (TBUF0) { pin (EN) { direction : input; } pin (Z) { direction : output; function : "0"; three_state : "!EN"; } }
      cell (DFF2) { ff (IQ, IQN) { clocked_on : "CK"; next_state : "D"; }
        pin (D) { direction : input; } pin (CK) { direction : input; clock : true; }
        pin (CK2) { direction : input; clock : true; } pin (Q) { direction : output; function : "IQ"; } }
    }"#;

    fn lib() -> Library {
        Library::read(&parse_lib(LIB).unwrap()).unwrap()
    }

    #[derive(Default)]
    struct D {
        /// (name, master, id)
        insts: Vec<(String, String, u32)>,
        /// net -> its terminals in order
        nets: Vec<(String, Vec<(String, String)>)>,
        /// (port, io, net)
        ports: Vec<(String, String, String)>,
        ops: Vec<String>,
        next_id: u32,
    }

    fn pins(master: &str) -> Vec<(&'static str, &'static str, &'static str)> {
        match master {
            "AND2" => vec![("A", "SIGNAL", "INPUT"), ("B", "SIGNAL", "INPUT"), ("Z", "SIGNAL", "OUTPUT")],
            "DFF" => vec![("VDD", "POWER", "INOUT"), ("D", "SIGNAL", "INPUT"), ("CK", "CLOCK", "INPUT"), ("Q", "SIGNAL", "OUTPUT")],
            "DFF2" => vec![("D", "SIGNAL", "INPUT"), ("CK", "CLOCK", "INPUT"), ("CK2", "CLOCK", "INPUT"), ("Q", "SIGNAL", "OUTPUT")],
            "TBUF0" => vec![("EN", "SIGNAL", "INPUT"), ("Z", "SIGNAL", "OUTPUT")],
            _ => vec![("Z", "SIGNAL", "OUTPUT")],
        }
    }

    impl D {
        fn inst(mut self, name: &str, master: &str, id: u32, conns: &[(&str, &str)]) -> D {
            self.insts.push((name.into(), master.into(), id));
            for (m, n) in conns {
                match self.nets.iter_mut().find(|(x, _)| x == n) {
                    Some((_, t)) => t.push((name.into(), m.to_string())),
                    None => self.nets.push((n.to_string(), vec![(name.into(), m.to_string())])),
                }
            }
            self.next_id = self.next_id.max(id + 1);
            self
        }
        fn port(mut self, name: &str, io: &str, net: &str) -> D {
            self.ports.push((name.into(), io.into(), net.into()));
            if !self.nets.iter().any(|(x, _)| x == net) {
                self.nets.push((net.into(), Vec::new()));
            }
            self
        }
        fn net_of(&self, inst: &str, m: &str) -> Option<String> {
            self.nets.iter().find(|(_, t)| t.iter().any(|(i, x)| i == inst && x == m)).map(|(n, _)| n.clone())
        }
    }

    impl BlifDesign for D {
        fn inst_id(&self, inst: &str) -> u32 {
            self.insts.iter().find(|i| i.0 == inst).unwrap().2
        }
        fn inst_master(&self, inst: &str) -> String {
            self.insts.iter().find(|i| i.0 == inst).map(|i| i.1.clone()).unwrap_or_default()
        }
        fn inst_iterms(&self, inst: &str) -> Vec<ITerm> {
            pins(&self.inst_master(inst))
                .into_iter()
                .map(|(m, sig, io)| ITerm { mterm: m.into(), sig_type: sig.into(), io_type: io.into(), net: self.net_of(inst, m) })
                .collect()
        }
        fn net_iterms(&self, net: &str) -> Vec<(String, String)> {
            self.nets.iter().find(|(n, _)| n == net).map(|(_, t)| t.clone()).unwrap_or_default()
        }
        fn net_bterms(&self, net: &str) -> Vec<(String, String)> {
            self.ports.iter().filter(|p| p.2 == net).map(|p| (p.0.clone(), p.1.clone())).collect()
        }
        fn master_exists(&self, master: &str) -> bool {
            ["AND2", "DFF", "DFF2", "TIE0", "TIE0B", "TIE1", "TBUF0"].contains(&master)
        }
        fn master_mterms(&self, master: &str) -> Vec<String> {
            pins(master).into_iter().map(|p| p.0.to_string()).collect()
        }
        fn net_exists(&self, net: &str) -> bool {
            self.nets.iter().any(|(n, _)| n == net)
        }
        fn inst_exists(&self, inst: &str) -> bool {
            self.insts.iter().any(|i| i.0 == inst)
        }
        fn iterm_exists(&self, inst: &str, mterm: &str) -> bool {
            pins(&self.inst_master(inst)).iter().any(|p| p.0 == mterm)
        }
        fn disconnect(&mut self, inst: &str, mterm: &str) -> Result<(), String> {
            for (_, t) in &mut self.nets {
                t.retain(|(i, m)| !(i == inst && m == mterm));
            }
            Ok(())
        }
        fn destroy_net(&mut self, net: &str) -> Result<(), String> {
            self.ops.push(format!("destroy net {net}"));
            self.nets.retain(|(n, _)| n != net);
            Ok(())
        }
        fn destroy_inst(&mut self, inst: &str) -> Result<(), String> {
            self.ops.push(format!("destroy {inst}"));
            self.insts.retain(|i| i.0 != inst);
            Ok(())
        }
        fn create_net(&mut self, net: &str) -> Result<bool, String> {
            self.ops.push(format!("net {net}"));
            self.nets.push((net.into(), Vec::new()));
            Ok(true)
        }
        fn create_inst(&mut self, master: &str, name: &str) -> Result<bool, String> {
            self.ops.push(format!("inst {name} {master}"));
            let id = self.next_id;
            self.next_id += 1;
            self.insts.push((name.into(), master.into(), id));
            Ok(true)
        }
        fn connect(&mut self, inst: &str, mterm: &str, net: &str) -> Result<(), String> {
            self.ops.push(format!("connect {inst}/{mterm} {net}"));
            self.nets.iter_mut().find(|(n, _)| n == net).unwrap().1.push((inst.into(), mterm.into()));
            Ok(())
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_writer_takes_instances_in_id_order_and_crossing_nets_as_ports() {
        // Rules (writeBlif): id order, not the order given; a net to an outside load is an
        // output; a net from outside is an input; an unconnected pin is `dummy_<n>`; a net that is
        // both (driven inside) is not an input; the sets print sorted.
        let d = D::default()
            .inst("g1", "AND2", 7, &[("A", "n3"), ("B", "n5"), ("Z", "n6")])
            .inst("g2", "AND2", 3, &[("Z", "n5")])
            .inst("drv", "AND2", 1, &[("Z", "n3")])
            .inst("load", "AND2", 9, &[("A", "n6")]);
        let mut c = ConstCells::default();
        let w = write_blif(&d, &lib(), &names(&["g1", "g2"]), &mut c).unwrap();
        assert_eq!(
            w.text,
            ".model tmp_circuit\n.inputs n3\n.outputs n6\n\n\n.gate AND2 A=dummy_0 B=dummy_1 Z=n5\n.gate AND2 A=n3 B=n5 Z=n6\n.end\n"
        );
        assert_eq!(w.instances, 2);
    }

    #[test]
    fn a_constant_driver_becomes_a_constant_gate_and_names_the_tie_cell() {
        let d = D::default()
            .inst("g", "AND2", 5, &[("A", "lo"), ("B", "hi"), ("Z", "o")])
            .inst("t0", "TIE0", 1, &[("Z", "lo")])
            .inst("t1", "TIE1", 2, &[("Z", "hi")])
            .port("po", "OUTPUT", "o");
        let mut c = ConstCells::default();
        let w = write_blif(&d, &lib(), &names(&["g"]), &mut c).unwrap();
        assert_eq!(w.text, ".model tmp_circuit\n.inputs\n.outputs o\n\n\n.gate _const0_ z=lo\n.gate _const1_ z=hi\n.gate AND2 A=lo B=hi Z=o\n.end\n");
        assert_eq!(c, ConstCells { const0_cell: "TIE0".into(), const0_port: "Z".into(), const1_cell: "TIE1".into(), const1_port: "Z".into() });
    }

    #[test]
    fn a_register_names_its_clock_last_and_one_without_a_clock_leaves_a_blank_slot() {
        // Rules: a clock pin is not a connection; the clock line has no line break of its own;
        // a register with no clock net is dropped but its slot is still written (blank).
        let d = D::default()
            .inst("r", "DFF", 2, &[("D", "d"), ("CK", "clk"), ("Q", "q")])
            .inst("r2", "DFF", 3, &[("D", "d")])
            .port("pd", "INPUT", "d")
            .port("pq", "OUTPUT", "q")
            .port("pclk", "INPUT", "clk");
        let mut c = ConstCells::default();
        let w = write_blif(&d, &lib(), &names(&["r", "r2"]), &mut c).unwrap();
        assert_eq!(w.text, ".model tmp_circuit\n.inputs d\n.outputs q\n.clock clk\n\n.mlatch DFF D=d Q=q clk\n\n.end\n");
        assert_eq!(w.instances, 1);
    }

    #[test]
    fn the_first_constant_driver_names_the_tie_cell_and_a_tristate_is_no_constant() {
        // Rules: the tie cell is taken from the FIRST constant net found; a tristate output with
        // a constant function is a plain input.
        let d = D::default()
            .inst("g", "AND2", 5, &[("A", "lo"), ("B", "lo2"), ("Z", "o")])
            .inst("h", "AND2", 6, &[("A", "z"), ("Z", "o2")])
            .inst("t0", "TIE0B", 1, &[("Z", "lo")])
            .inst("t1", "TIE0", 2, &[("Z", "lo2")])
            .inst("tb", "TBUF0", 3, &[("Z", "z")])
            .port("po", "OUTPUT", "o")
            .port("po2", "OUTPUT", "o2");
        let mut c = ConstCells::default();
        let w = write_blif(&d, &lib(), &names(&["g", "h"]), &mut c).unwrap();
        assert_eq!(c.const0_cell, "TIE0B");
        assert!(w.text.starts_with(".model tmp_circuit\n.inputs z\n.outputs o o2\n"), "{}", w.text);
    }

    #[test]
    fn a_net_driven_inside_and_read_outside_is_an_output_only() {
        // Rules: an input whose net also has an outside load is an input (that load is not
        // constant), the driver makes it an output, and a net that is both is not an input. A net
        // already classified is not looked at for ports.
        let d = D::default()
            .inst("a", "AND2", 1, &[("Z", "n")])
            .inst("b", "AND2", 2, &[("A", "n"), ("Z", "m")])
            .inst("out", "AND2", 3, &[("A", "n"), ("B", "m")])
            .inst("drv", "AND2", 4, &[("Z", "x")])
            .inst("c", "AND2", 5, &[("A", "x")])
            .port("px", "OUTPUT", "x");
        let mut c = ConstCells::default();
        let w = write_blif(&d, &lib(), &names(&["a", "b", "c"]), &mut c).unwrap();
        assert!(w.text.starts_with(".model tmp_circuit\n.inputs x\n.outputs m n\n"), "{}", w.text);
        // A register on two clock nets is dropped.
        let d = D::default().inst("r", "DFF2", 1, &[("D", "d"), ("CK", "c1"), ("CK2", "c2"), ("Q", "q")]).port("pq", "OUTPUT", "q");
        let w = write_blif(&d, &lib(), &names(&["r"]), &mut c).unwrap();
        assert_eq!(w.instances, 0);
    }

    #[test]
    fn the_reader_destroys_in_id_order_then_creates_named_by_master_and_call() {
        // Rules (readBlif): each instance disconnected and destroyed (a net left with nothing
        // destroyed); then `<master>_<call>_<n>`; a missing net is made as `or_<call><net>`; a
        // constant is a tie cell `<cell>_<call><n>` (no separator); an unknown master is logged.
        let mut d = D::default()
            .inst("b", "AND2", 4, &[("A", "x"), ("Z", "mid")])
            .inst("a", "AND2", 2, &[("A", "mid"), ("Z", "y")])
            .port("px", "INPUT", "x")
            .port("py", "OUTPUT", "y");
        let c = ConstCells { const0_cell: "TIE0".into(), const0_port: "Z".into(), ..Default::default() };
        let mut log = Vec::new();
        let blif = b".model m\n.inputs x\n.outputs y\n\n.gate _const0_ z=k\n.gate AND2 A=x B=k Z=y\n.gate NOPE A=x\n.gate AND2 A=x Z=new\n.end\n";
        read_blif(&mut d, &lib(), &names(&["b", "a"]), &c, 3, blif, &mut log).unwrap();
        assert_eq!(
            d.ops,
            vec![
                "destroy a", "destroy net mid", "destroy b",
                "net or_3k", "inst TIE0_31 TIE0", "connect TIE0_31/Z or_3k",
                "inst AND2_3_1 AND2", "connect AND2_3_1/A x", "connect AND2_3_1/B or_3k", "connect AND2_3_1/Z y",
                "inst AND2_3_2 AND2", "connect AND2_3_2/A x", "net or_3new", "connect AND2_3_2/Z or_3new",
            ]
        );
        assert_eq!(
            log,
            vec![
                "[INFO CUT-0006] Blif parsed successfully, will destroy 2 existing instances.",
                "[INFO CUT-0007] Found 1 inputs, 1 outputs, 0 clocks, 4 combinational gates, 0 registers after parsing the blif file.",
                "[INFO CUT-0008] Inserting 4 new instances.",
                "[INFO CUT-0010] Master (NOPE) not found while stitching back instances.",
            ]
        );
    }

    #[test]
    fn a_registers_bare_net_is_its_clock_and_names_skip_taken_ones() {
        let mut d = D::default().inst("DFF_1_1", "DFF", 1, &[]).port("pc", "INPUT", "c");
        let mut log = Vec::new();
        read_blif(&mut d, &lib(), &[], &ConstCells::default(), 1, b".model m .inputs c .outputs q .clock c .gate AND2 A=c Z=w= .mlatch DFF D=w Q=q c .end", &mut log).unwrap();
        // `Z=w=`: split at the FIRST `=`, so the net is `w=`.
        assert!(d.ops.contains(&"connect DFF_1_2/CK c".to_string()), "{:?}", d.ops);
        assert!(d.ops.contains(&"inst DFF_1_2 DFF".to_string()));
        assert!(d.ops.contains(&"net or_1w=".to_string()));
        let mut d = D::default();
        let mut log = Vec::new();
        read_blif(&mut d, &lib(), &[], &ConstCells::default(), 1, b".model m .inputs c .outputs q .gate AND2 A= Z=q .end", &mut log).unwrap();
        assert_eq!(log.last().unwrap(), "[INFO CUT-0012] Connection A= parsing failed for AND2 instance.");
        // A bare net on a register master with no clock port: no terminal, but the net (`or_<call>`
        // plus the empty name) is still made first.
        let mut d = D::default();
        let mut log = Vec::new();
        read_blif(&mut d, &lib(), &[], &ConstCells::default(), 1, b".model m .inputs c .outputs q .clock c .gate AND2 A=c Z=q .mlatch AND2 A=c c .end", &mut log).unwrap();
        assert!(d.ops.contains(&"net or_1".to_string()), "{:?}", d.ops);
        assert_eq!(log.last().unwrap(), "[INFO CUT-0013] Could not connect instance of cell type AND2 to  net due to unknown mterm in blif.");
        assert_eq!(read_blif(&mut d, &lib(), &[], &ConstCells::default(), 1, b".model", &mut log), Err(BlifError::Error("[ERROR CUT-0005] Blif parser failed. File doesn't follow blif spec.".into())));
    }
}
