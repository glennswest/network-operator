# Changelog

## [Unreleased]

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
