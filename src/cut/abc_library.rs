// SPDX-License-Identifier: Apache-2.0
//! The cell library a re-synthesis maps to: the corner's combinational Liberty cells, as an ABC
//! standard-cell library and the gate library derived from it.
//!
//! Stages in the reference's order: [`build`] (`Build`) runs [`populate_library_details`] on the
//! DEFAULT library (the first read: its name, units and default slew), then
//! [`populate_from_liberty`] over the corner's cells, then ABC's normalize / hash / link, and
//! derives the gate library.

use std::collections::BTreeSet;

use vyges_abc_lib::{Abc, CellLib, GateLib, Sense, Surface};
use vyges_sta::func_expr::FuncExpr;
use vyges_sta::liberty::{Cell, Direction, Library, Model, Port, FALL, MAX, RISE};
use vyges_sta::table::Table;

/// `Unit::staToUser`: a value in seconds / farads / watts over the unit's scale, in double.
fn sta_to_user(value: f32, scale: f32) -> f64 {
    f64::from(value) / f64::from(scale)
}

/// The library's units as the standard-cell library keeps them: capacitance `fst * 10^-snd`
/// farads, time `10^-unit_time` seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Units {
    pub unit_cap_fst: f32,
    pub unit_cap_snd: i32,
    pub unit_time: i32,
}

/// `StaCapacitanceToAbc`, `ScaleAbbreviationToExponent`, `StaTimeUnitToAbcInt`.
///
/// Rules: the capacitance multiplier is the scale times `10^floor(|log10 |scale||)` (the logarithm
/// in float, the product in double, kept as float); its exponent comes from the unit's scale
/// abbreviation (`m` 3, `u` 6, `n` 9, `p` 12, `f` 15 — the abbreviation is the scale fuzzily equal
/// to that power, else CUT-0023); the time unit is `floor(|log10 |scale||)`, which must lie in
/// 9..=12 (CUT-0022).
pub fn units(lib: &Library) -> Result<Units, String> {
    let cap = lib.cap_scale;
    let exponent_y = cap.abs().log10().abs().floor() as i32;
    let unit_cap_fst = (f64::from(cap) * 10f64.powf(f64::from(exponent_y))) as f32;
    let unit_cap_snd = [(1e-3f32, 3), (1e-6, 6), (1e-9, 9), (1e-12, 12), (1e-15, 15)]
        .iter()
        .find(|(s, _)| vyges_sta::fuzzy::equal(cap, *s))
        .map(|&(_, e)| e)
        .ok_or_else(|| format!("[ERROR CUT-0023] Can't convert scale abbreviation of capacitance unit {cap:e}"))?;
    let scale = lib.time_scale.abs().log10().abs();
    let unit_time = scale.floor() as i32;
    if !(9..=12).contains(&unit_time) {
        return Err(format!("[ERROR CUT-0022] Time unit {scale} outside abc range [1e-9, 1e-12]"));
    }
    Ok(Units { unit_cap_fst, unit_cap_snd, unit_time })
}

/// A Liberty port's direction as `PortDirection` reads it: an output with a `three_state` is a
/// tristate.
fn is_input(p: &Port) -> bool {
    p.direction == Direction::Input
}

fn is_output(p: &Port) -> bool {
    p.direction == Direction::Output && !p.is_any_tristate()
}

/// `isCompatibleWithAbc`: not dont-use, combinational (`IsCombinational`: not a clock gate, pad,
/// macro, sequential, level shifter, isolation cell or memory), at least one output, and no port
/// but inputs and outputs (power and ground pins are kept apart and do not count).
pub fn is_compatible(cell: &Cell, dont_use: bool) -> bool {
    !dont_use
        && !(cell.is_clock_gate || cell.is_pad || cell.is_macro || cell.is_sequential() || cell.is_level_shifter || cell.is_isolation_cell || cell.is_memory)
        && cell.ports.iter().any(is_output)
        && cell.ports.iter().all(|p| is_input(p) || is_output(p))
}

/// The standard-cell library, its gate library (until a network takes it), and the cell facts
/// the extractor and the insertion ask.
pub struct AbcLibrary {
    pub cells: CellLib,
    mio: Option<GateLib>,
    /// `IsSupportedCell`: cells with exactly one output.
    supported: BTreeSet<String>,
    /// `InitializeConstGates`: cells with no inputs whose first pin's truth table is all zeros /
    /// all ones.
    const0: BTreeSet<String>,
    const1: BTreeSet<String>,
    /// `ConstantZeroCell` / `ConstantOneCell`: the smallest-area such cell (the first of equal
    /// areas) and its output pin.
    const0_cell: Option<(String, String)>,
    const1_cell: Option<(String, String)>,
}

