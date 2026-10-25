// SPDX-License-Identifier: Apache-2.0
//! `vyges_rmp` — logic restructuring on an AIG: re-synthesis of timing-critical or area-heavy logic, with every rewrite checkable for equivalence.
//!
//! Stages are added here one at a time, each named after the step it performs and kept in the order
//! the command runs them.
pub mod cut;
pub mod design;
pub mod restructure;
pub mod timer;
