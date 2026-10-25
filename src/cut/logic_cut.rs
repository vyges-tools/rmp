// SPDX-License-Identifier: Apache-2.0
//! A logic cut out to ABC as a mapped netlist, and ABC's mapped netlist back in its place.
//!
//! [`build_mapped_abc_network`] is `BuildMappedAbcNetwork`: [`create_primary_inputs`],
//! [`create_primary_outputs`], [`create_standard_cells`], [`create_nets`], then the zero-fanout
//! check. [`insert_mapped_abc_network`] is `InsertMappedAbcNetwork`: [`map_constant_cells`],
//! [`create_instances`], [`create_new_nets`], [`delete_existing_logic_cut`],
//! [`connect_instances`].

use std::collections::{BTreeMap, BTreeSet, HashMap};

use vyges_abc_lib::{Abc, GateLib, Network, ObjType, Special};

use crate::cut::abc_library::AbcLibrary;
use crate::cut::logic_extractor::{LogicCut, View};
use crate::design::CutDesign;

/// `UniqueName`: `prefix` and a counter that only goes up, kept across calls.
#[derive(Clone, Debug, Default)]
pub struct UniqueName {
    pub counter: i64,
}

impl UniqueName {
    pub fn get(&mut self, prefix: &str) -> String {
        let id = self.counter;
        self.counter += 1;
        format!("{prefix}{id}")
    }
}

fn abc_err(e: vyges_abc_lib::Error) -> String {
    e.to_string()
}

/// Names sorted by their database id.
fn by_id(names: &BTreeSet<String>, id: &dyn Fn(&str) -> u32) -> Vec<String> {
    let mut v: Vec<String> = names.iter().cloned().collect();
    v.sort_by_key(|n| id(n));
    v
}

/// `BuildMappedAbcNetwork`. Returns the network, the gate library it took, and each cut
/// instance's node.
pub fn build_mapped_abc_network(abc: &Abc, cut: &LogicCut, lib: &mut AbcLibrary, view: &View) -> Result<(Network, GateLib), String> {
    let mio = lib.take_mio().ok_or("resynth: the gate library was already taken")?;
    let mut ntk = Network::mapped_netlist(abc, mio).map_err(abc_err)?;
    let d = view.d;
    let input_nets = create_primary_inputs(&mut ntk, &by_id(&cut.primary_inputs, &|n| d.net_id(n)))?;
    let output_nets = create_primary_outputs(&mut ntk, &by_id(&cut.primary_outputs, &|n| d.net_id(n)))?;
    let cells = create_standard_cells(&mut ntk, cut, lib, mio, view)?;
    create_nets(&mut ntk, &by_id(&cut.primary_outputs, &|n| d.net_id(n)), &input_nets, &output_nets, &cells, mio, view)?;
    // `AssertAbcNetworkHasNoZeroFanoutNodes`.
    for o in 0..ntk.obj_num_max() {
        if ntk.obj_type(o) == Some(ObjType::Node) && ntk.fanouts(o).is_empty() {
            return Err("[ERROR CUT-0029] Zero fanout node emitted from ABC. Please report this internal error.".into());
        }
    }
    Ok((ntk, mio))
}

/// `CreateAbcPrimaryInputs`: per net (by id), a primary input and a net named after it.
pub fn create_primary_inputs(ntk: &mut Network, nets: &[String]) -> Result<HashMap<String, usize>, String> {
    let mut map = HashMap::new();
    for net in nets {
        let pi = ntk.create_pi().map_err(abc_err)?;
        let n = ntk.create_net().map_err(abc_err)?;
        ntk.assign_name(n, net).map_err(abc_err)?;
        ntk.add_fanin(n, pi).map_err(abc_err)?;
        map.insert(net.clone(), n);
    }
    Ok(map)
}

/// `CreateAbcPrimaryOutputs`: per net (by id), a primary output and a net named after it.
pub fn create_primary_outputs(ntk: &mut Network, nets: &[String]) -> Result<HashMap<String, usize>, String> {
    let mut map = HashMap::new();
    for net in nets {
        let po = ntk.create_po().map_err(abc_err)?;
        let n = ntk.create_net().map_err(abc_err)?;
        ntk.assign_name(n, net).map_err(abc_err)?;
        ntk.add_fanin(po, n).map_err(abc_err)?;
        map.insert(net.clone(), n);
    }
    Ok(map)
}

