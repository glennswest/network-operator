//! network-operator — the rustkube/stormcos Cluster Network Operator.
//!
//! Reconciles a single `Network` CR into a Cilium install:
//!
//! ```text
//!   Network CR ──resolve──▶ EffectiveConfig ──render──▶ objects ──apply──▶ apiserver
//!        ▲                                                                     │
//!        └──────────────── status ◀── health ◀── owned-object watch ◀──────────┘
//! ```
//!
//! The layers below `apply` are pure functions, which is what makes the whole
//! install testable without a cluster. See README.md for the design.

pub mod apply;
pub mod controller;
pub mod crd;
pub mod health;
pub mod immutable;
pub mod modes;
pub mod render;
pub mod status;

#[cfg(test)]
pub(crate) mod testutil;
