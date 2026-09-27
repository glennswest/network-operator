# Changelog

## [Unreleased]

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
