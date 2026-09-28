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
- [ ] #16 test containers — CODE DONE (`test/`, see test/README.md), awaiting
      the first on-cluster run. Verified so far: 25 suite unit tests +
      clippy under sc-build; image built and smoke-run with podman on dev;
      stormcentral's runner built it (test/build.sh + podman build) and pushed
      it to C2NR0Q2's registry (run a845a17d5c, 2a1232c). stormcentral#56
      (`@@RESULT` parse) has a fix in stormcentral (20a570b, 15:07 2026-09-27),
      not yet exercised: the next run (dcae784bb9, 9998abd) stopped earlier,
      at the image lookup, because C2NR0Q2's sbregistry (:5100) refused
      connections (apiserver /readyz 200; still refused 10 min
      later, every component's runs hit it). stormcos#135 /
      stormcentral#71 cleared 2026-09-28 (C2NR0Q2 on 11.51, :5100 answers);
      stormcentral's own run fe3fc66b32 then failed the image push (broken
      pipe mid-blob), fixed in stormblock-registry v0.24.1 (#56 there). Next: `stormcentral test run
      network-operator short` on C2NR0Q2, fix what it finds, then medium.
      Expect exit 2 on the Network/nodes checks until stormcentral#55
      (cluster-scoped read for test runs).
- Open: #9 (render parity with the 18 objects stormcos ships; the
  stormcos-cilium pin is v1.20.2, the stormcos edition preloads v1.20.1 —
  stormcos#133) P1; #12 envoy not reaped, #13 CRD never updated, #14 health
  rollup — P2; #8 (drift-heal test + must-gather; collector filed as
  stormcos_qa#22), #15, #7 — P3. #9 also carries the envoy tag mismatch
  (1.36.9 here vs 1.37.5 in the stormcos edition; stormcos-cilium runs Envoy
  in the agent, so pins none — the unused edition preload is stormcos#153).
- Where the last session stopped (2026-09-27): issue validation pass done
  (all open issues still real, priorities confirmed); comment mining filed
  stormcos_qa#22 and added the envoy pin to #9; docs re-verified against the
  code (second pass: test/README env table). Second validation pass
  2026-09-28: unchanged; #16 status posted. Next: run `stormcentral test run
  network-operator short` (registry is back), then medium, for #16.
- Owner decisions pending: #18 (a golden, or stay a preloaded container) P2;
  #19 (where drift-heal is tested — test Jobs may not touch kube-system) P3.

## Status

Renders, applies, drift-heals and reports on Cilium for all five modes plus
the optional standalone Envoy. Not shipped as a golden (#18); the image is an
OCI archive on each release, preloaded by stormcos. Docs last refreshed from
the code 2026-09-27; operator code unchanged since v0.2.4 (only `test/` and
docs since). `test/` suites built and pushed by stormcentral, never yet run
on a cluster (stormcos#135, then stormcentral#56).