/// `NameToAbcGateMap`: a gate's index by name (a later gate of the same name replaces an earlier).
fn gate_by_name(mio: GateLib) -> HashMap<String, usize> {
    (0..mio.gate_count()).filter_map(|g| mio.gate_name(g).map(|n| (n, g))).collect()
}

/// `CreateStandardCells`: per cut instance (by id) a node of its cell's gate — a constant cell
/// as the gate library's constant gate — named after the instance (CUT-0024 for a cell with no
/// gate).
pub fn create_standard_cells(ntk: &mut Network, cut: &LogicCut, lib: &AbcLibrary, mio: GateLib, view: &View) -> Result<BTreeMap<String, usize>, String> {
    let gates = gate_by_name(mio);
    let mut map = BTreeMap::new();
    for inst in by_id(&cut.instances, &|i| view.d.inst_id(i)) {
        let mut cell = view.d.inst_master(&inst);
        if lib.is_const0_cell(&cell) {
            cell = mio.special(Special::Const0).and_then(|g| mio.gate_name(g)).unwrap_or_default();
        }
        if lib.is_const1_cell(&cell) {
            cell = mio.special(Special::Const1).and_then(|g| mio.gate_name(g)).unwrap_or_default();
        }
        let gate = *gates.get(&cell).ok_or_else(|| format!("[ERROR CUT-0024] cell: {cell} was not found in the ABC library. Please report this internal error."))?;
        let node = ntk.create_node(gate).map_err(abc_err)?;
        ntk.assign_name(node, &inst).map_err(abc_err)?;
        map.insert(inst, node);
    }
    Ok(map)
}

/// `CreateNets`: each primary output net (by id) driven by its driver's node; then per cut
/// instance (by id), each input in its gate's pin order connected to its driver
/// ([`connect_pin_to_driver`]).
#[allow(clippy::too_many_arguments)]
pub fn create_nets(
    ntk: &mut Network,
    output_nets: &[String],
    input_map: &HashMap<String, usize>,
    output_map: &HashMap<String, usize>,
    cells: &BTreeMap<String, usize>,
    mio: GateLib,
    view: &View,
) -> Result<(), String> {
    let mut net_map: HashMap<String, usize> = input_map.clone();
    for net in output_nets {
        let drivers = view.drivers(net);
        if drivers.len() != 1 {
            return Err(format!("resynth: cut output net {net} has {} drivers — the first by pin id is not modelled", drivers.len()));
        }
        let inst = view.pin_inst(&drivers[0]).ok_or_else(|| format!("resynth: cut output net {net} is driven by a port"))?;
        let node = *cells.get(&inst).ok_or_else(|| format!("resynth: the driver {inst} of cut output net {net} is not in the cut"))?;
        let po_net = output_map[net];
        ntk.add_fanin(po_net, node).map_err(abc_err)?;
        net_map.insert(net.clone(), po_net);
    }
    let mut by_inst: Vec<(&String, &usize)> = cells.iter().collect();
    by_inst.sort_by_key(|(i, _)| view.d.inst_id(i));
    for (inst, &node) in by_inst {
        let gate = ntk.obj_gate(node).ok_or_else(|| format!("[ERROR CUT-0028] Can't find gate for {inst}. Please report this internal error."))?;
        for port in mio.gate_pins(gate) {
            connect_pin_to_driver(ntk, inst, &port, node, &mut net_map, cells, view)?;
        }
    }
    Ok(())
}

