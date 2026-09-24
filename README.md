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

Version **0.2.4**. Everything below is read from the source at that version;
where the code does not do something yet, this README says so.

## What it does today

- **One binary, two commands:** `run` (the controller, the default) and
  `dry-run` (render a `Network` manifest to YAML, no cluster).
- **Self-registers its CRD** on start (`Network.network.storm.io/v1`,
  create-if-absent — see [Known gaps](#known-gaps)).
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
- **Reaps** the LB-IPAM / L2 / BGP CRs a previous config rendered and this
  one does not.
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
| 1 | `ServiceAccount/cilium`, `ServiceAccount/cilium-operator` | always |
| 2 | `ClusterRole` + `ClusterRoleBinding` `cilium`, `cilium-operator` | always |
| 3 | `Role` + `RoleBinding` `cilium-config-agent` (read ConfigMaps in `kube-system`) | always |
| 4 | `ConfigMap/cilium-config` | always |
| 5 | `DaemonSet/cilium` (agent; hostNetwork, `system-node-critical`, rolling update `maxUnavailable: 2`) | always |
| 6 | `Deployment/cilium-operator` (`operator-generic` image, 1 replica, hostNetwork, `system-cluster-critical`) | always |
| 7 | `ServiceAccount`, `ConfigMap/cilium-envoy-config`, `DaemonSet`, headless `Service` — all `cilium-envoy` | `spec.cilium.envoy.enabled: true` |
| 8 | `CiliumLoadBalancerIPPool/storm-default` | LB-IPAM on |
| 9 | `CiliumL2AnnouncementPolicy/storm-default` (`cilium.io/v2alpha1`) | `announce: l2` |
| 10 | `CiliumBGPClusterConfig/storm`, `CiliumBGPPeerConfig/storm-peers`, `CiliumBGPAdvertisement/storm-advertisements` | `announce: bgp` |

The `cilium.io` CRs are applied last because `cilium-operator` installs their
CRDs itself: on a fresh install a missing CRD is **deferred**, not failed, and
the reconcile requeues in 10 s with `Progressing=True
(WaitingForCiliumCRDs)`. The LB pool and BGP CRs are addressed at
`cilium.io/v2` for Cilium ≥ 1.17 and `v2alpha1` below that.

`tests/golden/*.yaml` holds the full render for each mode (plus
`overlay-envoy`); those files are the exact object list per mode.

Compared with what the stormcos image ships today, the render is missing
Hubble relay and the TLS-interception RBAC / `cilium-secrets` namespace —
tracked in [#9](https://github.com/glennswest/network-operator/issues/9).

## The `Network` custom resource

`network.storm.io/v1`, kind `Network`, plural `networks`, short name `net`,
**cluster-scoped**, conventionally named `cluster`, with a `/status`
subresource. `kubectl get net` prints Mode, Version, Available, Progressing,
Degraded. The CRD in `deploy/crds/` is generated from `src/crd.rs`
(`make crds`) — never hand-edit it.

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
    version: "1.19.6"
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
| `spec.cilium.version` | `1.19.6` | Image tag for agent + operator; a leading `v` is added if missing. Bumping it is a rolling upgrade. |
| `spec.cilium.registry` | `quay.io/cilium` | Prefix for `cilium`, `operator-generic` and `cilium-envoy` images. |
| `spec.cilium.ipam.mode` | mode (`cluster-pool`) | `cluster-pool` \| `kubernetes`. Immutable. |
| `spec.cilium.ipam.clusterPoolIPv4MaskSize` | `24` | 1–32 and strictly longer than every `clusterNetwork` prefix (cluster-pool only). |
| `spec.cilium.routing.mode` | mode | `tunnel` (VXLAN, port 8472 — not configurable) \| `native` (sets `ipv4-native-routing-cidr` = clusterNetwork and `auto-direct-node-routes: true`). Immutable. |
| `spec.cilium.mtu` | `0` | 0 = let the agent detect it (key omitted); otherwise 576–9216. |
| `spec.cilium.kubeProxyReplacement` | `true` | eBPF service handling in place of kube-proxy. |
| `spec.cilium.hostRouting` | `bpf` | `bpf` \| `legacy` (`enable-host-legacy-routing`). |
| `spec.cilium.encryption.type` | mode (`none`) | `none` \| `wireguard`. `ipsec` is **rejected** (no keyfile Secret management). |
| `spec.cilium.k8sServiceHost` | — (**required**) | Apiserver address the agent, operator and Envoy dial; with kube-proxy replacement there is no Service route to it until Cilium is up. |
| `spec.cilium.k8sServicePort` | `6443` | Non-zero. |
| `spec.cilium.loadBalancer.ipam` | mode (`false`) | LB-IPAM for `type: LoadBalancer`. When on, `pools` is required. |
| `spec.cilium.loadBalancer.pools` | `[]` | CIDRs for `CiliumLoadBalancerIPPool/storm-default`. |
| `spec.cilium.loadBalancer.announce` | mode (`none`) | `none` \| `l2` \| `bgp`. Anything but `none` requires `ipam: true`; `bgp` requires native routing. |
| `spec.cilium.loadBalancer.bgp.localASN` | `0` | Required (1–4294967295) when announcing via BGP. |
| `spec.cilium.loadBalancer.bgp.peers[]` | `[]` | `{address, asn}`; at least one for BGP; address must be an IP. Every node peers (no node selector); advertises PodCIDR + LoadBalancer IPs. |
| `spec.cilium.envoy.enabled` | `false` | Split the L7 proxy into the `cilium-envoy` DaemonSet. |
| `spec.cilium.envoy.image` | `<registry>/cilium-envoy:v1.36.9-1782267392-edeb3f2…` | Envoy is versioned independently of Cilium; the default pairs with 1.19. Set it explicitly on any other Cilium minor. |
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
there is **no validating webhook**. A rejected change leaves the running
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
4. **Reap** LB/L2/BGP CRs this config no longer renders.
5. **Observe** health and write status, including the new `applied*`.

Requeue: 60 s after success, 10 s while Cilium CRs are deferred, 15 s after
an error. Any failure in steps 1–3 is written to the CR as
`Degraded=True (ReconcileFailed)`, while `Available` keeps reflecting the
workloads that are actually running.

### Health conditions (`src/health.rs`)

| Condition | Rule |
|---|---|
| `Available=True` | `DaemonSet/cilium` has ≥ 1 desired pod and all are ready, **and** `cilium-operator` has ≥ 1 ready replica. Otherwise False with `Installing`, `NoSchedulableNodes`, `AgentNotReady` or `OperatorNotReady`. |
| `Progressing=True` | workloads not created yet (`Installing`), Cilium CRs deferred (`WaitingForCiliumCRDs`), or either workload not fully ready *and* updated (`RolloutInProgress`). |
| `Degraded=True` | this pass failed (`ReconcileFailed`, with the error), or a pod labelled `k8s-app=cilium` is in `CrashLoopBackOff` with ≥ 3 restarts (`PodsCrashLooping`). |

`cilium-envoy`, `CiliumNode` objects and CRD establishment are **not** part
of the health rollup today.

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
network-operator [--log <filter>] [--log-json] [run | dry-run [FILE]]
```

| Flag | Env | Default | |
|---|---|---|---|
| `--log` | `RUST_LOG` | `info` | tracing filter, e.g. `network_operator=debug` |
| `--log-json` | `LOG_JSON` | off | JSON log lines |
| `run` | | (default command) | connect, register the CRD, run the controller |
| `dry-run [FILE]` | | `-` (stdin) | print the rendered YAML stream; nothing contacts a cluster |

`run` uses the standard kube client config: the in-cluster ServiceAccount
when deployed, otherwise `KUBECONFIG` / `~/.kube/config`. There is no config
file.

### Ports, health, metrics

**The operator itself listens on nothing**: no health endpoint, no metrics
endpoint, no probes on its Deployment (see [Known gaps](#known-gaps)). Its
health is visible as the `Network` conditions and its logs.

Ports in what it *renders* (all host ports, since those pods are
host-networked):

| Port | Where | What |
|---|---|---|
| 9879 | `cilium` agent | `/healthz` (`agent-health-port`); startup, liveness, readiness probes |
| 9234 | `cilium-operator` | `/healthz` on `127.0.0.1` (`operator-api-serve-addr`) |
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
| `make test` / `make clippy` | as above |
| `make crds` | regenerate `deploy/crds/network.storm.io_networks.yaml` from `src/crd.rs` |
| `make golden` | re-record `tests/golden/` after an intended render change — review the diff |
| `make dry-run FILE=examples/network-bgp.yaml` | render a CR without a cluster |
| `make image` | `podman build` → `localhost/network-operator:<version>` |
| `make packages` | `packaging/build-packages.sh`: `.rpm`, `.deb`, and the gzipped OCI archive, in `dist/` |

## How it ships

- **Container image**: static musl binary on `scratch` (`Dockerfile`), built
  `--locked` from the committed `Cargo.lock`. Entrypoint `/network-operator`,
  default command `run`. It is distributed as an **OCI archive attached to
  each GitHub release**, not through a registry:

  ```
  curl -L https://github.com/glennswest/network-operator/releases/download/v0.2.4/network-operator-0.2.4-oci.tar.gz \
    | gunzip | podman load        # -> localhost/network-operator:0.2.4
  ```

  `localhost/` is local-only to CRI-O, so nothing ever tries to pull it; the
  image must be preloaded on every node that may run the operator. The
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
  if absent — but it is how an existing CRD's schema gets updated.
- **Golden / stormcos**: there is **no golden** for network-operator —
  stormcentral's component registry and stormcos `deploy/build-goldens.sh`
  do not list it. stormcos's `kubernetes` edition declares it as a
  `container` component with `run = "deployment"` and preloads its image
  and the Cilium images it renders. The pins in that edition currently
  disagree with this repo (it names `ghcr.io/glennswest/network-operator:0.2.3`,
  Cilium `v1.20.1` and a `v1.37.5` Envoy); that is being reconciled in the
  stormcos consistency pass and
  [#9](https://github.com/glennswest/network-operator/issues/9).

## Relationship to the rest of the stack

- **rustkube** — the apiserver it talks to (kube-rs client, standard
  Kubernetes API).
- **rustkube-node** — the kubelet that runs the Cilium pods (the agent's
  `startupProbe` depends on its probe support).
- **stormcos-cilium** — pins the Cilium images (by digest) and chart that
  stormcos ships; network-operator's defaults must match it (#9).
- **stormlb** — the pre-cluster apiserver VIP; a separate concern.
  stormlb fronts the apiserver, network-operator manages in-cluster
  networking.

## Known gaps

What earlier versions of this README promised or implied, and the code does
not do yet:

- Immutability is enforced by the reconciler, **not** a validating webhook (#15).
- `Available` does not look at `CiliumNode` readiness, Cilium CRD
  establishment, or `cilium-envoy` (#14).
- Turning `envoy.enabled` off does **not** delete the `cilium-envoy` objects (#12);
  only the LB/L2/BGP CRs are reaped.
- The CRD is registered create-if-absent, so upgrading the operator does not
  update an existing CRD's schema; apply `deploy/crds/` on upgrade (#13).
- The operator exposes no health or metrics endpoint (#15).
- The tunnel protocol is VXLAN only (no Geneve, no port override); IPv6 and
  dual-stack are not supported; IPsec is rejected.
- Rendering is Rust code, not per-Cilium-version templates: `version` changes
  the image tags and the `cilium.io` API version, nothing else. Config keys
  that a newer Cilium renamed are not tracked (#9).
- Render parity with what stormcos ships (Hubble, TLS interception): #9.

## Validation

- `sc-build` on dev.g8.lo: `cargo build && cargo test` — unit tests plus the
  golden render tests, no cluster.
- 2026-07-20, on rustkube v0.7.29 + fastetcd v1.0.4 + rustkube-node v0.2.0:
  an `overlay` install matching this render came fully up (agent
  `cilium status: OK`, BPF programs loaded, `CiliumNode` created, all pods
  `Running` at attempt 0).

## License

Apache-2.0.
