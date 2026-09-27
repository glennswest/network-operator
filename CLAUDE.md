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
- On-cluster test container (`test/`, its own workspace + Cargo.lock):
  `sc-build 'cd test && cargo test --locked && cargo clippy --locked --all-targets -- -D warnings'`;
  image = `test/build.sh` then `podman build -f test/Containerfile .`;
  run = `stormcentral test run network-operator short|medium|long --url http://stormcentral.g8.lo`.
  What each suite checks: `test/README.md`.

## Layout

Docs: `README.md` (the reference), `docs/presentation.md` (Marp deck), `CHANGELOG.md`.

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

## Version

`0.2.4` (tag `v0.2.4`). Version locations, all must match: `Cargo.toml`,
`deploy/operator.yaml` (image tag), the OCI-archive example in `README.md`.
`Cargo.lock` is committed (the image builds `--locked`).

## Work plan

- [x] #10 docs from the code — README rewritten from the source, CHANGELOG.md
      created, module docs checked. Gaps filed: #12 (envoy not reaped), #13
      (CRD never updated), #14 (health rollup), #15 (no health/metrics
      endpoint, no webhook). Cross-component pin mismatches reported on
      stormcos#65.
- [x] #11 presentation — `docs/presentation.md` (Marp, 11 slides), drawn
      from README; "where it sits" from `stormcentral check` (missing
      stormcos edge filed as stormcentral#52). Keep it in step with README.
- [ ] #16 test containers (stormcentral docs/test-standard.md) — IN PROGRESS.
      `test/`: own workspace crate `network-operator-test`, static musl,
      `test/build.sh` stages `test/.stage/test`, `test/Containerfile` FROM
      scratch; `/test short|medium|long`, `/test workload serve` is the pod
      workload (same image). Plan:
      short: Network `cluster` Available/not Degraded/observed current; a pod
      gets an IP inside appliedClusterNetwork; pod→pod TCP; Service ClusterIP
      (kube-proxy replacement) reachable. medium: + NetworkPolicy deny/allow,
      Service follows scale 1→3 and pod loss, pod replaced gets new IP,
      cross-node (≥2 nodes else skip), LoadBalancer IP when LB-IPAM on (else
      skip), CiliumEndpoints gone after delete. long: waves of pods+Services
      sized from node allocatable pods, per-wave ramp/reach latency and
      residue (pods, CiliumEndpoints), trend vs wave 1.
      Blocker: runner grants no cluster-scoped read (stormcentral#55) —
      Network/nodes reads report `could not run` (exit 2) until it does.
- Open: #9 (render parity with the 18 objects stormcos ships, Cilium 1.20.1
  pin), #8 (QA tests + must-gather), #7 (release profile), #12–#15.

## Status

Renders, applies, drift-heals and reports on Cilium for all five modes plus
the optional standalone Envoy. Not shipped as a golden; the image is an OCI
archive on each release, preloaded by stormcos.
