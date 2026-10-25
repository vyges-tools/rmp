// SPDX-License-Identifier: Apache-2.0
//! `vyges_rmp` — logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.
//!
//! Stages are added here one at a time, each named after the step it performs and kept in the order
//! the command runs them.
// The crate's own name inside it: the command-line entry ([`cli_main`]) is built into the
// library too, and its `vyges_rmp::…` paths resolve here.
extern crate self as vyges_rmp;

pub mod annealing;
pub mod cut;
pub mod design;
pub mod events;
pub mod genetic;
pub mod gia;
pub mod restructure;
pub mod resynth;
pub mod rng;
pub mod slack_tuning;
pub mod timer;

/// The `vyges-rmp` command-line entry (the binary's `main`), built into the library too so a
/// suite binary can link every engine and run the one it is invoked as.
#[cfg(feature = "suite")]
#[doc(hidden)]
#[path = "main.rs"]
pub mod cli_main;
