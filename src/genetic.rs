// SPDX-License-Identifier: Apache-2.0
//! A genetic search over operation sequences: `GeneticStrategy::RunStrategy`.

use crate::rng::Mt19937;
use crate::slack_tuning::{Evaluator, SolutionSlack, Strategy};

/// `resynth_genetic`'s settings.
#[derive(Clone, Debug)]
pub struct Genetic {
    pub rng: Mt19937,
    pub population_size: u32,
    pub mutation_probability: f32,
    pub crossover_probability: f32,
    pub tournament_size: u32,
    pub tournament_probability: f32,
    pub iterations: u32,
    pub initial_ops: u32,
}

/// `removeDuplicates`: the first of each sequence kept, in order.
fn remove_duplicates(population: &mut Vec<SolutionSlack>) {
    let mut taken = std::collections::HashSet::new();
    population.retain(|s| taken.insert(s.solution.clone()));
}

/// `std::ranges::greater` / `less` over `std::optional<float>`: no value below every value.
fn opt_greater(a: Option<f32>, b: Option<f32>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x > y,
        (Some(_), None) => true,
        _ => false,
    }
}

impl Strategy for Genetic {
    /// A random population, evaluated; per iteration: crossovers (the first half of one parent and
    /// the second half of another), mutations (random neighbours), duplicates dropped, the new
    /// ones evaluated, the population sorted best first, and the next population picked by
    /// tournaments. The best of the last population wins.
    fn run_strategy(&mut self, all_ops: &[usize], t: &mut dyn Evaluator, out: &mut dyn FnMut(&str)) -> Result<Vec<usize>, String> {
        if self.population_size == 0 {
            return Err("[ERROR RMP-0064] Population size should be greater than 0".into());
        }
        let rng = &mut self.rng;
        let mut population: Vec<SolutionSlack> = (0..self.population_size)
            .map(|_| SolutionSlack::new((0..self.initial_ops).map(|_| all_ops[rng.uniform_i32(all_ops.len() as i32) as usize]).collect()))
            .collect();
        for candidate in &mut population {
            candidate.evaluate(t)?;
        }
        for i in 0..self.iterations {
            out(&format!("[INFO RMP-0065] Resynthesis: Iteration {i} of genetic algorithm"));
            let generation_size = population.len() as u32;
            let cross_size = ((self.crossover_probability * generation_size as f32) as u32).max(1);
            for _ in 0..cross_size {
                let rand1 = rng.uniform_i32(generation_size as i32) as usize;
                let rand2 = rng.uniform_i32(generation_size as i32) as usize;
                if rand1 == rand2 {
                    continue;
                }
                let (p1, p2) = (&population[rand1].solution, &population[rand2].solution);
                let mut child: Vec<usize> = p1[..p1.len() / 2].to_vec();
                child.extend_from_slice(&p2[p2.len() / 2..]);
                population.push(SolutionSlack::new(child));
            }
            let mut_size = ((self.mutation_probability * generation_size as f32) as u32).max(1);
            for _ in 0..mut_size {
                let r = rng.uniform_i32(generation_size as i32) as usize;
                let child = population[r].random_neighbor(all_ops, rng);
                population.push(SolutionSlack::new(child));
            }
            remove_duplicates(&mut population);
            for sol_slack in &mut population {
                if sol_slack.worst_slack.is_none() {
                    sol_slack.evaluate(t)?;
                }
            }
            // `stable_sort(greater, &WorstSlack)`.
            population.sort_by(|a, b| {
                if opt_greater(a.worst_slack, b.worst_slack) {
                    std::cmp::Ordering::Less
                } else if opt_greater(b.worst_slack, a.worst_slack) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            });
            let mut new_population = Vec::with_capacity(self.population_size as usize);
            for _ in 0..self.population_size {
                let mut tournament: Vec<usize> = (0..self.tournament_size).map(|_| rng.uniform_i32(population.len() as i32) as usize).collect();
                tournament.sort();
                tournament.dedup();
                // `find_if`: draws stop at the first winner.
                let p = f64::from(self.tournament_probability);
                if let Some(&w) = tournament.iter().find(|_| rng.bernoulli(p)) {
                    new_population.push(population[w].clone());
                }
            }
            remove_duplicates(&mut new_population);
            if new_population.is_empty() {
                new_population.push(population[0].clone());
            }
            population = new_population;
        }
        // `max_element`: the first of the greatest.
        let mut best = 0;
        for (k, c) in population.iter().enumerate() {
            if opt_greater(c.worst_slack, population[best].worst_slack) {
                best = k;
            }
        }
        out(&format!("[INFO RMP-0066] Resynthesis: Best result is of individual {best}"));
        Ok(population[best].solution.clone())
    }
}
