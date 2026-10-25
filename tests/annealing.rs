// SPDX-License-Identifier: Apache-2.0
//! The annealing search against recorded runs (`data/annealing/<case>.*`): every evaluation's
//! operation sequence, in order, and its worst slack, as the reference printed them at its debug
//! level, with its own log lines.
//!
//! The scorer here replays the recorded slacks (a sequence's slack is a function of the sequence:
//! each evaluation starts from the same design), so the test pins the search alone — the draws,
//! neighbour moves, acceptance, reverts and the temperature schedule — independent of timing.
use std::collections::HashMap;

use vyges_rmp::annealing::Annealing;
use vyges_rmp::gia::gia_ops;
use vyges_rmp::rng::Mt19937;
use vyges_rmp::slack_tuning::{Evaluator, Strategy};

struct Recorded {
    slack: HashMap<Vec<usize>, f32>,
    required: f32,
    seen: Vec<Vec<usize>>,
}

impl Evaluator for Recorded {
    fn evaluate(&mut self, ops: &[usize]) -> Result<(f32, f32), String> {
        self.seen.push(ops.to_vec());
        let s = *self.slack.get(ops).ok_or_else(|| format!("evaluation {} asks for {ops:?}, never recorded", self.seen.len() - 1))?;
        Ok((s, self.required))
    }
}

fn ops(s: &str) -> Vec<usize> {
    s.split(',').map(|x| x.parse().unwrap()).collect()
}

fn run(case: &str, evals: &str, info: &str, args: &str) {
    let mut order = Vec::new();
    let mut slack = HashMap::new();
    let mut final_ops = Vec::new();
    for line in evals.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w[0] {
            "eval" => {
                let o = ops(w[3]);
                slack.insert(o.clone(), w[5].parse::<f32>().unwrap());
                order.push(o);
            }
            "final" => final_ops = ops(w[2]),
            _ => panic!("{line}"),
        }
    }
    let words: Vec<&str> = args.split_whitespace().collect();
    let key = |k: &str| words.iter().position(|w| *w == k).map(|i| words[i + 1]);
    let temperature: f32 = info.lines().find_map(|l| l.split("Initial temperature: ").nth(1)).unwrap().split(',').next().unwrap().parse().unwrap();
    let mut a = Annealing {
        rng: Mt19937::new(key("-seed").map_or(0, |s| s.parse().unwrap())),
        temperature: key("-temp").map(|t| t.parse().unwrap()),
        iterations: key("-iters").map_or(100, |s| s.parse().unwrap()),
        revert_after: key("-revert_after").map(|s| s.parse().unwrap()),
        initial_ops: key("-initial_ops").map_or(10, |s| s.parse().unwrap()),
    };
    // Without `-temp` the initial temperature is the required time the first evaluation returns.
    let mut rec = Recorded { slack, required: temperature, seen: Vec::new() };
    let mut lines = Vec::new();
    let best = a.run_strategy(&gia_ops(), &mut rec, &mut |l| lines.push(l.to_string())).unwrap_or_else(|e| panic!("{case}: {e}"));
    for (k, (ours, theirs)) in rec.seen.iter().zip(&order).enumerate() {
        assert_eq!(ours, theirs, "{case}: evaluation {k}");
    }
    assert_eq!(rec.seen.len(), order.len(), "{case}: evaluations");
    assert_eq!(best, final_ops, "{case}: the sequence applied");
    let expected: Vec<&str> = info.lines().filter(|l| ["RMP-0052", "RMP-0053", "RMP-0054", "RMP-0057"].iter().any(|id| l.contains(id))).collect();
    assert_eq!(lines, expected, "{case}: log lines");
}

macro_rules! case {
    ($name:ident, $file:literal) => {
        #[test]
        fn $name() {
            run(
                $file,
                include_str!(concat!("data/annealing/", $file, ".evals.txt")),
                include_str!(concat!("data/annealing/", $file, ".info.txt")),
                include_str!(concat!("data/annealing/", $file, ".args.txt")),
            );
        }
    };
}

// `-revert_after 5`, the temperature from the first evaluation's required time.
case!(reverting_to_the_best_after_five_worse_iterations, "gcd_annealing1");
// `-temp 1e-10`.
case!(a_given_temperature_falls_linearly_over_the_iterations, "gcd_annealing2");
// `-initial_ops 5 -iters 30` on a larger design.
case!(five_initial_operations_over_thirty_iterations, "aes_annealing");
