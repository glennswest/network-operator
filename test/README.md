# network-operator-test

network-operator's test container, per stormcentral's
[`docs/test-standard.md`](https://github.com/glennswest/stormcentral/blob/main/docs/test-standard.md).
Unit and golden tests of the render pipeline stay in the root crate
(`cargo test`, via `sc-build`); this tests a **running** network-operator
and the network it installed, from inside a cluster.

One image serves every suite. stormcentral runs it as a Job in a fresh
namespace per run, starting it as `/test short|medium|long`. The workloads
the suites start are this same image (`/test workload serve 8080`, an HTTP
server that answers with its own pod name), so nothing else is pulled.

## Suites

| suite | budget | tests |
|---|---|---|
| `short` | < 2 min | `network-status`, `pod-ip`, `pod-to-pod`, `service`, `cleanup` |
| `medium` | < 30 min | everything in short, then `network-config`, `service-scale`, `backend-loss`, `network-policy`, `cross-node`, `load-balancer`, `endpoints-reaped`, `cleanup-all` |
| `long` | the night window (8 h unless `STORM_TIMEOUT`) | `network-status`, `capacity`, `wave-1` … `wave-N`, `trend`, `vm-waves` (skip), `network-status-end`, `cleanup` |

- **network-status**: the `Network` (named `cluster`, else the only one)
  has `Available=True`, `Degraded` not `True`, and `observedGeneration`
  equal to `metadata.generation`.
- **pod-ip**: a workload pod goes Ready with an IP inside the applied pod
  CIDR (`status.appliedClusterNetwork`, else `spec.clusterNetwork`).
- **pod-to-pod**: this Job's pod reaches it on `:8080` and gets its name back.
- **service**: a ClusterIP Service gets an address inside the service CIDR
  and reaches the pod. With kube-proxy replacement that is Cilium's eBPF
  service path.
- **network-config**: `status.applied*` equals what the operator's own
  `modes::resolve_network` + `immutable::applied_from` produce for the spec,
  so the immutability baseline is current.
- **service-scale**: a Deployment scaled from 1 to 3 replicas; the Service
  reaches all three backends.
- **backend-loss**: one backend deleted. The replacement gets a pod-CIDR IP
  and answers, and the Service then answers 20 times in a row, never from
  the dead pod.
- **network-policy**: a deny-all-ingress NetworkPolicy stops traffic. An
  allow from this Job's label (`storm.io/component=network-operator`)
  restores it, and so does removing both.
- **cross-node**: a pod pinned to another schedulable node is reachable.
  It is a **skip** (`requires min-nodes: 2`) on a single-node machine.
- **load-balancer**: a `LoadBalancer` Service gets an IP from
  `loadBalancer.pools` and answers. It is a **skip** (`requires
  loadBalancer.ipam`) when the resolved config has LB-IPAM off (modes
  `overlay`, `native`, `encrypted` by default).
- **endpoints-reaped**: Cilium holds at least one CiliumEndpoint per ready
  workload pod, and none are left once the workloads are deleted.
- **wave-N** (long): Deployments of 10 pods, each behind a ClusterIP Service,
  sized at 100% / 50% / 75% of half the schedulable nodes' allocatable pods
  (capped at 600). Each wave probes every pod and Service, deletes one pod
  per Deployment, waits for replacements, probes the Services again,
  re-reads the `Network`, then drains. The result line carries `pods`,
  `ramp_ms`, `ready_p95_ms`, `reach_p95_ms`, `reach_fail`, `drain_ms` and
  `residue`.
- **trend** (long): each wave is compared with the first wave of the same
  size. It fails at the first wave that is > 1.5× + 2 s slower to Ready,
  > 2× + 50 ms slower to answer, or that left anything (objects or
  CiliumEndpoints) behind (`src/trend.rs`).
- **vm-waves** (long): always a skip. network-operator's workload is pod
  networking; VM networking is stormvm's suite.

If network-operator is **not deployed** (no `networks.network.storm.io`
served), every test after `network-status` is a skip: the CNI there is not
this component's to test.

## What a run needs

- Everything it creates is in `STORM_NAMESPACE`, labelled
  `storm.io/test-run=<STORM_RUN_ID>` and `network-operator-test/workload`,
  and is deleted at the end of the suite (and by the runner with the
  namespace).
- **Cluster-scoped read**: `get`/`list` on `networks.network.storm.io`
  (every suite) and on `nodes` (`cross-node`, `capacity`). stormcentral's
  runner does not grant any yet
  ([stormcentral#55](https://github.com/glennswest/stormcentral/issues/55)).
  Until it does, those tests report `could not run` and the run exits
  **2**, never a pass. The pod tests still run. In long, the waves then
  use a fixed 20 pods.
- No privileges, no host access, no fixed node, device or core count.

### Environment (`src/env.rs`)

| variable | default | |
|---|---|---|
| `STORM_API` | — (**required**) | the apiserver URL |
| `STORM_NAMESPACE` | the ServiceAccount's `namespace` file | where everything the run creates goes (**required** if neither is set) |
| `STORM_RUN_ID` | — (**required**) | the `storm.io/test-run` label value |
| `STORM_SUITE` | `short` | used when no suite is given on the command line |
| `STORM_TIMEOUT` | 120 s short, 1800 s medium, 8 h long | seconds; waits are cut to what is left of it |
| `HOSTNAME` | — | this pod's name (set by the kubelet) |

The token and CA come from the mounted ServiceAccount
(`/var/run/secrets/kubernetes.io/serviceaccount`). `STORM_NODE` is not read:
everything goes through the API. A missing required variable is reported and
the run exits **2**.

## Output

One JSON line per test (`{"test", "status", "ms", "detail"}`; long's
`wave-N` lines add their numbers), then `{"summary": {...}}`; the same lines
go to `/results/results.jsonl` when `/results` exists. Exit `0` all passed,
`1` a test failed, `2` a test could not run.

## Building and running

stormcentral builds it on the build box: `test/build.sh` (static musl
binary into `test/.stage/test`), then
`podman build -f test/Containerfile <repo root>`. By hand, through sc-build:

```
sc-build 'cd test && cargo test --locked && cargo clippy --locked --all-targets -- -D warnings'
sc-build 'test/build.sh'
```

Run a suite on the test machines (as of 2026-09-27 no run has reached a
Job: C2NR0Q2's node registry refuses connections, stormcos#135 /
stormcentral#71; the runner's `@@RESULT` fix for stormcentral#56 is not yet
exercised):

```
stormcentral test run network-operator short --url http://stormcentral.g8.lo
```

`test/` is its own Cargo workspace with its own `Cargo.lock`. It depends on
the root crate by path, for the `Network` types and the operator's own
resolution rules.
