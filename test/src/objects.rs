//! The objects the suites create, as pure functions: workload pods and
//! Deployments running this same image as `/test workload serve 8080`,
//! Services in front of them, and NetworkPolicies. All in the run's
//! namespace, all labelled `storm.io/test-run=<run>` and [`WORKLOAD_LABEL`].

use serde_json::{json, Value};

use crate::api::WORKLOAD_LABEL;

/// Which group of workloads an object belongs to (a Service's selector).
pub const APP_LABEL: &str = "network-operator-test/app";

/// The workload's port, and the Service port in front of it.
pub const POD_PORT: u16 = 8080;
pub const SVC_PORT: u16 = 80;

fn labels(run: &str, app: &str) -> Value {
    json!({"storm.io/test-run": run, WORKLOAD_LABEL: "true", APP_LABEL: app})
}

/// The pod spec every workload uses: this image serving its own name on
/// [`POD_PORT`], Ready once it answers. `node` pins it (the cross-node test).
pub fn pod_spec(image: &str, node: Option<&str>, restart: &str) -> Value {
    let mut spec = json!({
        "restartPolicy": restart,
        "terminationGracePeriodSeconds": 0,
        "automountServiceAccountToken": false,
        "containers": [{
            "name": "serve",
            "image": image,
            "imagePullPolicy": "IfNotPresent",
            "command": ["/test"],
            "args": ["workload", "serve", POD_PORT.to_string()],
            "ports": [{"containerPort": POD_PORT, "protocol": "TCP"}],
            "readinessProbe": {"tcpSocket": {"port": POD_PORT}, "periodSeconds": 2},
            "resources": {"requests": {"cpu": "5m", "memory": "8Mi"}, "limits": {"memory": "32Mi"}},
        }],
    });
    if let Some(n) = node {
        spec["nodeName"] = json!(n);
    }
    spec
}

pub fn pod(ns: &str, run: &str, name: &str, app: &str, image: &str, node: Option<&str>) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": ns, "labels": labels(run, app)},
        "spec": pod_spec(image, node, "Never"),
    })
}

pub fn deployment(ns: &str, run: &str, name: &str, app: &str, image: &str, replicas: u32) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": ns, "labels": labels(run, app)},
        "spec": {
            "replicas": replicas,
            "selector": {"matchLabels": {"storm.io/test-run": run, APP_LABEL: app}},
            "template": {
                "metadata": {"labels": labels(run, app)},
                "spec": pod_spec(image, None, "Always"),
            },
        },
    })
}

/// A Service in front of `app`: `ClusterIP` or `LoadBalancer`.
pub fn service(ns: &str, run: &str, name: &str, app: &str, kind: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": name, "namespace": ns, "labels": labels(run, app)},
        "spec": {
            "type": kind,
            "selector": {"storm.io/test-run": run, APP_LABEL: app},
            "ports": [{"name": "http", "port": SVC_PORT, "targetPort": POD_PORT, "protocol": "TCP"}],
        },
    })
}

/// Deny all ingress to `app`.
pub fn deny_ingress(ns: &str, run: &str, name: &str, app: &str) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {"name": name, "namespace": ns, "labels": labels(run, app)},
        "spec": {
            "podSelector": {"matchLabels": {APP_LABEL: app}},
            "policyTypes": ["Ingress"],
            "ingress": [],
        },
    })
}

/// Allow ingress to `app` on [`POD_PORT`] from pods carrying `from` (this
/// Job's pod carries `storm.io/component=network-operator`; workloads do not).
pub fn allow_from(ns: &str, run: &str, name: &str, app: &str, from: (&str, &str)) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {"name": name, "namespace": ns, "labels": labels(run, app)},
        "spec": {
            "podSelector": {"matchLabels": {APP_LABEL: app}},
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{"podSelector": {"matchLabels": {from.0: from.1}}}],
                "ports": [{"protocol": "TCP", "port": POD_PORT}],
            }],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workloads_run_this_image_serving_on_the_pod_port() {
        let p = pod("ns1", "r1", "nt-r1-a", "a", "test-network-operator-short:abc", Some("n2"));
        assert_eq!(p["metadata"]["namespace"], "ns1");
        assert_eq!(p["metadata"]["labels"]["storm.io/test-run"], "r1");
        assert_eq!(p["metadata"]["labels"][WORKLOAD_LABEL], "true");
        assert_eq!(p["spec"]["nodeName"], "n2");
        let c = &p["spec"]["containers"][0];
        assert_eq!(c["image"], "test-network-operator-short:abc");
        assert_eq!(c["command"][0], "/test");
        assert_eq!(c["args"], json!(["workload", "serve", "8080"]));
        assert_eq!(c["readinessProbe"]["tcpSocket"]["port"], 8080);
    }

    #[test]
    fn a_service_selects_exactly_its_app_in_this_run() {
        let s = service("ns1", "r1", "svc", "a", "ClusterIP");
        assert_eq!(s["spec"]["selector"], json!({"storm.io/test-run": "r1", APP_LABEL: "a"}));
        let d = deployment("ns1", "r1", "d", "a", "img", 3);
        // The Deployment's selector matches its own template, and the
        // Service's selector matches the same pods.
        for (k, v) in d["spec"]["selector"]["matchLabels"].as_object().unwrap() {
            assert_eq!(&d["spec"]["template"]["metadata"]["labels"][k], v);
            assert_eq!(&s["spec"]["selector"][k], v);
        }
        assert_eq!(d["spec"]["template"]["spec"]["restartPolicy"], "Always");
    }

    #[test]
    fn policies_select_the_app_and_only_ingress() {
        let d = deny_ingress("ns1", "r1", "deny", "a");
        assert_eq!(d["spec"]["ingress"], json!([]));
        let a = allow_from("ns1", "r1", "allow", "a", ("storm.io/component", "network-operator"));
        assert_eq!(a["spec"]["ingress"][0]["from"][0]["podSelector"]["matchLabels"]["storm.io/component"], "network-operator");
        assert_eq!(a["spec"]["podSelector"], d["spec"]["podSelector"]);
    }
}
