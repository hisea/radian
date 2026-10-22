//! `radian-arc`: the gate library for arc (ADR 0048).
//!
//! The gate is a fixed sequence of stages against one commit. This crate owns the
//! configuration shape, the separate checkout for each stage, the `commands` stage, the
//! verdict record and its digest, and the integration candidate. It has no CLI and does not
//! schedule anything; a later slice connects it to the state machine.

pub mod gate;