impl AbcLibrary {
    pub fn is_supported_cell(&self, name: &str) -> bool {
        self.supported.contains(name)
    }

    pub fn is_const0_cell(&self, name: &str) -> bool {
        self.const0.contains(name)
    }

    pub fn is_const1_cell(&self, name: &str) -> bool {
        self.const1.contains(name)
    }

    pub fn is_const_cell(&self, name: &str) -> bool {
        self.is_const1_cell(name) || self.is_const0_cell(name)
    }

    pub fn constant_zero_cell(&self) -> Option<&(String, String)> {
        self.const0_cell.as_ref()
    }

    pub fn constant_one_cell(&self) -> Option<&(String, String)> {
        self.const1_cell.as_ref()
    }

    /// `mio_library()`: the gate library, handed over (the network built over it, and then ABC's
    /// frame, keep it); once handed over, each call derives a new one with standard output
    /// suppressed (its "Derived GENLIB" line is not printed again).
    pub fn take_mio(&mut self) -> Result<GateLib, String> {
        match self.mio.take() {
            Some(m) => Ok(m),
            None => {
                let _quiet = vyges_abc_lib::Quiet::new();
                self.cells.derive_genlib().map_err(|e| e.to_string())
            }
        }
    }
}

impl Drop for AbcLibrary {
    /// A gate library no network took is the library's to free.
    fn drop(&mut self) {
        if let Some(m) = self.mio.take() {
            m.free();
        }
    }
}

/// One Liberty library of the corner: its index among all libraries read (dont-use is kept per
/// library cell) and the library.
pub struct CornerLib<'a> {
    pub index: usize,
    pub lib: &'a Library,
}

/// `AbcLibraryFactory::Build`.
pub fn build(abc: &Abc, default: &Library, default_path: &str, corner: &[CornerLib], dont_use: &dyn Fn(usize, &str) -> bool, out: &mut dyn FnMut(&str)) -> Result<AbcLibrary, String> {
    let mut cells = populate_library_details(abc, default, default_path)?;
    populate_from_liberty(&mut cells, corner, default, dont_use, out)?;
    cells.finish().map_err(|e| e.to_string())?;
    let mio = cells.derive_genlib().map_err(|e| e.to_string())?;
    let info = cells.cells();
    let supported = info.iter().filter(|c| c.outputs == 1).map(|c| c.name.clone()).collect();
    let (mut const0, mut const1) = (BTreeSet::new(), BTreeSet::new());
    for (i, c) in info.iter().enumerate().filter(|(_, c)| c.inputs == 0) {
        match cells.pin_truth(i, 0) {
            Some(0) => {
                const0.insert(c.name.clone());
            }
            Some(u64::MAX) => {
                const1.insert(c.name.clone());
            }
            _ => {}
        }
    }
    let constant_cell = |value: u64| {
        let mut best: Option<(f32, String, String)> = None;
        for (i, c) in info.iter().enumerate().filter(|(_, c)| c.inputs == 0) {
            for pin in c.inputs..c.inputs + c.outputs {
                if cells.pin_truth(i, pin) == Some(value) && best.as_ref().is_none_or(|(a, _, _)| c.area < *a) {
                    best = Some((c.area, c.name.clone(), cells.pin_name(i, pin).unwrap_or_default()));
                }
            }
        }
        best.map(|(_, c, p)| (c, p))
    };
    let (const0_cell, const1_cell) = (constant_cell(0), constant_cell(u64::MAX));
    Ok(AbcLibrary { cells, mio: Some(mio), supported, const0, const1, const0_cell, const1_cell })
}

/// `PopulateLibraryDetails`: the library's name and file name, units, no wire loads, and its
/// default maximum slew in its time unit (-1 without one).
pub fn populate_library_details(abc: &Abc, lib: &Library, path: &str) -> Result<CellLib, String> {
    let u = units(lib)?;
    let slew = lib.default_max_transition.map_or(-1.0, |s| sta_to_user(s, lib.time_scale) as f32);
    CellLib::new(abc, &lib.name, path, u.unit_cap_fst, u.unit_cap_snd, u.unit_time, slew).map_err(|e| e.to_string())
}

