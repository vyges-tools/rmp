// SPDX-License-Identifier: Apache-2.0
//! The logic cut behind a set of end points: the combinational cells ABC can re-map, back from the
//! end points to registers, ports and unsupported cells.
//!
//! [`build_logic_cut`] is `BuildLogicCut`'s sequence: [`get_cut_vertices`],
//! [`add_missing_vertices`], [`get_primary_inputs`], [`get_primary_outputs`],
//! [`get_cut_instances`], [`filter_undriven_outputs`], then the boundary pins' nets. Every result
//! the reference keeps in a hashed or pointer-ordered set is a SET here: its consumers sort it by id
//! before anything depends on its order.

use std::collections::{BTreeSet, HashMap};

use crate::cut::abc_library::AbcLibrary;
use crate::design::CutDesign;
use crate::restructure::Facts;

/// The timer's vertices over the design: a vertex's pin is `inst/term` or a port's name.
pub struct View<'a> {
    pub f: &'a Facts,
    pub d: &'a dyn CutDesign,
    by_pin: HashMap<&'a str, usize>,
}

impl<'a> View<'a> {
    pub fn new(f: &'a Facts, d: &'a dyn CutDesign) -> View<'a> {
        View { f, d, by_pin: f.pin.iter().enumerate().map(|(v, p)| (p.as_str(), v)).collect() }
    }

    /// `graph->vertex(network->vertexId(pin))`.
    pub fn vertex(&self, pin: &str) -> Option<usize> {
        self.by_pin.get(pin).copied()
    }

    /// The instance and terminal of vertex `v`'s pin; `None` for a port.
    pub fn iterm(&self, v: usize) -> Option<(&str, &str)> {
        let inst = self.f.inst[v].as_deref()?;
        Some((inst, &self.f.pin[v][inst.len() + 1..]))
    }

    /// `network->net(pin)` (a port's `net(term(pin))`).
    pub fn net_of(&self, v: usize) -> Option<String> {
        match self.iterm(v) {
            Some((i, t)) => Some(self.d.inst_iterms(i).into_iter().find(|it| it.mterm == t)?.net?),
            None => self.d.bterm_net(&self.f.pin[v]),
        }
    }

    /// `network->direction(pin)` as input / output: an instance terminal's own direction (power
    /// and ground terminals are neither); a port as its direction.
    pub fn is_input(&self, inst: &str, mterm: &str) -> bool {
        self.d.inst_iterms(inst).iter().any(|it| it.mterm == mterm && it.io_type == "INPUT" && is_signal(&it.sig_type))
    }

    pub fn is_output(&self, inst: &str, mterm: &str) -> bool {
        self.d.inst_iterms(inst).iter().any(|it| it.mterm == mterm && it.io_type == "OUTPUT" && is_signal(&it.sig_type))
    }

    /// `network->connectedPinIterator`: every pin on the net, instance terminals then ports, as
    /// pin names.
    pub fn net_pins(&self, net: &str) -> Vec<String> {
        let mut pins: Vec<String> = self.d.net_iterms(net).into_iter().map(|(i, t)| format!("{i}/{t}")).collect();
        pins.extend(self.d.net_bterms(net).into_iter().map(|(b, _)| b));
        pins
    }

    /// `network->drivers(net)`: the net's output (or inout) instance terminals and its input (or
    /// inout) ports, as pin names.
    pub fn drivers(&self, net: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .d
            .net_iterms(net)
            .into_iter()
            .filter(|(i, t)| self.d.inst_iterms(i).iter().any(|it| &it.mterm == t && matches!(it.io_type.as_str(), "OUTPUT" | "INOUT") && is_signal(&it.sig_type)))
            .map(|(i, t)| format!("{i}/{t}"))
            .collect();
        out.extend(self.d.net_bterms(net).into_iter().filter(|(_, io)| matches!(io.as_str(), "INPUT" | "INOUT")).map(|(b, _)| b));
        out
    }

    /// The instance a pin name belongs to; `None` for a port.
    pub fn pin_inst(&self, pin: &str) -> Option<String> {
        match self.vertex(pin) {
            Some(v) => self.f.inst[v].clone(),
            None => pin.rsplit_once('/').map(|(i, _)| i.to_string()),
        }
    }
}

fn is_signal(sig_type: &str) -> bool {
    !matches!(sig_type, "POWER" | "GROUND")
}

/// The cut: its boundary nets (sets, by name) and its instances (a set, by name).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogicCut {
    pub primary_inputs: BTreeSet<String>,
    pub primary_outputs: BTreeSet<String>,
    pub instances: BTreeSet<String>,
}

impl LogicCut {
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }
}

/// `BuildLogicCut`.
pub fn build_logic_cut(view: &View, endpoints: &[usize], lib: &AbcLibrary) -> Result<LogicCut, String> {
    let cut_vertices = get_cut_vertices(view, endpoints, lib);
    let cut_vertices = add_missing_vertices(view, endpoints, &cut_vertices, lib)?;
    let primary_inputs = get_primary_inputs(view, endpoints, &cut_vertices);
    let primary_outputs = get_primary_outputs(view, endpoints, &cut_vertices);
    let instances = get_cut_instances(view, endpoints, &cut_vertices);
    let filtered = filter_undriven_outputs(view, &primary_outputs, &instances);
    Ok(LogicCut { primary_inputs: convert_io_pins_to_nets(view, &primary_inputs)?, primary_outputs: convert_io_pins_to_nets(view, &filtered)?, instances })
}

