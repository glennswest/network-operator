# Changelog

## [Unreleased]
<!-- New unreleased changes go here -->

### 2026-10-06
- **fix:** turning `spec.cilium.envoy.enabled` off now deletes the
  `cilium-envoy` ServiceAccount, ConfigMap, DaemonSet and Service (#12);
  they joined `render::reapable`. README reconcile-loop step 4 lists
  everything that is reaped.
- **feat:** the operator updates the `Network` CRD on start (#13) instead of
  create-if-absent: replaced (GET + PUT, exact schema) when its spec or the
  new `network.storm.io/operator-version` stamp differs; a CRD stamped by a
  newer operator is left alone so a rollback cannot strip fields.
  `src/register.rs`; `crdgen` and `deploy/crds/` carry the stamp, and a unit
  test fails if `deploy/crds/` is stale.
- **feat:** health rollup (#14): `Available` now also requires the
  `cilium-envoy` DaemonSet ready when Envoy is enabled (`EnvoyNotReady`),
  every `cilium.io` CRD `Established` (`CiliumCRDsNotEstablished`, also
  `Progressing=WaitingForCiliumCRDs`), and a `CiliumNode` for every node with
  a ready agent (`CiliumNodesMissing`). Envoy pods join the crash-loop check
  and the rollout in `Progressing`. README health table and deck updated.
- **docs:** CLAUDE.md #9 done; README notes v0.3.0 has no release assets
  (#18); deck test count 114.

## [v0.3.0] — 2026-10-06

### Breaking
- Cilium images are pinned by digest (`src/pins.rs`, from stormcos-cilium
  `pinned.txt`, linux/amd64). A `spec.cilium.version` with no pin is
  rejected unless `spec.cilium.images` names each needed image by digest; a
  tag there is rejected. Default Cilium moves from 1.19.6 (unpinned) to
  1.20.2. (#9)

### Added
- `spec.cilium.hubble.enabled` (default on): Hubble relay, `hubble-peer`,
  `hubble-metrics`, reaped when off. (#9)
- `cilium-secrets` namespace + TLS-interception RBAC, ztunnel RBAC,
  `Service/cilium-agent`, metrics ports and keys; the agent shaped like the
  1.20.2 chart. (#9)
- `tests/parity.rs`: the render against a verbatim copy of stormcos's Cilium
  manifests. (#9)
- `test/`: on-cluster short/medium/long suites (#16).

### Fixed
- The agent no longer claims host port 9964 beside a standalone
  `cilium-envoy`. (#9)

### Documentation
- README/deck rewritten from the code (#10, #11); stormcos does not ship
  this operator (#21); parity, pins and remaining gaps documented (#9).

### Detail, by date

### 2026-10-06 (#9 render parity with stormcos)
- **BREAKING:** images are pinned by digest. `spec.cilium.version` resolves
  through `src/pins.rs` (linux/amd64 digests copied from stormcos-cilium
  `pinned.txt`) and a version with no pin is rejected unless
  `spec.cilium.images.{agent,operator,hubbleRelay}` name each needed image
  by digest. A tag in `images` is rejected. The default Cilium is now
  **1.20.2** (was 1.19.6, which has no pin). `cilium-envoy` stays a tag
  pending #20.
- **feat:** `spec.cilium.hubble.enabled` (default `true`): Hubble server keys
  in `cilium-config`, `Service/hubble-peer`, `Service/hubble-metrics`,
  `ConfigMap/hubble-relay-config` and `Deployment/hubble-relay` (stormcos's
  host-networked, unix-socket shape). All reaped when turned off.
- **feat:** `Namespace/cilium-secrets` with the agent's and operator's
  TLS-interception Roles/RoleBindings and the `policy-secrets-*` keys; the
  1.20 `cilium-operator-ztunnel` Role/RoleBinding; headless
  `Service/cilium-agent` for the embedded proxy's metrics; agent/operator
  metrics keys and named host ports.
- **feat:** agent shaped like the 1.20.2 chart: named ports, the
  `cilium-netns` mount, `WRITE_CNI_CONF_WHEN_READY` on `clean-cilium-state`.
- **fix:** the agent does not declare the proxy-metrics host port (9964)
  while `cilium-envoy` runs standalone and binds it; `Service/cilium-agent`
  is rendered only for the embedded proxy and reaped otherwise.
- **test:** `tests/parity.rs` checks the default render against a verbatim
  copy of stormcos's Cilium manifests (`tests/fixtures/stormcos/`, stormcos
  3dcf6d6): the same 23 objects (the issue said 18; stormcos has grown),
  the same image digests, and `cilium-config` agreeing on every shared key
  bar three with stated reasons.
- **docs:** README (rendered objects, fields, ports, parity, known gaps),
  deck and examples updated. `k8sServiceHost` at reconcile time split out
  as owner decision #23.

### 2026-10-06
- **docs:** README, deck and CLAUDE.md no longer say stormcos ships or
  preloads network-operator (#21). Verified against stormcos: no
  network-operator in `deploy/image.toml`, `stormcos-compose` deleted,
  `editions/kubernetes.toml` is the pre-pivot plan, and the image runs Cilium
  from static manifests (stormcos-cilium's render, v1.20.2). README now warns
  that those manifests own the same `kube-system` objects the operator
  renders, so the operator must not be deployed on a stormcos node. The
  edition-vs-repo pin table is reduced to this repo vs stormcos-cilium.

### 2026-09-29
- **docs:** CLAUDE.md — comment mining filed owner decision #20 (standalone cilium-envoy support and tag pin); #18/#19/#20 labelled `needs-owner`.

### 2026-09-28
- **docs:** CLAUDE.md — the image-push fault is fixed in stormblock-registry v0.24.1; the cilium-envoy preload mismatch is filed as stormcos#153.
- **docs:** CLAUDE.md #16 blocker updated — C2NR0Q2's registry answers again; the next known fault is the image push (stormcentral#56).

### 2026-09-27 (docs refresh, second pass)
- **docs:** re-verified README, `test/README.md`, the deck and CLAUDE.md
  against the code (operator unchanged since v0.2.4; `test/` unchanged since
  2a1232c). Nothing the docs promise is missing beyond #12–#15 and #9.
- **docs:** `test/README.md` documents the container's environment
  (`STORM_API`, `STORM_NAMESPACE`, `STORM_RUN_ID`, `STORM_SUITE`,
  `STORM_TIMEOUT` and their defaults, from `test/src/env.rs`) and long's 8 h
  default budget.
- **docs:** README, deck and CLAUDE.md link the must-gather network collector
  (stormcos_qa#22, from #8) and note that stormcos-cilium pins no
  `cilium-envoy` image (#9).

### 2026-09-27 (docs refresh)
- **docs:** re-verified README, `test/README.md`, the deck and CLAUDE.md
  against the code (operator unchanged since v0.2.4). README lists the
  `make build` and `make deploy` targets; the test-run status now names the
  current blocker (C2NR0Q2's node registry, stormcos#135 / stormcentral#71)
  and says the stormcentral#56 fix is not yet exercised.
- **chore:** `.gitignore` covers `tmp/` (session scratch files)
- **docs:** README "How it ships" now tabulates the three disagreeing pin
  sources (this repo 1.19.6 / `localhost/…:0.2.4`, the stormcos edition
  1.20.1 / `ghcr…:0.2.3`, stormcos-cilium 1.20.2 by digest) with their issues
  (#9, stormcos#79, stormcos#133), and links the golden decision (#18).
- **docs:** README, `test/README.md` and CLAUDE.md say the `test/` suites have
  not yet run on a cluster (stormcentral#56) and that drift-heal is untested
  on a live cluster (#19); stormcentral added to the relationships.
- **docs:** deck status/planned slides updated to the same pins and
  decisions; CLAUDE.md open issues carry their priorities.

### 2026-09-27 (#16)
- **test:** `test/` — the on-cluster test container per stormcentral's test
  standard: one static scratch image, `/test short|medium|long`, JSON-lines
  results, exit 0/1/2, everything in the run's namespace under
  `storm.io/test-run`. short: Network status, pod IP in the pod CIDR,
  pod→pod, ClusterIP Service. medium: + `status.applied*` baseline (checked
  with the operator's own `resolve_network`/`applied_from`), Service
  scale-out and backend loss, NetworkPolicy deny/allow, cross-node,
  LoadBalancer IPAM, CiliumEndpoint cleanup. long: overnight waves sized from
  node capacity, per-wave ready/reach/drain numbers and residue, trend
  failing on slowdown or leftovers. The image is also its own workload
  (`/test workload serve`).
- **docs:** README, CLAUDE.md, deck and `test/README.md` describe the suites;
  cluster-scoped read for test runs filed as stormcentral#55.

### 2026-09-27
- **docs:** `docs/presentation.md`, a 11-slide Marp deck of purpose and
  functionality (#11): problem, place in stormcos (per `stormcentral check`),
  architecture, modes, features, interfaces, shipping, planned work, status.
  Linked from the README. Missing stormcos → network-operator edge in
  stormcentral's graph filed as stormcentral#52.

### 2026-09-24
- **docs:** README rewritten from the code (#10): what it renders and in what
  order, every `Network` field with its default, the mode table, the
  reconcile loop and health rules, CLI flags/env, rendered ports, build via
  `sc-build`, how it ships (OCI archive, rpm/deb, no golden), and a Known gaps
  section replacing claims the code does not meet (validating webhook,
  CiliumNode/CRD health, per-version templates, Geneve).
- **docs:** CHANGELOG.md created; CLAUDE.md carries version, work plan and status.
- **docs:** `render::reapable` doc notes the `cilium-envoy` objects are not reaped; gaps filed as #12–#15.

## [v0.2.4] — 2026-09

Releases up to v0.2.4 predate this changelog; see `git log`.
