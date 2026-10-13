//! Re-export of the session projection: it lives in `radian-host` (ADR 0023 U1/U5), because the
//! `SessionFacade` and the in-process `LocalSession` own it and `radian-tui` only reads it.
//! Kept as a module so `crate::projection::…` and the existing tests keep working.

pub use radian_host::projection::*;
