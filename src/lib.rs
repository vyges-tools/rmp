// SPDX-License-Identifier: Apache-2.0
//! `vyges_rmp` — logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.
//!
//! Stages are added here one at a time, each named after the step it performs and kept in the order
//! the command runs them.
pub mod annealing;
pub mod cut;
pub mod design;
pub mod genetic;
pub mod gia;
pub mod restructure;
pub mod resynth;
pub mod rng;
pub mod slack_tuning;
pub mod timer;
