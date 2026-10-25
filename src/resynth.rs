// SPDX-License-Identifier: Apache-2.0
//! `resynth`: the logic behind every end point with negative slack, re-mapped by ABC for delay and
//! put back in its place.
//!
//! [`zero_slack`] is `ZeroSlackStrategy::OptimizeDesign`: [`get_endpoints`], the cell library
//! ([`abc_library::build`]), the logic cut, the mapped network, [`delay_optimization`], and the
//! insertion.

use vyges_abc_lib::{Abc, BufferParams, MapParams, Network};
use vyges_opendb::Db;
use vyges_sta::liberty::Library;

use crate::cut::abc_library::{self, CornerLib};
use crate::cut::logic_cut::{self, UniqueName};
use crate::cut::logic_extractor::{self, View};
use crate::design::{CutDesign, OdbBlif};
use crate::restructure::{Facts, RunError};

/// What the zero-slack strategy needs besides the design and the timer.
pub struct Libraries<'a> {
    /// The default (first read) library, and the path it was read from.
    pub default: &'a Library,
    pub default_path: &'a str,
    /// The corner's libraries, in read order.
    pub corner: Vec<CornerLib<'a>>,
    /// `Resizer::dontUse`, per library index and cell name.
    pub dont_use: &'a dyn Fn(usize, &str) -> bool,
}

/// `GetEndpoints(sta, resizer, 0.0)`: the timer's end points in vertex order whose pin is an input,
/// whose pin, net and instance are not dont-touch, and whose max slack is below the threshold.
pub fn get_endpoints(f: &Facts, d: &dyn CutDesign, threshold: f32) -> Vec<usize> {
    let view = View::new(f, d);
    (0..f.pin.len())
        .filter(|&v| f.endpoint[v])
        .filter(|&v| match view.iterm(v) {
            Some((inst, term)) => view.is_input(inst, term) && !d.inst_dont_touch(inst) && !view.net_of(v).is_some_and(|n| d.net_dont_touch(&n)),
            // A top-level port's pin: an end point there is an output port's (not an input).
            None => false,
        })
        .filter(|&v| f.slack[v] < threshold)
        .collect()
}

extern "C" {
    fn fflush(stream: *mut std::ffi::c_void) -> i32;
}

/// Our buffered lines out, then ABC's (C stdio): the log interleaves as the calls ran.
pub(crate) fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // SAFETY: `fflush(NULL)` flushes every open output stream; it takes no pointer we own.
    unsafe { fflush(std::ptr::null_mut()) };
}

/// `ZeroSlackStrategy::OptimizeDesign`.
pub fn zero_slack(db: &mut Db, f: &Facts, libs: &Libraries, names: &mut UniqueName, out: &mut dyn FnMut(&str)) -> Result<(), RunError> {
    let err = RunError::Error;
    let endpoints = get_endpoints(f, &OdbBlif { db }, 0.0);
    if endpoints.is_empty() {
        out("[INFO RMP-0050] All candidate endpoints have positive slack, nothing to do.");
        return Ok(());
    }
    // ⚠️ Declared first so it is dropped last: every network and library goes before `Abc_Stop`.
    let mut abc = Abc::start().map_err(|e| err(e.to_string()))?;
    flush();
    let mut lib = abc_library::build(&abc, libs.default, libs.default_path, &libs.corner, libs.dont_use, out).map_err(err)?;
    flush();
    let cut = {
        let design = OdbBlif { db };
        logic_extractor::build_logic_cut(&View::new(f, &design), &endpoints, &lib).map_err(err)?
    };
    if cut.is_empty() {
        out("[WARNING RMP-1032] Logic cut is empty after extraction, nothing to do.");
        return Ok(());
    }
    let (mapped, _mio) = {
        let design = OdbBlif { db };
        logic_cut::build_mapped_abc_network(&abc, &cut, &mut lib, &View::new(f, &design)).map_err(err)?
    };
    let remapped = delay_optimization(&mut abc, &mapped, &lib).map_err(err)?;
    flush();
    logic_cut::insert_mapped_abc_network(&remapped, &cut, &lib, names, &mut OdbBlif { db }).map_err(err)
}

/// `DelayOptimizationStrategy::Optimize`: to a logic network, strash, balance; the network's gate
/// library installed in ABC's frame with its buffer as the driving cell (RMP-0049 without one);
/// map for delay, clean up, buffer; back to a netlist.
pub fn delay_optimization(abc: &mut Abc, ntk: &Network, lib: &abc_library::AbcLibrary) -> Result<Network, String> {
    let e = |e: vyges_abc_lib::Error| e.to_string();
    let mut current = ntk.to_logic().map_err(e)?;
    current = current.strash(false, true, false).map_err(e)?;
    current = current.balance(true, true, true).map_err(e)?;
    ntk.install_library(abc).map_err(|_| {
        "[ERROR RMP-0049] Cannot find buffer cell in abc library (ensure buffer exists in your PDK), if it does please report this internal error.".to_string()
    })?;
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
    current.cleanup(false).map_err(e)?;
    let buffers = BufferParams { gain_ratio: 300, degree: 8, size_only: false, add_bufs: true, buf_pis: true, use_wire_loads: false, verbose: false, very_verbose: false };
    current = current.buffering(&lib.cells, &buffers).map_err(e)?;
    current.to_netlist().map_err(e)
}
