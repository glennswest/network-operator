---
marp: true
theme: default
paginate: true
title: network-operator
description: Purpose and functionality of network-operator, the Cluster Network Operator for rustkube / stormcos
---

<!-- Render: npx @marp-team/marp-cli docs/presentation.md   (add --pdf for PDF)
     Every claim here is checkable against the code at v0.3.0; the source file
     is named on each slide. Keep it in step with README.md. -->

# network-operator

**The Cluster Network Operator for rustkube / stormcos**

Cilium from one `Network` custom resource — installed, kept, healed, reported.

v0.3.0 · Rust · Apache-2.0 · github.com/glennswest/network-operator

---

## What it is, and the problem it solves

A cluster needs a CNI before any pod can talk, and the CNI is a dozen-plus
coupled objects (RBAC, config, agent DaemonSet, operator, LB/BGP CRs) that
must agree with each other and with the cluster's CIDRs.

**network-operator** turns that into **one cluster-scoped `Network` CR**:

- pick a **mode** (`overlay`, `native`, `bgp`, `encrypted`, `bare-metal`),
- it renders every Cilium object, applies them, **reapplies on drift**,
  **deletes** what a config change no longer wants,
- and reports `Available` / `Progressing` / `Degraded` on the CR.

The stack's analog of OpenShift's **CNO**. *Not* `cilium-operator` — it
*installs* `cilium-operator`.

---

## Where it sits in stormcos

```
                 stormcos  (does NOT ship it: Cilium runs from static
                            manifests rendered by stormcos-cilium)

            ┌──────────────────┐
            │ network-operator │   group: network
            └──────────────────┘
               │            │
               ▼            ▼
          rustkube     stormcos-cilium
       (apiserver it   (the Cilium image/chart
        talks to)       pins its defaults must match)
```

- Depends on (`stormcentral check`): **rustkube**, **stormcos-cilium**.
- Runs on nodes via **rustkube-node** (kubelet; agent `startupProbe`).
- **Not shipped by stormcos**: the image has no network-operator member and
  nothing preloads it; the old `editions/kubernetes.toml` entry is the
  pre-pivot plan. stormcos ships the same Cilium objects (`DaemonSet/cilium`,
  `cilium-operator`, `cilium-config`) as static manifests — so deploying the
  operator on a stormcos node would fight them. Do not.
- Not **stormlb**: that owns inbound (shipped as its L7 router only; its
  apiserver-VIP half is not yet run on any node); this manages pod networking.

---

## How it works

```
 Network CR ──► modes.rs ──► render/ ──► apply.rs ──► cluster
 (crd.rs)       defaults +    objects,    server-side   │
                validation    in order    apply         │
                → Effective   (pure)      + fallbacks   │
                  Config                                │
      ▲                                                 │
      │  status.rs ◄── health.rs ◄── observe ◄──────────┘
      │  conditions,   conditions
      │  applied*      (pure)
      │
 controller.rs: watch Network + owned DaemonSets / Deployments /
 ConfigMaps in kube-system; resync 60 s → drift re-triggers a pass
```

Everything before `apply` is a pure function, so the whole install is tested
off-cluster: unit tests plus golden renders per mode (`tests/golden/`).

---

## The five modes (`src/modes.rs`)

| Mode | Routing | Encryption | LB-IPAM | Announce |
|---|---|---|---|---|
| **overlay** *(default)* | VXLAN tunnel | none | off | none |
| **native** | native + direct node routes | none | off | none |
| **bgp** | native + direct node routes | none | on | BGP |
| **encrypted** | VXLAN tunnel | WireGuard | off | none |
| **bare-metal** | VXLAN tunnel | none | on | L2 (ARP) |

All modes: cluster-pool IPAM (/24 per node), kube-proxy replacement, bpf host
routing, MTU auto. Any `spec.cilium.*` field overrides the mode; the result is
validated as a whole, and an invalid spec is **not applied** — the CR goes
`Degraded` with the field and the reason.

---

## What it does today — install and keep

From the code, working now:

- **Renders** as pure Rust, no Helm: `cilium-secrets` ns → SAs → RBAC
  (incl. TLS-interception, ztunnel) → `cilium-config` → Services →
  `DaemonSet/cilium` → `Deployment/cilium-operator` → `hubble-relay` →
  optional `cilium-envoy` → LB pool / L2 policy / BGP CRs (`src/render/`).
