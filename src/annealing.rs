// SPDX-License-Identifier: Apache-2.0
//! Simulated annealing over operation sequences: `AnnealingStrategy::RunStrategy`.

use crate::restructure::fmt_float;
use crate::rng::Mt19937;
use crate::slack_tuning::{Evaluator, SolutionSlack, Strategy};

/// `resynth_annealing`'s settings.
#[derive(Clone, Debug)]
pub struct Annealing {
    pub rng: Mt19937,
    /// `-temp`; without it, the required time at the initial solution's worst vertex.
    pub temperature: Option<f32>,
    pub iterations: u32,
    pub revert_after: Option<u32>,
    pub initial_ops: u32,
}

impl Strategy for Annealing {
    /// A random initial sequence, evaluated; then per iteration a random neighbour of the current
    /// sequence, accepted when no worse, or worse with probability `exp(Δ / T)` at a temperature
    /// falling linearly to zero; after `revert_after` iterations below the best, back to the best.
    fn run_strategy(&mut self, all_ops: &[usize], t: &mut dyn Evaluator, out: &mut dyn FnMut(&str)) -> Result<Vec<usize>, String> {
        let mut ops: Vec<usize> = (0..self.initial_ops).map(|_| all_ops[self.rng.uniform_usize(all_ops.len())]).collect();
        let mut sol_slack = SolutionSlack::new(ops.clone());
        let (mut worst_slack, required) = sol_slack.evaluate(t)?;
        let temperature = *self.temperature.get_or_insert(required);
        out("[INFO RMP-0052] Resynthesis: starting simulated annealing");
        out(&format!("[INFO RMP-0053] Initial temperature: {}, worst slack: {}", fmt_float(temperature), fmt_float(worst_slack)));
        let mut best_worst_slack = worst_slack;
        let mut best_ops = ops.clone();
        let mut worse_iters = 0u32;
        for i in 0..self.iterations {
            let current_temp = temperature * ((self.iterations - i) as f32 / self.iterations as f32);
            if self.revert_after.is_some_and(|r| worse_iters >= r) {
                out("[INFO RMP-0057] Reverting to the best found solution");
                ops = best_ops.clone();
                worst_slack = best_worst_slack;
                worse_iters = 0;
            }
            if (i + 1) % 10 == 0 {
                out(&format!("[INFO RMP-0054] Iteration: {}, temperature: {}, best worst slack: {}", i + 1, fmt_float(current_temp), fmt_float(best_worst_slack)));
            }
            let new_ops = SolutionSlack::new(ops.clone()).random_neighbor(all_ops, &mut self.rng);
            sol_slack = SolutionSlack::new(new_ops.clone());
            let (worst_slack_new, _) = sol_slack.evaluate(t)?;
            if worst_slack_new < best_worst_slack {
                worse_iters += 1;
            } else {
                worse_iters = 0;
            }
            if worst_slack_new < worst_slack {
                let accept_prob = if current_temp == 0.0 { 0.0 } else { ((worst_slack_new - worst_slack) / current_temp).exp() };
                if self.rng.uniform_f32() >= accept_prob {
                    continue;
                }
            }
            ops = new_ops;
            worst_slack = worst_slack_new;
            if worst_slack > best_worst_slack {
                best_worst_slack = worst_slack;
                best_ops = ops.clone();
            }
        }
        Ok(best_ops)
    }
}
