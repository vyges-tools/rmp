// SPDX-License-Identifier: Apache-2.0
//! Slack tuning: a search over sequences of AIG rewrites ([`crate::gia`]) for the one that leaves
//! the corner's worst slack best, then that sequence applied.
//!
//! [`optimize_design`] is `SlackTuningStrategy::OptimizeDesign`: the end points below the
//! threshold, the cell library, the strategy's search ([`crate::annealing`],
//! [`crate::genetic`]), the winning sequence run for real. A candidate is scored by
//! [`SolutionSlack::evaluate`]: its sequence run inside an ECO, the design re-timed, the ECO undone.
//! The database's name counters and the cut name generator are NOT rolled back.

use vyges_abc_lib::Abc;
use vyges_opendb::Db;
use vyges_sta::fuzzy;
use vyges_sta::search::INF_SLACK;

use crate::cut::abc_library::{self, AbcLibrary};
use crate::cut::logic_cut::UniqueName;
use crate::design::OdbBlif;
use crate::gia;
use crate::restructure::{fmt_float, Facts, RunError};
use crate::resynth::{get_endpoints, Libraries};
use crate::rng::Mt19937;

/// `kSearchResizeIters` / `kFinalResizeIters`: ABC's sizing iterations while searching and for
/// the final run.
pub const SEARCH_RESIZE_ITERS: i32 = 100;
pub const FINAL_RESIZE_ITERS: i32 = 1000;

/// What every evaluation needs: the design and the timer over it, the end points, the library,
/// the name generator. ⚠️ `abc` is declared last so it is dropped last.
pub struct Tuning<'a> {
    pub db: &'a mut Db,
    /// The design's timing (the cut is extracted over it; every ECO is undone back to it).
    pub facts: &'a Facts,
    pub candidates: Vec<usize>,
    pub lib: AbcLibrary,
    pub names: &'a mut UniqueName,
    /// Times a database: the timer's facts for it.
    pub time: &'a dyn Fn(&Db) -> Result<Facts, String>,
    /// The scene the strategy scores (`-corner`).
    pub scene: usize,
    pub abc: Abc,
}

/// One candidate: its operation sequence and, once evaluated, the worst slack it left.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SolutionSlack {
    pub solution: Vec<usize>,
    pub worst_slack: Option<f32>,
}

impl SolutionSlack {
    pub fn new(solution: Vec<usize>) -> SolutionSlack {
        SolutionSlack { solution, worst_slack: None }
    }

    /// `toString`: `[a, b, …], worst slack: s` (`not computed` before evaluation).
    pub fn describe(&self) -> String {
        if self.solution.is_empty() {
            return "[]".into();
        }
        let ids: Vec<String> = self.solution.iter().map(|i| i.to_string()).collect();
        let slack = self.worst_slack.map_or("not computed".to_string(), |s| format!("{s}"));
        format!("[{}], worst slack: {slack}", ids.join(", "))
    }

    /// `RandomNeighbor`: add an operation (always, for fewer than two), else one of add, remove,
    /// swap two adjacent — each position and operation drawn uniformly.
    pub fn random_neighbor(&self, all_ops: &[usize], rng: &mut Mt19937) -> Vec<usize> {
        let mut sol = self.solution.clone();
        let mv = if sol.len() > 1 { rng.uniform_i32(3) } else { 0 };
        match mv {
            0 => {
                let i = rng.uniform_usize(sol.len() + 1);
                let j = rng.uniform_usize(all_ops.len());
                sol.insert(i, all_ops[j]);
            }
            1 => {
                let i = rng.uniform_usize(sol.len());
                sol.remove(i);
            }
            _ => {
                let i = rng.uniform_usize(sol.len() - 1);
                sol.swap(i, i + 1);
            }
        }
        sol
    }

    /// `Evaluate`: the worst slack the sequence leaves (kept) and the required time at its vertex.
    pub fn evaluate(&mut self, t: &mut dyn Evaluator) -> Result<(f32, f32), String> {
        let (worst, required) = t.evaluate(&self.solution)?;
        self.worst_slack = Some(worst);
        Ok((worst, required))
    }
}

