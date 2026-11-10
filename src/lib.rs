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
//!
//! The VLAN layer ([`vlan`], #32) is a second, independent controller: `VLAN`
//! CRs -> Multus NetworkAttachmentDefinitions, namespace defaults, and the
//! node agent that puts each VLAN on the node bridge's uplinks.

pub mod apply;
pub mod controller;
pub mod crd;
pub mod health;
pub mod immutable;
pub mod metrics;
pub mod modes;
pub mod pins;
pub mod register;
pub mod render;
pub mod status;
pub mod vlan;

#[cfg(test)]
pub(crate) mod testutil;
