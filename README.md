# network-operator

The **Cluster Network Operator** for the rustkube / stormcos stack, in Rust.

It installs and keeps **Cilium** running from a single declarative `Network`
custom resource: it renders every Cilium object from the CR, server-side
applies them, reapplies them when they drift, deletes the ones a config change
no longer wants, and reports health as `Available` / `Progressing` /
`Degraded` conditions. It is the stack's analog of OpenShift's **Cluster
Network Operator (CNO)**.

> **Not** the same as `cilium-operator`. `cilium-operator` is Cilium's *own*
> control plane (CRDs, IPAM allocation, identity/endpoint GC — it manages
> Cilium's **data**). `network-operator` *installs* `cilium-operator`, along
> with the agent DaemonSet, RBAC and config, and manages their
> **lifecycle**.

Version **0.3.0**. Everything below is read from the source at that version;
where the code does not do something yet, this README says so. A short
slide deck of the same material is in [`docs/presentation.md`](docs/presentation.md)
(Marp: `npx @marp-team/marp-cli docs/presentation.md`).

## What it does today

- **One binary, two commands:** `run` (the controller, the default) and
  `dry-run` (render a `Network` manifest to YAML, no cluster).
- **Self-registers its CRD** on start (`Network.network.storm.io/v1`), and
  updates its schema when the operator is upgraded (`src/register.rs`).
- **Resolves** the CR: `spec.mode` supplies defaults, any field set under
  `spec.cilium` overrides them, and the result is validated as a whole
  (`src/modes.rs`). A spec that fails validation is not applied; the CR goes
  `Degraded` with the field and the reason.
- **Refuses immutable changes** (datapath family, IPAM mode, pod and service
  CIDRs) by comparing against `status.applied*`, not by a webhook.
- **Renders** the object set as pure Rust (`src/render/`) — no Helm, no
  templates at runtime. The only embedded asset is the `cilium-envoy`
  bootstrap JSON.
- **Applies** each object with server-side apply (field manager
  `network-operator`, `force`), in dependency order, with fallbacks for
  rustkube apiservers.
- **Pins every Cilium image by digest**: `spec.cilium.version` resolves
  through a compiled-in table (`src/pins.rs`, copied from stormcos-cilium's
  `pinned.txt`, linux/amd64) to one digest per image. A tag is never pulled;
  an unpinned version is rejected unless the CR names each image by digest.
- **Reaps** what a previous config rendered and this one does not: the
  Hubble relay and Services, the `cilium-agent` Service, and the LB-IPAM /
  L2 / BGP CRs.
- **Watches** the `Network` plus every DaemonSet, Deployment and ConfigMap in
  `kube-system` that it owns (owner reference), so a hand edit or a deletion
  re-triggers a reconcile. It also resyncs every 60 s.
- **Reports** `Available` / `Progressing` / `Degraded` on `Network.status`,
  with `observedGeneration` and `lastTransitionTime` preserved across passes.

### What gets rendered

Every object lands in `kube-system` (cluster-scoped ones have no namespace),
carries `app.kubernetes.io/managed-by: network-operator`,
`app.kubernetes.io/part-of: cilium`, `network.storm.io/owner: <Network name>`,
and, on a live cluster, a controller owner reference to the `Network` — so
deleting the CR garbage-collects the install. Apply order:

| # | Object | When |
|---|---|---|
| 1 | `Namespace/cilium-secrets` (where the L7 proxy reads TLS-interception secrets) | always |
| 2 | `ServiceAccount/cilium`, `ServiceAccount/cilium-operator` | always |
| 3 | `ClusterRole` + `ClusterRoleBinding` `cilium`, `cilium-operator` | always |
| 4 | `Role` + `RoleBinding` `cilium-config-agent` (read ConfigMaps in `kube-system`) | always |
| 5 | `Role` + `RoleBinding` `cilium-tlsinterception-secrets` (agent reads) and `cilium-operator-tlsinterception-secrets` (operator writes), both in `cilium-secrets` | always — the L7 proxy is always on |
| 6 | `Role` + `RoleBinding` `cilium-operator-ztunnel` (operator manages the ztunnel DaemonSet in `kube-system`) | always, as the 1.20 chart does |
| 7 | `ConfigMap/cilium-config` | always |
| 8 | headless `Service/cilium-agent` (embedded proxy metrics, 9964) | Envoy not split out |
| 9 | headless `Service/hubble-metrics` (9965), `Service/hubble-peer` (80 → 4244, `internalTrafficPolicy: Local`) | Hubble on (default) |
| 10 | `DaemonSet/cilium` (agent; hostNetwork, `system-node-critical`, rolling update `maxUnavailable: 2`) | always |
| 11 | `Deployment/cilium-operator` (`operator-generic` image, 1 replica, hostNetwork, `system-cluster-critical`) | always |
| 12 | `ConfigMap/hubble-relay-config`, `Deployment/hubble-relay` (hostNetwork, :4245, dials the agent's `hubble.sock`) | Hubble on (default) |
| 13 | `ServiceAccount`, `ConfigMap/cilium-envoy-config`, `DaemonSet`, headless `Service` — all `cilium-envoy` | `spec.cilium.envoy.enabled: true` |
| 14 | `CiliumLoadBalancerIPPool/storm-default` | LB-IPAM on |
| 15 | `CiliumL2AnnouncementPolicy/storm-default` (`cilium.io/v2alpha1`) | `announce: l2` |
| 16 | `CiliumBGPClusterConfig/storm`, `CiliumBGPPeerConfig/storm-peers`, `CiliumBGPAdvertisement/storm-advertisements` | `announce: bgp` |

The `cilium.io` CRs are applied last because `cilium-operator` installs their
CRDs itself: on a fresh install a missing CRD is **deferred**, not failed, and
the reconcile requeues in 10 s with `Progressing=True
(WaitingForCiliumCRDs)`. The LB pool and BGP CRs are addressed at
`cilium.io/v2` for Cilium ≥ 1.17 and `v2alpha1` below that.

`tests/golden/*.yaml` holds the full render for each mode (plus
`overlay-envoy`); those files are the exact object list per mode.

**Parity with stormcos.** The default overlay render holds exactly the 23
objects the stormcos image ships as static manifests (see
[How it ships](#how-it-ships)), under the same kinds, namespaces and names,
with the same image digests; and `cilium-config` agrees with stormcos's on
every key both set, bar three listed with reasons in `tests/parity.rs`.
`tests/fixtures/stormcos/` is a verbatim copy of those manifests, and
`cargo test --test parity` checks all of it. What parity does **not** cover
is listed under [Known gaps](#known-gaps).

## The `Network` custom resource

`network.storm.io/v1`, kind `Network`, plural `networks`, short name `net`,
**cluster-scoped**, conventionally named `cluster`, with a `/status`
subresource. `kubectl get net` prints Mode, Version, Available, Progressing,
Degraded. The CRD in `deploy/crds/` is generated from `src/crd.rs`
(`make crds`) — never hand-edit it; a unit test fails if it is stale.

**Upgrades** (`src/register.rs`): the CRD carries the annotation
`network.storm.io/operator-version` naming the operator that wrote it. On
start the operator creates the CRD if absent, and otherwise replaces it
(GET, then PUT with its `resourceVersion` — an exact replace, since rustkube's
apply is a merge that would keep a removed property) whenever its spec or
stamp differs. A CRD stamped by a **newer** operator is left alone, with a
warning, so rolling the operator back does not strip a newer schema's
fields. An unstamped CRD (an older `deploy/crds/`, or an operator from before #13)
counts as older and is replaced.

```yaml
apiVersion: network.storm.io/v1
kind: Network
metadata:
  name: cluster
spec:
  cni: cilium
  mode: overlay
  clusterNetwork: ["10.244.0.0/16"]
  serviceNetwork: ["10.96.0.0/12"]
  cilium:
    version: "1.20.2"
    k8sServiceHost: "192.168.8.98"
    k8sServicePort: 6443
```

### Every field, with its default

"Mode" means the default comes from the mode table below.

| Field | Default | Notes |
|---|---|---|
| `spec.cni` | `cilium` | The only value. |
| `spec.mode` | `overlay` | `overlay` \| `native` \| `bgp` \| `encrypted` \| `bare-metal`. |
| `spec.clusterNetwork` | — (**required**) | Pod CIDR(s), at least one, each a valid CIDR. Immutable. |
| `spec.serviceNetwork` | — (**required**) | Service CIDR(s), at least one. Immutable. Recorded in status and guarded; not written into `cilium-config`. |
| `spec.cilium.version` | `1.20.2` | Resolved to a digest per image via `src/pins.rs` (pinned today: `1.20.2`); a leading `v` is accepted. An unpinned version is **rejected** unless `images` names each needed image by digest. Bumping it is a rolling upgrade. |
| `spec.cilium.images.agent` / `.operator` / `.hubbleRelay` | from the pin | Full references, each **must** be `<repository>@sha256:<64 hex>`; a tag is rejected. Each one set wins over the pin. `hubbleRelay` is only needed with Hubble on. |
| `spec.cilium.registry` | `quay.io/cilium` | Repository prefix for the pinned `cilium`, `operator-generic` and `hubble-relay` digests and the `cilium-envoy` default. A mirror serves the same bytes, so the digest carries over. |
| `spec.cilium.hubble.enabled` | `true` | The agent's Hubble server plus `hubble-relay`, `hubble-peer` and `hubble-metrics`. Turning it off deletes those objects. |
| `spec.cilium.ipam.mode` | mode (`cluster-pool`) | `cluster-pool` \| `kubernetes`. Immutable. |
| `spec.cilium.ipam.clusterPoolIPv4MaskSize` | `24` | 1–32 and strictly longer than every `clusterNetwork` prefix (cluster-pool only). |
| `spec.cilium.routing.mode` | mode | `tunnel` (VXLAN, port 8472 — not configurable) \| `native` (sets `ipv4-native-routing-cidr` = clusterNetwork and `auto-direct-node-routes: true`). Immutable. |
| `spec.cilium.mtu` | `0` | 0 = let the agent detect it (key omitted); otherwise 576–9216. |
| `spec.cilium.kubeProxyReplacement` | `true` | eBPF service handling in place of kube-proxy. |
| `spec.cilium.hostRouting` | `bpf` | `bpf` \| `legacy` (`enable-host-legacy-routing`). |
| `spec.cilium.encryption.type` | mode (`none`) | `none` \| `wireguard`. `ipsec` is **rejected** (no keyfile Secret management). |
| `spec.cilium.k8sServiceHost` | — (**required**) | Apiserver address the agent, operator and Envoy dial; with kube-proxy replacement there is no Service route to it until Cilium is up. Whether it should instead be left to the kubelet (rustkube-node injects it per node) or resolved at reconcile time is an owner decision, [#23](https://github.com/glennswest/network-operator/issues/23). |
| `spec.cilium.k8sServicePort` | `6443` | Non-zero. |
| `spec.cilium.loadBalancer.ipam` | mode (`false`) | LB-IPAM for `type: LoadBalancer`. When on, `pools` is required. |
| `spec.cilium.loadBalancer.pools` | `[]` | CIDRs for `CiliumLoadBalancerIPPool/storm-default`. |
| `spec.cilium.loadBalancer.announce` | mode (`none`) | `none` \| `l2` \| `bgp`. Anything but `none` requires `ipam: true`; `bgp` requires native routing. |
| `spec.cilium.loadBalancer.bgp.localASN` | `0` | Required (1–4294967295) when announcing via BGP. |
| `spec.cilium.loadBalancer.bgp.peers[]` | `[]` | `{address, asn}`; at least one for BGP; address must be an IP. Every node peers (no node selector); advertises PodCIDR + LoadBalancer IPs. |
| `spec.cilium.envoy.enabled` | `false` | Split the L7 proxy into the `cilium-envoy` DaemonSet. |
| `spec.cilium.envoy.image` | `<registry>/cilium-envoy:v1.36.9-1782267392-edeb3f2…` | The one image still named by **tag**: Envoy is versioned independently of Cilium, the default is from a 1.19 install, and nothing pins a digest for it yet (#20). Set it explicitly when running standalone Envoy on 1.20. |
| `spec.cilium.clusterName` | `default` | Non-empty. |
| `spec.cilium.clusterID` | `0` | 0–255. |

Status, written only by the operator: `conditions`, `observedGeneration`,
and `appliedMode`, `appliedVersion`, `appliedDatapath`, `appliedIpam`,
`appliedClusterNetwork`, `appliedServiceNetwork` — the baseline for the
immutability check, moved only by a successful reconcile.

## Network modes (install-time profiles)

A mode is a named set of defaults (`src/modes.rs`, `defaults_for`). Every
mode starts from the same base and changes only its own cells; any explicit
`spec.cilium.*` field wins. All modes: cluster-pool IPAM with /24 per node,
kube-proxy replacement on, bpf host routing, MTU auto.

| Mode | Routing | Encryption | LB-IPAM | Announce | Use when |
|---|---|---|---|---|---|
| **overlay** *(default)* | tunnel (VXLAN) | none | off | none | Any L2 segment; the safe default. |
| **native** | native + auto direct node routes | none | off | none | Nodes share an L2; no overlay. |
| **bgp** | native + auto direct node routes | none | **on** | **bgp** | Routed fabric; advertise pod CIDRs + LB VIPs. Needs `pools`, `localASN`, `peers`. |
| **encrypted** | tunnel (VXLAN) | **wireguard** | off | none | Untrusted underlay. |
| **bare-metal** | tunnel (VXLAN) | none | **on** | **l2** | `type: LoadBalancer` via ARP, no cloud LB. Needs `pools`. |

`examples/` has `network-overlay.yaml`, `network-bgp.yaml` and
`network-envoy.yaml`.

### OpenShift concept → what this sets

| OpenShift (OVN-Kubernetes) | Here |
|---|---|
| `networkType` | always Cilium; `mode` picks the profile |
| `clusterNetwork` / `serviceNetwork` | `spec.clusterNetwork` (cluster-pool CIDR) / `spec.serviceNetwork` |
| Geneve overlay vs local routes | `routing.mode: tunnel` (VXLAN) vs `native` |
| `gatewayConfig.routingViaHost` | `hostRouting: legacy` vs `bpf` |
| `mtu` | `cilium.mtu` |
| `genevePort` | no equivalent: VXLAN port is fixed at 8472 |
| `ipsecConfig` | `encryption.type: wireguard` (ipsec rejected) |
| MetalLB / external LB | LB-IPAM + L2 or BGP announcements |

### Immutability

Checked by the reconciler against `status.applied*` (`src/immutable.rs`) —
there is **no validating webhook** (whether to add one, and how it gets a
serving cert, is decision #24). A rejected change leaves the running
install on its applied config, sets `Degraded=True (ReconcileFailed)` listing
every violation, and does not move the baseline.

- **Immutable after first successful apply:** the datapath family
  (`tunnel` ↔ `native` — so `overlay`→`native` is rejected, but
  `overlay`→`encrypted` and `native`→`bgp` are allowed), IPAM mode,
  `clusterNetwork`, `serviceNetwork`.
- **Everything else** is reapplied live: encryption, LB/BGP/L2, MTU, version,
  kube-proxy replacement, host routing, Envoy, cluster name/ID.

## Reconcile loop

`src/controller.rs`, one pass:

1. **Resolve** the spec (mode defaults + overrides + validation).
2. **Check immutability** against `status.applied*`.
3. **Render and apply** every object in order (`src/apply.rs`).
4. **Reap** the conditional objects this config no longer renders: the Hubble
   relay and Services, the standalone `cilium-envoy` objects, and the LB/L2/BGP
   CRs (`render::reapable`).
5. **Observe** health and write status, including the new `applied*`.

Requeue: 60 s after success, 10 s while Cilium CRs are deferred, 15 s after
an error. Any failure in steps 1–3 is written to the CR as
`Degraded=True (ReconcileFailed)`, while `Available` keeps reflecting the
workloads that are actually running.

### Health conditions (`src/health.rs`)

| Condition | Rule |
|---|---|
| `Available=True` | all of: `DaemonSet/cilium` has ≥ 1 desired pod and all are ready; `cilium-operator` has ≥ 1 ready replica; with Envoy enabled, `DaemonSet/cilium-envoy` exists, has ≥ 1 desired pod and all are ready; every `cilium.io` CRD is `Established=True`; and every node with a ready agent pod has a `CiliumNode`. Otherwise False with the first failing reason, in that order: `Installing`, `NoSchedulableNodes`, `AgentNotReady`, `OperatorNotReady`, `EnvoyNotReady`, `CiliumCRDsNotEstablished`, `CiliumNodesMissing`. |
| `Progressing=True` | workloads (Envoy included, when enabled) not created yet (`Installing`), Cilium CRs deferred or a `cilium.io` CRD not yet Established (`WaitingForCiliumCRDs`), or a workload not fully ready *and* updated (`RolloutInProgress`). |
| `Degraded=True` | this pass failed (`ReconcileFailed`, with the error), or a pod labelled `k8s-app=cilium` (or `k8s-app=cilium-envoy`, with Envoy enabled) is in `CrashLoopBackOff` with ≥ 3 restarts (`PodsCrashLooping`). |

`CiliumNode` is checked by name against the nodes running a ready agent, so
a stale `CiliumNode` for a removed node does not matter; if the `CiliumNode`
kind is not registered at all, every such node counts as missing. Envoy is
observed only while `envoy.enabled` — a disabled one's DaemonSet is being
reaped, so it is ignored — and not at all on a pass whose spec failed to resolve
(whether it is wanted is then unknown).

### rustkube compatibility

The control plane is rustkube. `apply.rs` and `status.rs` carry fallbacks
for builds that predate its fixes; all of the rustkube issues they cite are
now closed, and the fallbacks stay for older apiservers:

- SSA 404 on a missing *object* (rustkube#45): the object is created directly.
  A 404 on a missing *kind* is still deferral for `cilium.io` CRs.
- No PATCH at all (405/501, rustkube#23): create, or replace with the current
  `resourceVersion`.
- Status write ladder: SSA patch on `/status` → PUT `/status` → whole-object
  PUT (re-reading the spec first).
- rustkube's apply-patch is a plain merge with no field ownership, so our
  fields are restored on drift but fields someone else *added* survive:
  drift-heal, not drift-purge.

## The operator process

### Command line and environment (`src/main.rs`)

```
network-operator [--log <filter>] [--log-json] [--health-addr <addr>] [run | dry-run [FILE]]
```

| Flag | Env | Default | |
|---|---|---|---|
| `--log` | `RUST_LOG` | `info` | tracing filter, e.g. `network_operator=debug` |
| `--log-json` | `LOG_JSON` | off | JSON log lines |
| `--health-addr` | `HEALTH_ADDR` | `0.0.0.0:9446` | the `/healthz`, `/readyz`, `/metrics` listener (`run` only) |
| `run` | | (default command) | connect, register the CRD, run the controller |
| `dry-run [FILE]` | | `-` (stdin) | print the rendered YAML stream; nothing contacts a cluster |

`run` uses the standard kube client config: the in-cluster ServiceAccount
when deployed, otherwise `KUBECONFIG` / `~/.kube/config`. There is no config
file.

### Ports, health, metrics

**The operator itself** listens on `--health-addr` (default `0.0.0.0:9446`,
a host port since the pod is host-networked; `src/metrics.rs`), plain HTTP:

| Path | Answers |
|---|---|
| `/healthz` (`/livez`) | 200 while the process serves — the Deployment's `livenessProbe` |
| `/readyz` | 503 until the Network CRD is registered and the controller has started, then 200 — the `readinessProbe` |
| `/metrics` | Prometheus text (below) |

| Metric | Type | |
|---|---|---|
| `network_operator_build_info{version}` | gauge | always 1 |
| `network_operator_ready` | gauge | 1 once `/readyz` would answer 200 |
| `network_operator_reconciles_total{result="success"\|"error"}` | counter | reconcile passes |
| `network_operator_reconcile_errors_total{reason}` | counter | failed passes: `invalid` (spec does not resolve), `immutable`, `apply`, `kube` (status/observe/reap calls) |
| `network_operator_last_reconcile_success_timestamp_seconds` | gauge | Unix time of the last good pass, 0 = never |
| `network_operator_last_reconcile_duration_seconds` | gauge | the last pass, either outcome |

The listener binds before the CRD is registered, so a port clash stops the
operator at start. Cilium's own health is still read from the `Network`
conditions.

Ports in what it *renders* (all host ports, since those pods are
host-networked):

| Port | Where | What |
|---|---|---|
| 9879 | `cilium` agent | `/healthz` (`agent-health-port`); startup, liveness, readiness probes |
| 9962 | `cilium` agent | Prometheus metrics (`prometheus-serve-addr`) |
| 9964 | `cilium` agent | embedded proxy's metrics (`proxy-prometheus-port`); `Service/cilium-agent`. Not declared while Envoy is standalone — `cilium-envoy` binds it then |
| 4244 | `cilium` agent | Hubble server (`hubble-listen-address`), TLS off; `Service/hubble-peer` (Hubble on) |
| 9965 | `cilium` agent | Hubble metrics (`hubble-metrics-server`); `Service/hubble-metrics` (Hubble on) |
| 9234 | `cilium-operator` | `/healthz` on `127.0.0.1` (`operator-api-serve-addr`) |
| 9963 | `cilium-operator` | Prometheus metrics (`operator-prometheus-serve-addr`) |
| 4245 | `hubble-relay` | the flow API (gRPC), TLS off (Hubble on) |
| 9878 | `cilium-envoy` | health listener (loopback) |
| 9964 | `cilium-envoy` | Prometheus metrics; exposed by the headless `cilium-envoy` Service |
| 8472/udp | every node | VXLAN (tunnel modes) |

## Build and test

Builds and tests run on the build box, never on the session VM and never as
root. Push first, then:

```
sc-build                          # cargo build && cargo test, on dev.g8.lo
sc-build 'cargo clippy --all-targets -- -D warnings'
```

`sc-build` fetches the pushed commit into a scratch directory as the
unprivileged `stormbuild` user, runs the command and deletes the directory.
The tests are pure (unit + golden); no cluster is needed.

Make targets (run through `sc-build 'make …'` where they need cargo):

| Target | Does |
|---|---|
| `make build` | `cargo build --release` |
| `make test` / `make clippy` | as above |
| `make crds` | regenerate `deploy/crds/network.storm.io_networks.yaml` from `src/crd.rs` |
| `make golden` | re-record `tests/golden/` after an intended render change — review the diff |
| `make dry-run FILE=examples/network-bgp.yaml` | render a CR without a cluster |
| `make image` | `podman build` → `localhost/network-operator:<version>` |
| `make deploy` | `kubectl apply` of `deploy/crds/` then `deploy/operator.yaml` |
| `make packages` | `packaging/build-packages.sh`: `.rpm`, `.deb`, and the gzipped OCI archive, in `dist/` |

### Tests on a running cluster (`test/`)

The test container per stormcentral's test standard: one image,
`/test short|medium|long`, run by stormcentral as a Job in its own namespace
on each test machine. **short** (< 2 min) checks the `Network` is Available
and current, and that a pod gets a pod-CIDR address, answers pod to pod and
behind a ClusterIP Service. **medium** (< 30 min) adds the `status.applied*`
baseline, Service scale-out and backend loss, NetworkPolicy deny/allow,
cross-node, LoadBalancer IPAM and CiliumEndpoint cleanup. **long** runs
overnight waves sized from node capacity and fails on slowdown or residue.
Every test, its skips and what a run needs are in
[`test/README.md`](test/README.md). Until stormcentral grants test runs the
cluster-scoped read of `networks` and `nodes` (stormcentral#55), the tests
that read them report *could not run* (exit 2).

**Status, 2026-09-27: no suite has run on a cluster yet.** The first run
(a845a17d5c) built and pushed the image, then errored on the runner's own
`@@RESULT` parse before creating the Job (stormcentral#56; fixed in
stormcentral, not yet exercised). The next (dcae784bb9) stopped earlier: the
test machine's node registry (C2NR0Q2, sbregistry on :5100) refuses
connections (stormcos#135, stormcentral#71). Drift-heal is not among the suites: a test Job may not
touch `kube-system`, so where it is tested is an open decision (#19).

```
sc-build 'cd test && cargo test --locked'                    # the suites' own unit tests
stormcentral test run network-operator short --url http://stormcentral.g8.lo
```

## How it ships

- **Container image**: static musl binary on `scratch` (`Dockerfile`), built
  `--locked` from the committed `Cargo.lock`. Entrypoint `/network-operator`,
  default command `run`. It is distributed as an **OCI archive attached to
  each GitHub release**, not through a registry:

  ```
  curl -L https://github.com/glennswest/network-operator/releases/download/v0.3.0/network-operator-0.3.0-oci.tar.gz \
    | gunzip | podman load        # -> localhost/network-operator:0.3.0
  ```

  v0.3.0 is tagged but has no release assets yet: the build box keeps no
  artifacts, and how this image should be built and shipped is #18. The
  newest published archive is v0.2.4.

  `localhost/` is local-only to CRI-O, so nothing ever tries to pull it; the
  image must be preloaded on every node that may run the operator (nothing
  preloads it today — stormcos does not; see below). The
  package build fails if `deploy/operator.yaml` and the archive disagree on
  the tag.
- **`.rpm` / `.deb`** with the binary, `network-operator-crdgen`, the CRD,
  `deploy/operator.yaml` and the examples under
  `/usr/share/network-operator/`.
- **Deployment** (`deploy/operator.yaml`): `ServiceAccount`, a cluster-admin
  equivalent `ClusterRole` (it creates the Cilium ClusterRoles, and RBAC
  escalation prevention requires it to hold their union), and a 1-replica
  `Recreate` Deployment in `kube-system` — hostNetwork, control-plane node
  selector, tolerates everything, `system-cluster-critical`, read-only root,
  no capabilities. hostNetwork is what lets it start on a node with no CNI and
  then bring Cilium up underneath itself.

  ```
  kubectl apply -f deploy/operator.yaml
  kubectl apply -f examples/network-overlay.yaml   # edit k8sServiceHost first
  kubectl get net cluster
  ```

  Applying `deploy/crds/` first is optional — the operator registers the CRD
  if absent and updates an older one on start.
- **Golden / stormcos**: there is **no golden** for network-operator —
  stormcentral's component registry and stormcos `deploy/build-goldens.sh`
  do not list it. Whether it should get one is an owner decision,
  [#18](https://github.com/glennswest/network-operator/issues/18).
  **stormcos does not ship, preload or run network-operator.** Its image
  (`deploy/image.toml`) has no network-operator member, and the crate that
  used to preload container images (`stormcos-compose`) was deleted
  (stormcos#42). stormcos `editions/kubernetes.toml` still lists
  network-operator, but that file is the pre-pivot plan, kept only for a CI
  lint, and says it is not what the image carries.

  Instead a stormcos node runs Cilium from **static manifests**
  (`deploy/manifests/10-cilium-namespace.yaml` … `70-cilium-operator.yaml`,
  `75-hubble-relay.yaml`), byte-identical to stormcos-cilium's render and
  shipped inside the apiserver golden, with Cilium goldens built from
  `deploy/pinned-images.txt` (v1.20.2). There is no `Network` CR on a node.

  > **Do not deploy network-operator on a stormcos node.** Its render and
  > the static manifests own the same objects — `DaemonSet/cilium`,
  > `Deployment/cilium-operator` and `ConfigMap/cilium-config` in
  > `kube-system` — so the two would overwrite each other on every
  > reconcile.

  Pins, for when the two are compared (#9):

  | | Cilium | Envoy |
  |---|---|---|
  | this repo (`src/pins.rs`, by digest) | `1.20.2` | `v1.36.9-1782267392-…` (tag; standalone Envoy only) |
  | stormcos-cilium `pinned.txt` / stormcos `deploy/pinned-images.txt` (by digest) | `v1.20.2` | — |

  The Cilium digests are the same bytes (`tests/parity.rs` checks them).
  stormcos-cilium runs Envoy inside the agent, as this operator does by
  default, and pins no `cilium-envoy` image; where the standalone tag should
  be pinned is #20.

## Relationship to the rest of the stack

- **rustkube** — the apiserver it talks to (kube-rs client, standard
  Kubernetes API).
- **rustkube-node** — the kubelet that runs the Cilium pods (the agent's
  `startupProbe` depends on its probe support).
- **stormcos-cilium** — pins the Cilium images (by digest) and renders the
  manifests stormcos ships (v1.20.2 today). `src/pins.rs` copies its
  digests, and `tests/fixtures/stormcos/` its rendered manifests; both are
  refreshed by hand when it moves its pin.
- **stormcos** — does **not** use network-operator: it ships Cilium as
  static manifests rendered by stormcos-cilium (see
  [How it ships](#how-it-ships)); the two must not run on the same cluster.
- **stormcentral** — runs the `test/` suites on its test machines, and is
  where a golden would be built if #18 decides for one.
- **stormlb** — a separate concern: it owns inbound, network-operator
  manages in-cluster networking. What ships on a stormcos node is its
  **router only** (`[router]`, the L7 Host-header demux over HTTPRoutes on
  port 80); its VIP half (the pre-cluster apiserver VIP, VRRP/BGP) is
  implemented but no shipped node runs it (#22). Neither half overlaps the
  Cilium Service data plane this operator configures.

## Known gaps

What earlier versions of this README promised or implied, and the code does
not do yet:

- Immutability is enforced by the reconciler, **not** a validating webhook
  (decision #24: cert source, reachability before the CNI, `failurePolicy`).
- The tunnel protocol is VXLAN only (no Geneve, no port override); IPv6 and
  dual-stack are not supported; IPsec is rejected.
- Rendering is Rust code, not per-Cilium-version templates: `version` changes
  the image digests and the `cilium.io` API version, nothing else. Config
  keys that a newer Cilium renamed are not tracked; the agent and operator
  are shaped like the 1.20.2 chart.
- Parity with stormcos is by object and by overlapping key, not byte for
  byte. `cilium-config` sets 62 keys; stormcos's chart render sets 170 (51 in common).
  The rest are left to the agent's defaults, which is not always what the
  chart writes — e.g. stormcos names `devices: stormbr0`, which has no CRD
  field here. The agent's `clustermesh-secrets` volume is not rendered
  (no ClusterMesh).
- `hubble-relay` dials the agent's unix socket on its own node (stormcos's
  shape), so on a multi-node cluster it sees one node's flows; pointing it
  at `hubble-peer` is not wired.
- Pins are linux/amd64 only, and only for 1.20.2.
- `k8sServiceHost` is still required and written into every pod, which
  stormcos deliberately does not do: decision #23.

## Validation

- `cargo test --test parity`: the render against a verbatim copy of
  stormcos's Cilium manifests — object set, image digests, overlapping
  `cilium-config` keys (see [What gets rendered](#what-gets-rendered)).
- `sc-build` on dev.g8.lo: `cargo build && cargo test` — unit tests plus the
  golden render tests, no cluster.
- `test/`: the suites' unit tests under `sc-build`; the image built and
  smoke-run with podman on dev.g8.lo, and built + pushed by stormcentral's
  runner. Not yet run on a cluster (stormcos#135, then stormcentral#56).
- Drift-heal (reapply after a hand edit or deletion in `kube-system`) is
  covered by unit tests of `apply`/`reapable` only; no test exercises it on a
  running cluster (#19).
- Must-gather: stormcos_qa has no network collector yet (stormcos_qa#22,
  from #8).
- 2026-07-20, on rustkube v0.7.29 + fastetcd v1.0.4 + rustkube-node v0.2.0:
  an `overlay` install matching this render came fully up (agent
  `cilium status: OK`, BPF programs loaded, `CiliumNode` created, all pods
  `Running` at attempt 0).

## License

Apache-2.0.