/// `ConnectPinToDriver`: the pin's net, if already made, feeds the node; otherwise a new net, driven
/// by a new primary input when the net's one driver (CUT-0026) is a port or a sequential cell's
/// output (the net then named after the driver pin), else by the driver's node (CUT-0027).
pub fn connect_pin_to_driver(ntk: &mut Network, inst: &str, port: &str, node: usize, net_map: &mut HashMap<String, usize>, cells: &BTreeMap<String, usize>, view: &View) -> Result<(), String> {
    let pin = format!("{inst}/{port}");
    let net = view.d.inst_iterms(inst).into_iter().find(|it| it.mterm == port).and_then(|it| it.net).ok_or_else(|| format!("resynth: cut input {pin} on no net is not modelled"))?;
    if let Some(&n) = net_map.get(&net) {
        return ntk.add_fanin(node, n).map_err(abc_err);
    }
    let drivers = view.drivers(&net);
    if drivers.len() != 1 {
        return Err(format!("[ERROR CUT-0026] output_pin: {pin} has the wrong number of drivers {}. Please report this internal error.", drivers.len()));
    }
    let driver = &drivers[0];
    let abc_net = ntk.create_net().map_err(abc_err)?;
    let driver_inst = view.pin_inst(driver);
    // `libertyCell(driver instance)->isSequential()`: the timer's register flag of the pin.
    let sequential = view.vertex(driver).is_some_and(|v| view.f.register[v]);
    if driver_inst.is_none() || sequential {
        let pi = ntk.create_pi().map_err(abc_err)?;
        ntk.add_fanin(abc_net, pi).map_err(abc_err)?;
        ntk.assign_name(abc_net, driver).map_err(abc_err)?;
    } else {
        let i = driver_inst.unwrap_or_default();
        let &driver_node = cells.get(&i).ok_or_else(|| format!("[ERROR CUT-0027] ABC version of instance {i} of type {} not found. Please report this internal error.", view.d.inst_master(&i)))?;
        ntk.add_fanin(abc_net, driver_node).map_err(abc_err)?;
    }
    ntk.add_fanin(node, abc_net).map_err(abc_err)?;
    net_map.insert(net, abc_net);
    Ok(())
}

/// `InsertMappedAbcNetwork` (CUT-0045 / CUT-0046 if the network is not a mapped netlist).
pub fn insert_mapped_abc_network(ntk: &Network, cut: &LogicCut, lib: &AbcLibrary, names: &mut UniqueName, d: &mut dyn CutDesign) -> Result<(), String> {
    if !ntk.is_mapped_netlist() {
        return Err("[ERROR CUT-0045] abc_network has no mapping, please report this internal error.".into());
    }
    map_constant_cells(lib, ntk.gate_lib())?;
    let instances = create_instances(ntk, names, d)?;
    let nets = create_new_nets(ntk, names, d)?;
    delete_existing_logic_cut(cut, d)?;
    connect_instances(ntk, &instances, &nets, d)
}

/// `MapConstantCells`: the gate library's constant-0 and constant-1 gates take the names (and
/// output pin names) of the library's smallest constant cells (CUT-0043 / CUT-0044 without one).
pub fn map_constant_cells(lib: &AbcLibrary, mut mio: GateLib) -> Result<(), String> {
    let zero = lib.constant_zero_cell().ok_or("[ERROR CUT-0043] cannot find 0 valued constant tie cell.")?.clone();
    let one = lib.constant_one_cell().ok_or("[ERROR CUT-0044] cannot find 1 valued constant tie cell.")?.clone();
    for (which, (cell, pin)) in [(Special::Const0, zero), (Special::Const1, one)] {
        let g = mio.special(which).ok_or("resynth: the gate library has no constant gate")?;
        mio.rename(g, &cell, &pin).map_err(abc_err)?;
    }
    Ok(())
}

/// `CreateInstances`: per node in object order, an instance of its gate's cell named `cut_<n>`.
pub fn create_instances(ntk: &Network, names: &mut UniqueName, d: &mut dyn CutDesign) -> Result<BTreeMap<usize, String>, String> {
    let mio = ntk.gate_lib();
    let mut out = BTreeMap::new();
    for o in 0..ntk.obj_num_max() {
        if ntk.obj_type(o) != Some(ObjType::Node) {
            continue;
        }
        let cell = ntk.obj_gate(o).and_then(|g| mio.gate_name(g)).unwrap_or_default();
        if !d.master_exists(&cell) {
            return Err(format!("[ERROR CUT-0032] Could not find cell name {cell}, please report this internal error"));
        }
        let name = names.get("cut_");
        if !d.create_inst(&cell, &name)? {
            return Err(format!("resynth: instance {name} already exists"));
        }
        out.insert(o, name);
    }
    Ok(out)
}

