---
marp: true
theme: default
paginate: true
title: network-operator
description: Purpose and functionality of network-operator, the Cluster Network Operator for rustkube / stormcos
---

<!-- Render: npx @marp-team/marp-cli docs/presentation.md   (add --pdf for PDF)
     Every claim here is checkable against the code at v0.2.4; the source file
     is named on each slide. Keep it in step with README.md. -->

# network-operator

**The Cluster Network Operator for rustkube / stormcos**

Cilium from one `Network` custom resource — installed, kept, healed, reported.

v0.2.4 · Rust · Apache-2.0 · github.com/glennswest/network-operator

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
                 stormcos  (kubernetes edition ships it: run = "deployment")
                     │
                     ▼
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
- Depended on by **stormcos**: `editions/kubernetes.toml` declares it and
  preloads its and Cilium's images. (stormcentral's graph misses this edge —
  stormcentral#52.)
- Not **stormlb**: that fronts the apiserver; this manages pod networking.

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

- **Renders** as pure Rust, no Helm: SAs → RBAC → `cilium-config` →
  `DaemonSet/cilium` → `Deployment/cilium-operator` → optional
  `cilium-envoy` → LB pool / L2 policy / BGP CRs (`src/render/`).
- **Applies** with server-side apply, field manager `network-operator`.
- **Defers**, not fails, `cilium.io` CRs whose CRDs `cilium-operator` has not
  installed yet — requeues in 10 s (`WaitingForCiliumCRDs`).
- **Drift-heals**: watches its owned objects; an edit or deletion re-triggers
  a reconcile.
- **Reaps** LB-IPAM / L2 / BGP CRs a new config no longer renders.
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
  - `Available` — agent DaemonSet all ready **and** operator ≥ 1 ready
  - `Progressing` — installing, CRDs deferred, or rolling out
  - `Degraded` — the pass failed, or a `k8s-app=cilium` pod crash-loops (≥ 3 restarts)
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
network-operator [--log <filter>] [--log-json] [run | dry-run [FILE]]
  --log       RUST_LOG   default info
  --log-json  LOG_JSON   JSON lines
  run         the controller (default)
  dry-run     render a CR to YAML, no cluster  (make dry-run FILE=…)
```

**Ports** — the operator listens on **none**. Rendered (host network):
agent 9879 `/healthz`, cilium-operator 9234 (loopback), envoy 9878 health /
9964 metrics, 8472/udp VXLAN.

---

## How it ships and runs

- **Image**: static musl binary on `scratch`, built `--locked`. Distributed as
  a **gzipped OCI archive on each GitHub release** → `podman load` →
  `localhost/network-operator:<version>`; preloaded on nodes, never pulled.
- **Packages**: `.rpm` / `.deb` with the binary, `crdgen`, CRD, manifests,
  examples.
- **Golden**: **none** (whether it should get one: decision #18) — not in stormcentral's golden builds; stormcos's
  kubernetes edition carries it as a `container` component.
- **Starts** from `deploy/operator.yaml`: 1-replica `Recreate` Deployment in
  `kube-system`, **hostNetwork**, control-plane nodes, `system-cluster-critical`
  — so it runs on a node with no CNI and brings Cilium up underneath itself.
  Registers its own CRD if absent.
- **Updated**: Cilium by editing `spec.cilium.version`; the operator by loading
  a new archive and reapplying `deploy/operator.yaml` **and** `deploy/crds/`
  (the CRD is not self-updated — #13).
- **Built and tested** with `sc-build` on the build box, never as root; on a
  running cluster by the `test/` container (short / medium / long suites),
  which stormcentral runs on every test machine.

---

## Planned — not in the code yet

Each is an open issue; the docs say so rather than promise it.

- **#9** render parity with what stormcos ships: Hubble relay,
  TLS-interception RBAC, `cilium-secrets` namespace; the stormcos-cilium pin (v1.20.2, by digest).
- **#12** turning Envoy off should delete the `cilium-envoy` objects.
- **#13** the operator should update its CRD schema on upgrade.
- **#14** `Available` should include `CiliumNode` readiness, CRD
  establishment and `cilium-envoy`.
- **#15** a health/metrics endpoint for the operator; a validating webhook
  for immutability.
- Out of scope today: Geneve / tunnel-port override, IPv6 / dual-stack,
  IPsec (rejected at validation).

---

## Status and the issues that matter

- **Works**: renders, applies, drift-heals and reports for all five modes plus
  standalone Envoy. 95 tests (unit + golden) pass under `sc-build`.
- **Proven on hardware**: 2026-07-20, rustkube v0.7.29 + fastetcd v1.0.4 +
  rustkube-node v0.2.0 — an `overlay` install came fully up (agent OK, BPF
  loaded, `CiliumNode` created, pods Running first try).
- **Most pressing**:
  - **#9 / stormcos#79 / stormcos#133** — three pin sources: this repo
    renders 1.19.6 and ships `localhost/…:0.2.4`; the stormcos edition preloads
    Cilium 1.20.1, Envoy 1.37.5 and `ghcr…:0.2.3`; stormcos-cilium pins 1.20.2.
    A mismatch means a runtime pull on a node with no CNI.
  - **stormcos#135 / stormcentral#56 / #55** — the `test/` suites are built
    but no run reaches a cluster yet: the test machine's registry refuses
    connections (stormcos#135), and the runner's `@@RESULT` fix (#56) is
    unexercised. Then the `Network` checks report *could not run* until test
    runs get cluster-scoped read (#55).
  - **Decisions**: #18 (a golden or not), #19 (where drift-heal is tested).
  - **#8** QA tests + must-gather, **#7** release profile.
