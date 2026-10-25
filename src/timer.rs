// SPDX-License-Identifier: Apache-2.0
//! The timer `restructure` asks, built over the database as the other engines build it: the
//! netlist from the database, the libraries, the constraints, the logic constants, one graph,
//! its delays and arrivals.

use std::collections::{BTreeMap, HashMap};

use vyges_opendb::Db;
use vyges_rsz::preamble::Libs;
use vyges_sta::liberty::Library;
use vyges_sta::fuzzy;
use vyges_sta::search::{Search, INF_SLACK};

use crate::restructure::{Facts, TimerEnv};

/// The design's libraries (Liberty text, in read order), its constraints, and the wire RC an
/// `estimate_parasitics -placement` ran with (`set_wire_rc`'s arguments; `None`: no estimate, so
/// no wire parasitics).
pub struct Timer {
    pub liberties: Vec<String>,
    /// `define_corners`, in order; empty is the one scene `default`.
    pub corners: Vec<String>,
    /// Per library (parallel to `liberties`): the corner `read_liberty -corner` named, `None` for
    /// every corner.
    pub liberty_corners: Vec<Option<String>>,
    pub sdc: String,
    pub wire_rc: Option<Vec<String>>,
    /// `set_routing_alpha` when the estimate ran.
    pub alpha: f32,
}

impl Timer {
    /// The libraries as the scenes read them: a `-corner` library serves its corner, any other
    /// every corner; the first library read is the link library, its corner the link scene.
    pub fn libs(&self) -> Result<Libs, String> {
        let mut libs = Libs { scenes: if self.corners.is_empty() { vec!["default".into()] } else { self.corners.clone() }, ..Libs::default() };
        if !self.corners.is_empty() {
            libs.scene_libs = vec![Vec::new(); self.corners.len()];
        }
        for (k, text) in self.liberties.iter().enumerate() {
            let group = vyges_sta::liberty_parse::parse(text)?;
            if group.simple.iter().any(|(n, _)| n.starts_with("k_")) || group.groups_of("scaling_factors").next().is_some() {
                return Err("liberty k_ scale factors are not modelled".into());
            }
            let lib = Library::read(&group)?;
            let corner = self.liberty_corners.get(k).cloned().flatten();
            let scene = match &corner {
                Some(c) => Some(self.corners.iter().position(|s| s == c).ok_or_else(|| format!("read_liberty -corner {c}: corner not defined"))?),
                None => None,
            };
            if self.corners.is_empty() {
                if scene.is_some() {
                    return Err("read_liberty -corner without define_corners".into());
                }
            } else {
                if libs.libs.is_empty() {
                    libs.link_scene = scene;
                }
                for (j, sl) in libs.scene_libs.iter_mut().enumerate() {
                    if scene.is_none_or(|c| c == j) {
                        sl.push(lib.clone());
                    }
                }
            }
            libs.libs.push(lib);
        }
        Ok(libs)
    }

    fn clocks(&self) -> Result<vyges_est::liberty::LibertyClocks, String> {
        let mut liberty = vyges_est::liberty::LibertyClocks::default();
        for text in &self.liberties {
            liberty.read(text)?;
        }
        Ok(liberty)
    }
}

impl TimerEnv for Timer {
    fn logic_constants(&self, db: &Db) -> Result<BTreeMap<(String, String), bool>, String> {
        let pins = vyges_est::sim::logic_constants(db, &self.clocks()?).map_err(|e| e.to_string())?;
        Ok(pins
            .into_iter()
            .filter_map(|(p, v)| match p {
                vyges_est::sim::Pin::ITerm(i, t) => Some(((i, t), v)),
                vyges_est::sim::Pin::Port(_) => None,
            })
            .collect())
    }