/// `GetCutVertices`: a backward breadth-first search from the end points with
/// `SearchPredCombAbcSupport` — a vertex's fanin is entered only when the vertex is searched to
/// (not a constant), through an edge `SearchPred1` crosses that is not a register clock-to-Q or
/// latch D-to-Q edge, from a vertex that is not a constant and whose instance's cell is a
/// supported (single-output) library cell. Returns the vertices reached, the end points included.
pub fn get_cut_vertices(view: &View, endpoints: &[usize], lib: &AbcLibrary) -> BTreeSet<usize> {
    let f = view.f;
    let supported = |v: usize| f.inst[v].as_deref().is_some_and(|i| lib.is_supported_cell(&view.d.inst_master(i)));
    let mut seen: BTreeSet<usize> = endpoints.iter().copied().collect();
    let mut queue: Vec<usize> = endpoints.to_vec();
    while let Some(v) = queue.pop() {
        if !f.to_ok[v] {
            continue;
        }
        for &e in &f.in_edges[v] {
            let (from, _, thru) = f.edges[e];
            if f.from_ok[from] && thru && supported(from) && seen.insert(from) {
                queue.push(from);
            }
        }
    }
    seen
}

/// `AddMissingVertices`: every input terminal of the instance of each non-end-point cut vertex
/// joins the cut, and, where the terminal is constant, the tie cell driving it (the net's one
/// driver, CUT-0047 otherwise) when its cell is one of the library's constant cells.
pub fn add_missing_vertices(view: &View, endpoints: &[usize], cut: &BTreeSet<usize>, lib: &AbcLibrary) -> Result<BTreeSet<usize>, String> {
    let ends: BTreeSet<usize> = endpoints.iter().copied().collect();
    let mut out = cut.clone();
    for &v in cut {
        if ends.contains(&v) {
            continue;
        }
        let Some(inst) = view.f.inst[v].clone() else {
            return Err(format!("resynth: port {} inside the cut (the top instance's pins) is not modelled", view.f.pin[v]));
        };
        for it in view.d.inst_iterms(&inst) {
            let pin = format!("{inst}/{}", it.mterm);
            let Some(u) = view.vertex(&pin) else { continue };
            if out.contains(&u) || !view.is_input(&inst, &it.mterm) {
                continue;
            }
            out.insert(u);
            if view.f.from_ok[u] {
                continue;
            }
            let drivers = it.net.as_deref().map(|n| view.drivers(n)).unwrap_or_default();
            if drivers.len() != 1 {
                return Err(format!("[ERROR CUT-0047] constant vertex: {pin} should have exactly one constant driver. Has {}, please report this internal error.", drivers.len()));
            }
            let Some(driver_inst) = view.pin_inst(&drivers[0]) else {
                return Err(format!("resynth: {pin} is constant from port {} (the top instance's cell) — not modelled", drivers[0]));
            };
            if !lib.is_const_cell(&view.d.inst_master(&driver_inst)) {
                continue;
            }
            if let Some(c) = view.vertex(&drivers[0]) {
                out.insert(c);
            }
        }
    }
    Ok(out)
}

/// `GetPrimaryInputs`: each non-end-point cut vertex on a net with drivers, none of them in the
/// cut.
pub fn get_primary_inputs(view: &View, endpoints: &[usize], cut: &BTreeSet<usize>) -> BTreeSet<usize> {
    let pins: BTreeSet<&str> = cut.iter().map(|&v| view.f.pin[v].as_str()).collect();
    cut.iter()
        .copied()
        .filter(|v| !endpoints.contains(v))
        .filter(|&v| {
            let drivers = view.net_of(v).map(|n| view.drivers(&n)).unwrap_or_default();
            !drivers.is_empty() && !drivers.iter().any(|d| pins.contains(d.as_str()))
        })
        .collect()
}

/// `GetPrimaryOutputs`: the end points, and every pin outside the cut on the net of a cut output
/// terminal.
pub fn get_primary_outputs(view: &View, endpoints: &[usize], cut: &BTreeSet<usize>) -> BTreeSet<usize> {
    let pins: BTreeSet<&str> = cut.iter().map(|&v| view.f.pin[v].as_str()).collect();
    let mut out: BTreeSet<usize> = endpoints.iter().copied().collect();
    for &v in cut {
        let Some((inst, term)) = view.iterm(v) else { continue };
        if !view.is_output(inst, term) {
            continue;
        }
        let Some(net) = view.net_of(v) else { continue };
        for p in view.net_pins(&net) {
            if !pins.contains(p.as_str()) {
                if let Some(u) = view.vertex(&p) {
                    out.insert(u);
                }
            }
        }
    }
    out
}

/// `GetCutInstances`: the instances of the cut's vertices, less the end points' instances.
pub fn get_cut_instances(view: &View, endpoints: &[usize], cut: &BTreeSet<usize>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = cut.iter().filter_map(|&v| view.f.inst[v].clone()).collect();
    for &v in endpoints {
        if let Some(i) = &view.f.inst[v] {
            out.remove(i);
        }
    }
    out
}

/// `FilterUndrivenOutputs`: the primary outputs whose net an output terminal of a cut instance
/// drives.
pub fn filter_undriven_outputs(view: &View, outputs: &BTreeSet<usize>, instances: &BTreeSet<String>) -> BTreeSet<usize> {
    outputs
        .iter()
        .copied()
        .filter(|&v| {
            view.net_of(v).is_some_and(|net| {
                view.d.net_iterms(&net).iter().any(|(i, t)| instances.contains(i) && view.is_output(i, t))
            })
        })
        .collect()
}

/// `ConvertIoPinsToNets`: the nets of the pins (CUT-0048 for a pin on no net).
pub fn convert_io_pins_to_nets(view: &View, pins: &BTreeSet<usize>) -> Result<BTreeSet<String>, String> {
    pins.iter().map(|&v| view.net_of(v).ok_or_else(|| format!("[ERROR CUT-0048] primary input pin {} connected to null net", view.f.pin[v]))).collect()
}