- **Pins by digest**: `version` → per-image linux/amd64 digests
  (`src/pins.rs`, from stormcos-cilium); an unpinned version is rejected
  unless `images` names each one by digest. Never a tag (Envoy aside, #20).
- **Parity with stormcos**: the default render is the same 23 objects,
  digests and overlapping `cilium-config` values as stormcos's static
  manifests — checked by `tests/parity.rs` against a verbatim copy.
- **Applies** with server-side apply, field manager `network-operator`.
- **Defers**, not fails, `cilium.io` CRs whose CRDs `cilium-operator` has not
  installed yet — requeues in 10 s (`WaitingForCiliumCRDs`).
- **Drift-heals**: watches its owned objects; an edit or deletion re-triggers
  a reconcile.
- **Reaps** what a new config no longer renders: Hubble relay + Services,
  `cilium-agent` Service, LB-IPAM / L2 / BGP CRs.
- **Owner references** on everything: deleting the CR garbage-collects the
  install.

---

## What it does today — guard and report

- **Immutability** (`src/immutable.rs`): datapath family (tunnel ↔ native),
  IPAM mode, pod and service CIDRs are fixed after the first successful apply.
  Checked against `status.applied*`; a rejected change keeps the running
  config, and a failed pass never moves the baseline.
- **Live changes**: encryption, LB/BGP/L2, MTU, Cilium version (a rolling
  upgrade), kube-proxy replacement, host routing, Envoy, cluster name/ID.
- **Health** (`src/health.rs`):
  - `Available` — agents all ready, operator ≥ 1 ready, Envoy (if on) all
    ready, `cilium.io` CRDs Established, a `CiliumNode` per ready agent
  - `Progressing` — installing, CRDs deferred / not Established, or rolling out
  - `Degraded` — the pass failed, or a `k8s-app=cilium` (or `cilium-envoy`) pod crash-loops (≥ 3 restarts)
- **rustkube fallbacks** (`apply.rs`, `status.rs`): create/replace when PATCH
  is missing; `/status` patch → PUT → whole-object PUT.

---

## Interfaces

**The `Network` CR** — `network.storm.io/v1`, cluster-scoped, short name `net`,
named `cluster`. Required: `clusterNetwork`, `serviceNetwork`,
`cilium.k8sServiceHost`. `kubectl get net` → Mode, Version, Available,
Progressing, Degraded.

**CLI** (`src/main.rs`) — no config file:

```
network-operator [--log <filter>] [--log-json] [--health-addr <addr>] [run | dry-run [FILE]]
  --log          RUST_LOG     default info
  --log-json     LOG_JSON     JSON lines
  --health-addr  HEALTH_ADDR  default 0.0.0.0:9446
  run         the controller (default)
  dry-run     render a CR to YAML, no cluster  (make dry-run FILE=…)
```

**Ports** — the operator: 9446 `/healthz` (liveness), `/readyz`
(readiness), `/metrics` (reconcile counts, last success). Rendered (host network):
agent 9879 `/healthz`, 9962 metrics, 9964 proxy metrics (embedded proxy
only), 4244 Hubble, 9965 Hubble metrics; cilium-operator 9234 (loopback),
9963 metrics; hubble-relay 4245; envoy 9878 health / 9964 metrics;
8472/udp VXLAN.

---

## How it ships and runs

- **Image**: static musl binary on `scratch`, built `--locked`. Distributed as
  a **gzipped OCI archive on each GitHub release** → `podman load` →
  `localhost/network-operator:<version>`; must be preloaded (never pulled) —
  nothing preloads it today.
- **Packages**: `.rpm` / `.deb` with the binary, `crdgen`, CRD, manifests,
  examples.
- **Golden**: **none** (whether it should get one: decision #18) — not in
  stormcentral's golden builds, and not in the stormcos image.
- **Starts** from `deploy/operator.yaml`: 1-replica `Recreate` Deployment in
  `kube-system`, **hostNetwork**, control-plane nodes, `system-cluster-critical`
  — so it runs on a node with no CNI and brings Cilium up underneath itself.
  Registers its own CRD, and updates its schema on upgrade (never over a
  newer operator's).
- **Updated**: Cilium by editing `spec.cilium.version`; the operator by loading
  a new archive and reapplying `deploy/operator.yaml` — it brings its CRD
  schema up to date itself.
- **Built and tested** with `sc-build` on the build box, never as root; on a
  running cluster by the `test/` container (short / medium / long suites),
  which stormcentral runs on every test machine.

---

## Planned — not in the code yet

Each is an open issue; the docs say so rather than promise it.

- **#23** (decision) `k8sServiceHost`: optional and kubelet-injected (as
  stormcos does), resolved at reconcile time, or still required.
- Hubble relay across nodes (it reads its own node's socket today).
- **#24** (decision) a validating webhook for immutability: serving cert,
  reachable before the CNI, `failurePolicy`.
- Out of scope today: Geneve / tunnel-port override, IPv6 / dual-stack,
  IPsec (rejected at validation).

---

## Status and the issues that matter

- **Works**: renders, applies, drift-heals and reports for all five modes plus
  standalone Envoy, with stormcos's Cilium objects and digests (parity test).
  132 tests (unit + golden + parity) pass under `sc-build`.
- **Proven on hardware**: 2026-07-20, rustkube v0.7.29 + fastetcd v1.0.4 +
  rustkube-node v0.2.0 — an `overlay` install came fully up (agent OK, BPF
  loaded, `CiliumNode` created, pods Running first try). That was Cilium
  1.19.6; the 1.20.2 render has not been on a cluster yet (stormcos runs
  the same 1.20.2 objects from its own manifests).
- **Most pressing**:
  - **#21** — stormcos does not ship this operator; it runs Cilium from
    static manifests. The operator must not be deployed on a stormcos node.
  - **stormcos#135 / stormcentral#56 / #55** — the `test/` suites are built
    but no run reaches a cluster yet: the test machine's registry refuses
    connections (stormcos#135), and the runner's `@@RESULT` fix (#56) is
    unexercised. Then the `Network` checks report *could not run* until test
    runs get cluster-scoped read (#55).
  - **Decisions**: #18 (a golden or not), #19 (where drift-heal is tested),
    #23 (`k8sServiceHost`).
  - **#8** QA tests + must-gather (collector: stormcos_qa#22), **#7** release profile.