/// `CreateNets` (insertion): each primary input's and output's net found by its name (CUT-0034),
/// then every other net, in object order, made as `cut_<n>`.
pub fn create_new_nets(ntk: &Network, names: &mut UniqueName, d: &mut dyn CutDesign) -> Result<BTreeMap<usize, String>, String> {
    let mut out = BTreeMap::new();
    for o in 0..ntk.obj_num_max() {
        let net = match ntk.obj_type(o) {
            Some(ObjType::Po) => ntk.fanins(o).first().copied(),
            Some(ObjType::Pi) => ntk.fanouts(o).first().copied(),
            _ => continue,
        };
        let Some(net) = net.filter(|&n| ntk.obj_type(n) == Some(ObjType::Net)) else {
            return Err("[ERROR CUT-0033] Primary input or output is not connected to an AIG PI/PO, please report this internal error".into());
        };
        let name = ntk.obj_name(net).unwrap_or_default();
        if !d.net_exists(&name) {
            return Err(format!("[ERROR CUT-0034] Cannot find primary net {name}"));
        }
        out.insert(net, name);
    }
    for o in 0..ntk.obj_num_max() {
        if ntk.obj_type(o) != Some(ObjType::Net) || out.contains_key(&o) {
            continue;
        }
        // `dbNetwork::makeNet(name)` uniquifies ALWAYS: the generator's name is only the base.
        let name = d.new_net_name(&names.get("cut_"))?;
        if !d.create_net(&name)? {
            return Err(format!("resynth: net {name} already exists"));
        }
        out.insert(o, name);
    }
    Ok(out)
}

/// `DeleteExistingLogicCut`: the cut's instances deleted (by id), then every net on one of their
/// terminals that is not a boundary net (by id).
pub fn delete_existing_logic_cut(cut: &LogicCut, d: &mut dyn CutDesign) -> Result<(), String> {
    let boundary: BTreeSet<&String> = cut.primary_inputs.iter().chain(&cut.primary_outputs).collect();
    let mut nets = BTreeSet::new();
    for inst in &cut.instances {
        for it in d.inst_iterms(inst) {
            if let Some(n) = it.net.filter(|n| !boundary.contains(n)) {
                nets.insert(n);
            }
        }
    }
    let mut insts: Vec<&String> = cut.instances.iter().collect();
    insts.sort_by_key(|i| d.inst_id(i));
    for i in insts {
        d.destroy_inst(i)?;
    }
    let mut nets: Vec<String> = nets.into_iter().collect();
    nets.sort_by_key(|n| d.net_id(n));
    for n in nets {
        d.destroy_net(&n)?;
    }
    Ok(())
}

