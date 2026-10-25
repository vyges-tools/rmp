// SPDX-License-Identifier: Apache-2.0
//! The AIG rewrites the tuning strategies choose among, and one run of a chosen sequence over the
//! logic behind the failing end points.
//!
//! [`gia_ops`] is the operation table, an operation known by its index. [`run_gia`] is `RunGia`'s
//! sequence: the cut, the mapped network, a logic network over the cut's gate library,
//! [`init_gia`], the operations in order, [`gia_to_network`], strash, map, clean up, size, back to
//! a netlist, and the insertion. Every parameter is ABC's own command default (or the speed
//! script's) as the reference passes it.

use vyges_abc_lib::{Abc, Gia, GiaHas, MapParams, Network, Quiet, Resize, SizeParams};

use crate::cut::abc_library::AbcLibrary;
use crate::cut::logic_cut::{self, UniqueName};
use crate::cut::logic_extractor::{self, View};
use crate::design::OdbBlif;
use crate::restructure::Facts;

/// The number of operations, each named by its index.
pub const GIA_OP_COUNT: usize = 12;

/// The operation indices in table order: `&st`, `&dch` + `&equiv_reduce`, `&syn2`, `&syn3`,
/// `&syn4`, `&retime`, `&dc2`, `&b`, `&b -d`, `&reduce`, `&if -g -K 6`, `&synch2`.
pub fn gia_ops() -> Vec<usize> {
    (0..GIA_OP_COUNT).collect()
}

fn e(err: vyges_abc_lib::Error) -> String {
    err.to_string()
}

/// Choices first when the manager has none (`&dch` with its defaults).
fn ensure_choices(g: &mut Gia) -> Result<(), String> {
    if !g.has(GiaHas::Choices) {
        g.dch(None).map_err(e)?;
    }
    Ok(())
}

/// One operation, replacing the manager.
pub fn run_op(g: &mut Gia, op: usize) -> Result<(), String> {
    match op {
        0 => g.rehash(false),
        1 => {
            ensure_choices(g)?;
            g.equiv_reduce(true, false, false, false)
        }
        2 => g.syn2(false, true, false, 20, false, false, false),
        3 => g.syn3(false, false),
        4 => g.syn4(false, false),
        5 => g.retime_forward(100, false),
        6 => g.compress2(true, false),
        // `ABC_INFINITY` new nodes.
        7 => g.area_balance(false, 1_000_000_000, false, false),
        8 => g.balance(false, false, false),
        9 => {
            ensure_choices(g)?;
            g.equiv_reduce_and_remap(true, false)
        }
        10 => {
            if g.has(GiaHas::Mapping) {
                g.rehash(false).map_err(e)?;
            }
            g.if_map(true, 6, true, true, false)
        }
        11 => g.synch2(100, 6, 20),
        _ => return Err(format!("no GIA operation {op}")),
    }
    .map_err(e)
}

/// `initGia`: the logic network strashed, through an AIG, latches zeroed, names kept.
pub fn init_gia(abc: &Abc, ntk: &Network) -> Result<Gia, String> {
    Gia::from_network(abc, ntk).map_err(e)
}

/// The manager back to a network: by its cell mapping, its LUT mapping (or MUXes), else as an
/// AIG (rehashed first when it has dangling nodes); then the CI and CO names, and CIs and COs of
/// one name decoupled.
pub fn gia_to_network(abc: &mut Abc, g: &mut Gia) -> Result<Network, String> {
    abc.getopt_reset();
    let mut ntk = if g.has(GiaHas::CellMapping) {
        Network::from_cell_mapped_gia(g, false).map_err(e)?
    } else if g.has(GiaHas::Mapping) || g.has(GiaHas::Muxes) {
        Network::from_mapped_gia(g, false, false).map_err(e)?
    } else {
        if g.has(GiaHas::Dangling) {
            g.rehash(false).map_err(e)?;
        }
        Network::from_gia_aig(g).map_err(e)?
    };
    ntk.name_from_gia(g).map_err(e)?;
    if !ntk.is_strash() {
        ntk.redirect_ci_co().map_err(e)?;
    }
    Ok(ntk)
}

/// `RunGia`.
#[allow(clippy::too_many_arguments)]
pub fn run_gia(abc: &mut Abc, db: &mut vyges_opendb::Db, f: &Facts, candidates: &[usize], lib: &mut AbcLibrary, ops: &[usize], resize_iters: i32, names: &mut UniqueName) -> Result<(), String> {
    let cut = {
        let design = OdbBlif { db };
        logic_extractor::build_logic_cut(&View::new(f, &design), candidates, lib)?
    };
    let (mapped, mio) = {
        let design = OdbBlif { db };
        logic_cut::build_mapped_abc_network(abc, &cut, lib, &View::new(f, &design))?
    };
    let mut current = mapped.to_logic().map_err(e)?;
    abc.set_lib_gen(mio).map_err(e)?;
    current.set_gate_lib(mio);
    {
        let mut g = init_gia(abc, &current)?;
        for &op in ops {
            run_op(&mut g, op)?;
        }
        current = gia_to_network(abc, &mut g)?;
    }
    if !current.is_strash() {
        current = current.strash(false, true, false).map_err(e)?;
    }
    {
        let _quiet = Quiet::new();
        let map = MapParams {
            delay_target: 1.0,
            area_multi: 0.0,
            delay_multi: 2.5,
            log_fan: 0.0,
            slew: 0.0,
            gain: 250.0,
            gates_min: 0,
            recovery: true,
            switching: false,
            skip_fanout: false,
            use_profile: false,
            use_buffs: false,
            verbose: false,
        };
        current = current.map(&map).map_err(e)?;
    }
    current.cleanup(false).map_err(e)?;
    current = current.dup_dfs().map_err(e)?;
    if resize_iters > 0 {
        let _quiet = Quiet::new();
        let p = SizeParams { iters: resize_iters, iter_no_change: 50, window: 1, ratio: 10, notches: 1000, use_dept: true };
        current.resize(&lib.cells, Resize::Up, &p).map_err(e)?;
        current.resize(&lib.cells, Resize::Down, &p).map_err(e)?;
    }
    current = current.to_netlist().map_err(e)?;
    logic_cut::insert_mapped_abc_network(&current, &cut, lib, names, &mut OdbBlif { db })
}