/// What scores a sequence: the worst slack it leaves and the required time at that slack's vertex.
pub trait Evaluator {
    fn evaluate(&mut self, ops: &[usize]) -> Result<(f32, f32), String>;
}

impl Evaluator for Tuning<'_> {
    /// The sequence run inside an ECO (with the search's sizing effort), the design re-timed — the
    /// scene's worst slack, and the required time at its vertex over every scene — and the ECO
    /// undone.
    fn evaluate(&mut self, ops: &[usize]) -> Result<(f32, f32), String> {
        let e = |x: vyges_opendb::Error| x.to_string();
        self.db.eco_begin().map_err(e)?;
        let ran = gia::run_gia(&mut self.abc, self.db, self.facts, &self.candidates, &mut self.lib, ops, SEARCH_RESIZE_ITERS, self.names);
        self.db.eco_end().map_err(e)?;
        let timed = ran.and_then(|()| (self.time)(self.db));
        self.db.eco_undo().map_err(e)?;
        let f = timed?;
        let (worst, vertex) = get_worst_slack(&f, self.scene);
        Ok((worst, vertex.map_or(INF_SLACK, |v| f.required_all[v])))
    }
}

/// `Sta::worstSlack(scene, max)` from a fresh search: the fuzzily least `wnsSlack` over the end
/// points in vertex order (one without a path not counted), and the first vertex that has it.
pub fn get_worst_slack(f: &Facts, scene: usize) -> (f32, Option<usize>) {
    let (mut worst, mut vertex) = (INF_SLACK, None);
    for v in (0..f.pin.len()).filter(|&v| f.endpoint[v]) {
        let s = f.scene_wns[scene][v];
        if !fuzzy::equal(s, INF_SLACK) && fuzzy::less(s, worst) {
            worst = s;
            vertex = Some(v);
        }
    }
    (worst, vertex)
}

/// A strategy's search: the operation sequence it settles on.
pub trait Strategy {
    fn run_strategy(&mut self, all_ops: &[usize], t: &mut dyn Evaluator, out: &mut dyn FnMut(&str)) -> Result<Vec<usize>, String>;
}

/// `SlackTuningStrategy::OptimizeDesign`.
#[allow(clippy::too_many_arguments)]
pub fn optimize_design(
    db: &mut Db,
    f: &Facts,
    libs: &Libraries,
    names: &mut UniqueName,
    time: &dyn Fn(&Db) -> Result<Facts, String>,
    scene: usize,
    slack_threshold: f32,
    strategy: &mut dyn Strategy,
    out: &mut dyn FnMut(&str),
) -> Result<(), RunError> {
    let err = RunError::Error;
    let candidates = get_endpoints(f, &OdbBlif { db }, slack_threshold);
    if candidates.is_empty() {
        out("[INFO RMP-0058] All endpoints have slack above threshold, nothing to do.");
        return Ok(());
    }
    out(&format!("[INFO RMP-0059] Resynthesis: starting tuning algorithm, Worst slack is {}", fmt_float(get_worst_slack(f, scene).0)));
    let abc = Abc::start().map_err(|e| err(e.to_string()))?;
    crate::resynth::flush();
    let lib = abc_library::build(&abc, libs.default, libs.default_path, &libs.corner, libs.dont_use, out).map_err(err)?;
    crate::resynth::flush();
    let mut t = Tuning { db, facts: f, candidates, lib, names, time, scene, abc };
    let all_ops = gia::gia_ops();
    let best_ops = strategy.run_strategy(&all_ops, &mut t, out).map_err(err)?;
    out("[INFO RMP-0067] Resynthesis: End of slack tuning, applying ABC operations");
    gia::run_gia(&mut t.abc, t.db, t.facts, &t.candidates, &mut t.lib, &best_ops, FINAL_RESIZE_ITERS, t.names).map_err(err)?;
    let after = (t.time)(t.db).map_err(err)?;
    out(&format!("[INFO RMP-0068] Resynthesis: Worst slack is {}", fmt_float(get_worst_slack(&after, scene).0)));
    Ok(())
}
