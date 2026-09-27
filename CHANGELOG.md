# Changelog

## [Unreleased]

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