/// `PopulateAbcSclLibFromSta`: every compatible cell of the corner's libraries, library by
/// library in read order and each library's cells by name; its area; its leakage (the cell's own
/// `cell_leakage_power`, else the average of its `leakage_power` groups, else none — CUT-0021),
/// in the DEFAULT library's power unit; its input pins, then its output pins.
pub fn populate_from_liberty(cells: &mut CellLib, corner: &[CornerLib], default: &Library, dont_use: &dyn Fn(usize, &str) -> bool, out: &mut dyn FnMut(&str)) -> Result<(), String> {
    for c in corner {
        for cell in c.lib.cells.values() {
            if !is_compatible(cell, dont_use(c.index, &cell.name)) {
                continue;
            }
            let average = cell.leakage_powers.iter().copied().reduce(|a, b| a + b);
            let leakage = match (cell.leakage_power, average) {
                (Some(l), _) => Some(sta_to_user(l, default.power_scale) as f32),
                (None, Some(sum)) => Some(sta_to_user(sum / cell.leakage_powers.len() as f32, default.power_scale) as f32),
                (None, None) => {
                    out(&format!("[WARNING CUT-0021] Leakage power doesn't exist for cell {}", cell.name));
                    None
                }
            };
            cells.add_cell(&cell.name, cell.area, leakage).map_err(|e| e.to_string())?;
            create_input_pins(cells, cell, c.lib)?;
            create_output_pins(cells, cell, c.lib)?;
        }
    }
    Ok(())
}