    fn facts(&self, db: &Db) -> Result<Facts, String> {
        let liberty = self.clocks()?;
        let libs = self.libs()?;
        let time_scale = libs.default_library().ok_or("restructure needs a liberty library")?.time_scale;
        let mut s = vyges_loom::sdc::Sdc::parse(&self.sdc).map_err(|e| format!("constraints: {e:?}"))?;
        let netlist = vyges_grt::timer::netlist(db);
        let mpins = vyges_rsz::env::master_pins(db)?;
        for (name, cell) in netlist.insts.iter().map(|i| &**i) {
            if !libs.libs.iter().any(|l| l.cells.contains_key(cell.as_str())) {
                return Err(format!("{name}: master {cell} has no Liberty cell"));
            }
        }
        // A constraint names a pin by its path name, the database by its escaped name.
        let mut by_path: HashMap<String, String> = netlist.ports.iter().map(|(p, _)| (vyges_sta::power::path_name(p), p.clone())).collect();
        for (name, cell) in netlist.insts.iter().map(|i| &**i) {
            for p in mpins.get(cell).cloned().unwrap_or_default() {
                let pin = format!("{name}/{p}");
                by_path.insert(vyges_sta::power::path_name(&pin), pin);
            }
        }
        for c in &mut s.clocks {
            for src in &mut c.sources {
                if let Some(name) = by_path.get(src.as_str()) {
                    *src = name.clone();
                }
            }
        }
        let clock_sources: Vec<String> = s.clocks.iter().filter(|c| !c.is_virtual()).flat_map(|c| c.sources.iter().cloned()).collect();
        let mut env = vyges_rsz::env::sdc_env(&s, &libs, &netlist)?;
        // `ensureConstantsPropagated` seeds `set_logic_*` and `set_case_analysis` too.
        let mut constraints = vyges_est::sim::Constraints::default();
        for (cmds, map) in [(&s.logic_values, &mut constraints.logic), (&s.case_values, &mut constraints.case)] {
            for c in cmds {
                for o in &c.objects {
                    let pin = match c.accessor.as_str() {
                        "get_ports" => vyges_est::sim::Pin::Port(by_path.get(o).cloned().unwrap_or_else(|| o.clone())),
                        "get_pins" => {
                            let name = by_path.get(o).cloned().unwrap_or_else(|| o.clone());
                            let (i, t) = name.rsplit_once('/').ok_or_else(|| format!("{}: `{o}` is not an instance pin", c.cmd))?;
                            vyges_est::sim::Pin::ITerm(i.to_string(), t.to_string())
                        }
                        a => return Err(format!("{} on `{a}` objects is not modelled", c.cmd)),
                    };
                    map.insert(pin, c.constant());
                }
            }
        }
        let values = vyges_est::sim::constant_pins_constrained(db, &liberty, &constraints).map_err(|e| format!("constant propagation: {e}"))?;
        env.constants = vyges_rsz::env::constants_by_name(values);
        let propagated = !s.propagated.is_empty();
        // The link scene's graph is the design's; every scene's slack is asked of its own.
        let link = libs.link_scene.unwrap_or(0);
        let scene_graph = |k: usize| -> Result<vyges_sta::graph::Graph, String> {
            let mut g = vyges_sta::graph::Graph::build_with_pins(libs.scene_slice(k), &netlist, Some(&mpins))?;
            g.sdc = env.clone();
            if !propagated {
                g.ideal_clock = vyges_rsz::timing::clock_pins(&g, &clock_sources).into_iter().collect();
            }
            Ok(g)
        };
        let mut g = scene_graph(link)?;
        let parasitics = match &self.wire_rc {
            Some(args) => {
                let u = liberty.units.ok_or("set_wire_rc before a liberty library: the timer's default units are not modelled")?;
                let mut rc = vyges_est::rc::Rc::new();
                rc.check_set_wire_rc(db, args).map_err(|e| e.to_string())?;
                rc.set_wire_rc(db, vyges_est::rc::Units { resistance: u.resistance, capacitance: u.capacitance, distance: u.distance }, args).map_err(|e| e.to_string())?;
                rc.sort_clk_and_signal_layers();
                let port_caps = g.sdc.port_pin_cap.clone();
                vyges_rsz::env::placement_parasitics_at(db, &rc, &liberty, &clock_sources, propagated, self.alpha, libs.scene_count(), &port_caps)?
            }
            None => vec![HashMap::new(); libs.scene_count()],
        };
        g.find_delays(&parasitics[link], None)?;
        let ssdc = vyges_rsz::env::search_sdc_on(&s, &netlist, time_scale, propagated, false)?;
        let mut search = Search::in_graph_order(&g, &ssdc);
        search.constraints_modelled()?;
        // `check_timing` searches arrivals: an unclocked register pin is one no clock reaches.
        search.find_arrivals()?;
        // Timing mode's `worstSlack` (requireds), and the BLIF writer's arrivals and requireds.
        search.find_requireds()?;
        // Per scene: `Sta::slack(vertex, max)` (the least over every scene's max paths, fuzzily,
        // the scenes in order), `wnsSlack`, and `Sta::required` over every scene.
        let n = g.vertices.len();
        let (mut slack, mut scene_wns, mut required_all) = (vec![INF_SLACK; n], Vec::new(), vec![INF_SLACK; n]);
        for (k, par) in parasitics.iter().enumerate() {
            let take = |sk: &Search, slack: &mut [f32], required_all: &mut [f32]| {
                for v in 0..n {
                    let s = sk.vertex_slack(v);
                    if fuzzy::less(s, slack[v]) {
                        slack[v] = s;
                    }
                    for p in &sk.paths[v] {
                        if p.tag.mm == vyges_sta::liberty::MAX && fuzzy::less(p.required, required_all[v]) {
                            required_all[v] = p.required;
                        }
                    }
                }
                (0..n).map(|v| sk.wns_slack(v, true)).collect::<Vec<f32>>()
            };
            if k == link {
                scene_wns.push(take(&search, &mut slack, &mut required_all));
            } else {
                let mut gk = scene_graph(k)?;
                gk.find_delays(par, None)?;
                let mut sk = Search::in_graph_order(&gk, &ssdc);
                sk.find_arrivals()?;
                sk.find_requireds()?;
                scene_wns.push(take(&sk, &mut slack, &mut required_all));
            }
        }
        // `getArrivalTime` / `getRequiredTime`: an infinite value (no path) is 0.
        let finite = |x: f32| if x.abs() >= vyges_sta::search::INF_SLACK { 0.0 } else { x };
        let arrival = |v: usize| -> [f32; 2] {
            match search.worst_arrival_path(v, vyges_sta::liberty::MAX) {
                None => [0.0, 0.0],
                Some(p) => [vyges_sta::liberty::RISE, vyges_sta::liberty::FALL].map(|rf| finite(search.arrival_rf(v, rf, p.tag.mm))),
            }
        };
        let required = |v: usize| -> [f32; 2] { [vyges_sta::liberty::RISE, vyges_sta::liberty::FALL].map(|rf| finite(search.required_rf(v, rf, vyges_sta::liberty::MAX))) };
        let mut f = Facts {
            pin: g.vertices.iter().map(|v| v.name.clone()).collect(),
            inst: (0..n).map(|v| if g.is_port(v) { None } else { g.vertices[v].name.rsplit_once('/').map(|(i, _)| i.to_string()) }).collect(),
            endpoint: (0..n).map(|v| search.is_endpoint(v)).collect(),
            port: (0..n).map(|v| g.is_port(v)).collect(),
            reg_output: (0..n).map(|v| search.is_reg_output(v)).collect(),
            register: (0..n)
                .map(|v| {
                    let vx = &g.vertices[v];
                    match (vx.lib, &vx.cell) {
                        (Some(l), Some(c)) => g.libs[l].cells.get(c).is_some_and(|c| c.is_sequential()),
                        _ => false,
                    }
                })
                .collect(),
            from_ok: (0..n).map(|v| search.pred0_from(v)).collect(),
            to_ok: (0..n).map(|v| search.pred0_to(v)).collect(),
            edges: (0..g.edges.len()).map(|e| (g.edges[e].from, g.edges[e].to, search.comb_logic_thru(e))).collect(),
            in_edges: g.in_edges.clone(),
            out_edges: g.out_edges.clone(),
            unconstrained: Vec::new(),
            unclocked: (0..n).filter(|&v| search.is_reg_clk(v) && !search.has_clock_arrival(v)).collect(),
            depth: (0..n).map(|v| search.worst_slack_path(v, vyges_sta::liberty::MAX).map(|p| search.expanded_size(p) / 2)).collect(),
            arrival: (0..n).map(arrival).collect(),
            required: (0..n).map(required).collect(),
            slack,
            scene_wns,
            required_all,
        };
        // `checkUnconstrainedEndpoints` lists only end points, which are already all ends here.
        f.unconstrained.clear();
        Ok(f)
    }
}