/// `ConnectInstances`: per new instance (by id), each gate input in pin order to its fanin net,
/// then the gate's output to its fanout net.
pub fn connect_instances(ntk: &Network, instances: &BTreeMap<usize, String>, nets: &BTreeMap<usize, String>, d: &mut dyn CutDesign) -> Result<(), String> {
    let mio = ntk.gate_lib();
    let mut order: Vec<(&usize, &String)> = instances.iter().collect();
    order.sort_by_key(|(_, i)| d.inst_id(i));
    for (&o, inst) in order {
        let gate = ntk.obj_gate(o).ok_or_else(|| format!("[ERROR CUT-0035] Cannot find abc gate port order {inst}, please report this internal error"))?;
        let fanins = ntk.fanins(o);
        for (k, port) in mio.gate_pins(gate).iter().enumerate() {
            let net = fanins.get(k).and_then(|f| nets.get(f)).ok_or_else(|| format!("[ERROR CUT-0039] Could not find corresponding sta net for abc net of {inst}/{port}"))?;
            d.connect(inst, port, net)?;
        }
        let out_port = mio.gate_out_name(gate).unwrap_or_default();
        let net = ntk.fanouts(o).first().and_then(|f| nets.get(f)).ok_or_else(|| format!("[ERROR CUT-0042] Could not find corresponding sta net for abc net of {inst}/{out_port}"))?;
        d.connect(inst, &out_port, net)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cut::blif::{BlifDesign, ITerm};
    use vyges_abc_lib::{CellLib, Sense, Surface};

    /// Nets by name, and the database's next unique net index.
    #[derive(Default)]
    struct D {
        nets: Vec<String>,
        index: u32,
    }

    impl BlifDesign for D {
        fn inst_id(&self, _: &str) -> u32 {
            0
        }
        fn inst_master(&self, _: &str) -> String {
            String::new()
        }
        fn inst_iterms(&self, _: &str) -> Vec<ITerm> {
            Vec::new()
        }
        fn net_iterms(&self, _: &str) -> Vec<(String, String)> {
            Vec::new()
        }
        fn net_bterms(&self, _: &str) -> Vec<(String, String)> {
            Vec::new()
        }
        fn master_exists(&self, _: &str) -> bool {
            false
        }
        fn master_mterms(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn net_exists(&self, net: &str) -> bool {
            self.nets.iter().any(|n| n == net)
        }
        fn inst_exists(&self, _: &str) -> bool {
            false
        }
        fn iterm_exists(&self, _: &str, _: &str) -> bool {
            false
        }
        fn disconnect(&mut self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn destroy_net(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn destroy_inst(&mut self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn create_net(&mut self, net: &str) -> Result<bool, String> {
            self.nets.push(net.into());
            Ok(true)
        }
        fn create_inst(&mut self, _: &str, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn connect(&mut self, _: &str, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    impl CutDesign for D {
        fn net_id(&self, net: &str) -> u32 {
            self.nets.iter().position(|n| n == net).unwrap() as u32
        }
        fn bterm_net(&self, _: &str) -> Option<String> {
            None
        }
        fn inst_dont_touch(&self, _: &str) -> bool {
            false
        }
        fn net_dont_touch(&self, _: &str) -> bool {
            false
        }
        /// odb's ALWAYS rule: the base, then the next index whose name is free.
        fn new_net_name(&mut self, base: &str) -> Result<String, String> {
            loop {
                let name = format!("{base}{}", self.index);
                self.index += 1;
                if !self.net_exists(&name) {
                    return Ok(name);
                }
            }
        }
    }

    // `CreateNets` on insertion: a boundary net keeps its name (found by it), every other net is
    // `makeNet(GetUniqueName("cut_"))`, and `makeNet` uniquifies ALWAYS — the generator's `cut_<n>`
    // is only the base, the block's own next free net index is appended (`cut_7` + `3` = `cut_73`).
    #[test]
    fn a_new_net_is_the_generators_name_with_the_databases_index_appended() {
        let abc = Abc::start().unwrap();
        let mut lib = CellLib::new(&abc, "tiny", "tiny.lib", 1.0, 15, 12, -1.0).unwrap();
        for (name, inputs, func, sense) in [("INV", &["A"][..], "!A", Sense::NegativeUnate), ("NAND2", &["A", "B"][..], "!(A*B)", Sense::NegativeUnate)] {
            lib.add_cell(name, 1.0, Some(0.5)).unwrap();
            for i in inputs {
                lib.add_input_pin(i, 1.0, 1.0).unwrap();
            }
            lib.add_output_pin("Y", Some(50.0), None, func).unwrap();
            for i in inputs {
                lib.add_timing(i, sense).unwrap();
                for s in [Surface::CellRise, Surface::CellFall, Surface::RiseTransition, Surface::FallTransition] {
                    lib.add_surface(s, &[5.0, 50.0], &[1.0, 10.0], &[10.0, 20.0, 15.0, 25.0]).unwrap();
                }
            }
        }
        lib.finish().unwrap();
        let mio = lib.derive_genlib().unwrap();
        let gate = |name: &str| (0..mio.gate_count()).find(|&g| mio.gate_name(g).as_deref() == Some(name)).unwrap();
        let mut ntk = Network::mapped_netlist(&abc, mio).unwrap();
        let named = |ntk: &mut Network, name: &str| {
            let n = ntk.create_net().unwrap();
            ntk.assign_name(n, name).unwrap();
            n
        };
        let (pa, pb, po) = (ntk.create_pi().unwrap(), ntk.create_pi().unwrap(), ntk.create_po().unwrap());
        let (a, b, y) = (named(&mut ntk, "a"), named(&mut ntk, "b"), named(&mut ntk, "y"));
        let (nand, inv, mid) = (ntk.create_node(gate("NAND2")).unwrap(), ntk.create_node(gate("INV")).unwrap(), ntk.create_net().unwrap());
        for (o, f) in [(a, pa), (b, pb), (po, y), (nand, a), (nand, b), (mid, nand), (inv, mid), (y, inv)] {
            ntk.add_fanin(o, f).unwrap();
        }

        // `cut_72` is taken, so the index skips to 3.
        let mut d = D { nets: ["a", "b", "y", "cut_72"].map(String::from).to_vec(), index: 2 };
        let mut names = UniqueName { counter: 7 };
        let nets = create_new_nets(&ntk, &mut names, &mut d).unwrap();
        assert_eq!(nets.values().filter(|n| !["a", "b", "y"].contains(&n.as_str())).collect::<Vec<_>>(), ["cut_73"]);
        assert_eq!((names.counter, d.index), (8, 4));
        drop(ntk);
        mio.free();
    }
}
