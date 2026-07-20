# network-operator

Rust operator that owns the Cilium CNI lifecycle from a single `Network` CR.
The design and the mode table live in README.md — read it before changing
`src/modes.rs`, because that file *is* the mode table.

## Commands

- Build: `cargo build`
- Test: `cargo test` (pure unit + golden tests; no cluster needed)
- Lint: `cargo clippy --all-targets -- -D warnings`
- Regenerate the CRD after editing `src/crd.rs`: `make crds`
  (writes `deploy/crds/network.storm.io_networks.yaml` — never hand-edit it)
- Accept an intended render change: `make golden`, then review the diff
- Render without a cluster: `make dry-run FILE=examples/network-bgp.yaml`

## Layout

The pipeline is `crd -> modes -> render -> apply`, and everything before
`apply` is a pure function. That is what lets the whole install be tested off
-cluster, so keep it that way — no client calls in `modes.rs` or `render/`.

- `src/crd.rs` — the `Network` wire types. Serde's camelCase mangles acronyms,
  so `clusterPoolIPv4MaskSize` and `localASN` carry explicit `rename`s; a
  golden test guards them.
- `src/modes.rs` — mode defaults + validation -> `EffectiveConfig`. All policy
  lives here. Unimplemented features (ipsec, standalone envoy) are *rejected*
  here rather than half-rendered.
- `src/render/` — `EffectiveConfig` -> objects, in apply order. `config.rs` is
  the load-bearing one: it is where a mode becomes Cilium behaviour.
- `src/apply.rs` — server-side apply, with the rustkube fallbacks.
- `src/health.rs` — `observe` reads, `conditions` decides. Keep `conditions`
  pure.
- `src/immutable.rs` — the CNO-style immutability check, against
  `status.applied*`.
- `src/controller.rs` — the reconcile loop and the object watches that make
  drift self-heal.

## Invariants (do not weaken)

- The DaemonSet/Deployment **selectors must not depend on the spec**. They are
  immutable server-side; deriving them from config makes any change unappliable.
- `k8sServiceHost` is required. With kube-proxy replacement there is no Service
  route to the apiserver until Cilium is up, so the agent must be told where it
  is or the cluster cannot bootstrap.
- The Cilium CRs (`cilium.io/*`) are applied **last** and a missing CRD is
  deferred, not failed — `cilium-operator` installs those CRDs itself, so on a
  fresh install they legitimately do not exist yet.
- Immutability is checked against `status.applied*`, and a failed reconcile must
  **not** update `applied*` — moving the baseline on failure would let a rejected
  change slip in on the next pass.
- Turning a feature off must delete its objects (`render::reapable`), not orphan
  them. A stale `CiliumBGPClusterConfig` keeps advertising.

## rustkube compat (the control plane is rustkube, not upstream)

rustkube implements `application/apply-patch+yaml` as a plain merge with **no
field-ownership tracking**. Our fields are still restored on drift; fields added
by someone else survive rather than being pruned. Drift-heal, not drift-purge.
Older builds reject PATCH outright (rustkube#23), so `apply.rs` falls back to
create/replace and `status.rs` walks patch `/status` -> PUT `/status` -> whole-
object PUT.