/// `CreateAbcInputPins`: each input port in port order, its rise and fall capacitance (max).
fn create_input_pins(cells: &mut CellLib, cell: &Cell, lib: &Library) -> Result<(), String> {
    for p in cell.ports.iter().filter(|p| is_input(p)) {
        let cap = |rf: usize| sta_to_user(p.capacitance(rf, MAX), lib.cap_scale) as f32;
        cells.add_input_pin(&p.name, cap(RISE), cap(FALL)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `CreateAbcOutputPins`: each output port in port order, its capacitance and slew limits where
/// set, its function as the timer prints it (CUT-0049 without one), and ONE timing per input pin:
/// the first arc set into the port from that pin (an output-to-output set is skipped), its sense
/// (CUT-0017 if the timing group gave none), and the delay and slew tables of the set's LAST arc
/// to a rise and the last to a fall.
fn create_output_pins(cells: &mut CellLib, cell: &Cell, lib: &Library) -> Result<(), String> {
    for p in cell.ports.iter().filter(|p| is_output(p)) {
        let max_cap = p.max_capacitance.map(|c| sta_to_user(c, lib.cap_scale) as f32);
        let max_slew = p.max_transition.map(|s| sta_to_user(s, lib.time_scale) as f32);
        let func = p.function.as_deref().ok_or_else(|| format!("[ERROR CUT-0049] cell port function is null for cell {}:{}", cell.name, p.name))?;
        let text = FuncExpr::parse(func).map_err(|e| format!("{}:{}: {e}", cell.name, p.name))?.to_text();
        cells.add_output_pin(&p.name, max_cap, max_slew, &text).map_err(|e| e.to_string())?;
        let mut seen = BTreeSet::new();
        for (k, set) in cell.arc_sets.iter().enumerate().filter(|(_, s)| s.to == p.name) {
            if cell.port(&set.from).is_some_and(|f| f.direction == Direction::Output) {
                continue;
            }
            if !seen.insert(set.from.clone()) {
                continue;
            }
            let sense = match cell.arc_set_senses.get(k).and_then(|s| s.as_deref()) {
                Some("positive_unate") => Sense::PositiveUnate,
                Some("negative_unate") => Sense::NegativeUnate,
                Some("non_unate") => Sense::NonUnate,
                _ => return Err(format!("[ERROR CUT-0017] Unrecognized sta::TimingSense -> {}/{} to {}", cell.name, set.from, p.name)),
            };
            cells.add_timing(&set.from, sense).map_err(|e| e.to_string())?;
            let model = |rf: usize| {
                set.arcs.iter().rev().find(|a| a.to_rf == rf).and_then(|a| match &a.model {
                    Model::Gate(g) => Some(g),
                    Model::Check(_) => None,
                })
            };
            let (Some(rise), Some(fall)) = (model(RISE), model(FALL)) else {
                return Err(format!("[ERROR CUT-0018] rise/fall Gate model is empty for cell {}. Need timing information", cell.name));
            };
            for (which, table) in [
                (Surface::CellRise, &rise.delay),
                (Surface::CellFall, &fall.delay),
                (Surface::RiseTransition, &rise.slew),
                (Surface::FallTransition, &fall.slew),
            ] {
                let table = table.as_ref().ok_or_else(|| format!("[ERROR CUT-0018] rise/fall Gate model is empty for cell {}. Need timing information", cell.name))?;
                let (i0, i1, v) = surface(table, lib)?;
                cells.add_surface(which, &i0, &i1, &v).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// `AbcPopulateAbcSurfaceFromSta`: a table of order 2 (CUT-0014 otherwise); its first axis in the
/// time unit and its second in the capacitance unit, whatever their variables; its values in the
/// time unit, row by row.
/// A surface's first axis, second axis and values, in the library's units.
type SurfaceData = (Vec<f32>, Vec<f32>, Vec<f32>);

fn surface(t: &Table, lib: &Library) -> Result<SurfaceData, String> {
    if t.axes.len() != 2 {
        return Err(format!("[ERROR CUT-0014] sta::TableModel order not equal to 2, order: {}", t.axes.len()));
    }
    let i0 = t.axes[0].values.iter().map(|&v| sta_to_user(v, lib.time_scale) as f32).collect();
    let i1 = t.axes[1].values.iter().map(|&v| sta_to_user(v, lib.cap_scale) as f32).collect();
    let v = t.values.iter().map(|&v| sta_to_user(v, lib.time_scale) as f32).collect();
    Ok((i0, i1, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(text: &str) -> Library {
        Library::read(&vyges_sta::liberty_parse::parse(text).unwrap()).unwrap()
    }

    /// Rules (`StaCapacitanceToAbc`, `ScaleAbbreviationToExponent`, `StaTimeUnitToAbcInt`): 1ff is
    /// `1 x 10^-15`, 1pf `1 x 10^-12`; 1ps is 12, 1ns 9; 10ps is outside nothing but 100ff has no
    /// abbreviation.
    #[test]
    fn units_are_split_as_the_reference_splits_them() {
        let u = |cap: &str, time: &str| units(&lib(&format!("library (l) {{ time_unit : \"{time}\" ; capacitive_load_unit ({cap}) ; }}")));
        assert_eq!(u("1, ff", "1ps").unwrap(), Units { unit_cap_fst: 1.0, unit_cap_snd: 15, unit_time: 12 });
        assert_eq!(u("1, pf", "1ns").unwrap(), Units { unit_cap_fst: 1.0, unit_cap_snd: 12, unit_time: 9 });
        assert_eq!(u("1, ff", "10ps").unwrap().unit_time, 11);
        assert!(u("100, ff", "1ns").unwrap_err().contains("CUT-0023"));
        assert!(u("1, ff", "1us").unwrap_err().contains("CUT-0022"));
    }

    /// Rule (`isCompatibleWithAbc`): dont-use, sequential, macro, memory, pad, and cells with a
    /// tristate output or no output are left out; a plain gate is kept.
    #[test]
    fn only_plain_combinational_cells_are_compatible() {
        let l = lib(r#"library (l) {
            cell (G) { pin (A) { direction : input ; } pin (Y) { direction : output ; function : "!A" ; } }
            cell (T) { pin (A) { direction : input ; } pin (Y) { direction : output ; function : "A" ; three_state : "A" ; } }
            cell (M) { is_macro_cell : true ; pin (A) { direction : input ; } pin (Y) { direction : output ; function : "A" ; } }
            cell (N) { pin (A) { direction : input ; } }
            cell (F) { ff (IQ, IQN) { clocked_on : "A" ; next_state : "A" ; } pin (A) { direction : input ; } pin (Q) { direction : output ; function : "IQ" ; } }
            cell (B) { pin (A) { direction : inout ; } pin (Y) { direction : output ; function : "A" ; } }
            }"#);
        let ok = |n: &str, du: bool| is_compatible(&l.cells[n], du);
        assert!(ok("G", false));
        assert!(!ok("G", true));
        for n in ["T", "M", "N", "F", "B"] {
            assert!(!ok(n, false), "{n}");
        }
    }
}
