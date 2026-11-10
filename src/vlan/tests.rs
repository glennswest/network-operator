use super::*;
use crd::{Addresses, Route};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

fn vlan(name: &str, id: u16, namespaces: &[&str]) -> Vlan {
    Vlan {
        metadata: ObjectMeta { name: Some(name.into()), ..Default::default() },
        spec: VlanSpec {
            id,
            namespaces: namespaces.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
        status: None,
    }
}

fn world(vlans: Vec<Vlan>, namespaces: &[&str]) -> World {
    World {
        vlans,
        namespaces: namespaces.iter().map(|n| (n.to_string(), None)).collect(),
        ..Default::default()
    }
}

fn applied(p: &Plan) -> Vec<String> {
    p.apply.iter().map(|r| r.id()).collect()
}

#[test]
fn a_vlan_renders_a_nad_in_each_allowed_namespace_only() {
    let p = plan(&world(vec![vlan("vlan10", 10, &["a", "b"])], &["a", "b", "c"]));
    assert_eq!(
        applied(&p),
        vec!["NetworkAttachmentDefinition/a/vlan10", "NetworkAttachmentDefinition/b/vlan10"]
    );
    let r = &p.reports["vlan10"];
    assert_eq!(r.nads, vec!["a/vlan10", "b/vlan10"]);
    assert!(r.problems.is_empty());
}

#[test]
fn the_nad_config_tags_the_port_and_drops_the_default_vlan() {
    let v = vlan("vlan10", 10, &["a"]);
    let c: Value = serde_json::from_str(&nad_config("vlan10", &v.spec)).unwrap();
    assert_eq!(c["type"], "bridge");
    assert_eq!(c["bridge"], DEFAULT_BRIDGE);
    assert_eq!(c["vlan"], 10);
    assert_eq!(c["name"], "vlan10");
    // Without this the pod's port stays on VLAN 1, the node's own LAN.
    assert_eq!(c["preserveDefaultVlan"], false);
    assert_eq!(c["isGateway"], false);
    assert!(c.get("ipam").is_none(), "address type none is L2 only");
    assert!(c.get("mtu").is_none());
}

#[test]
fn address_types_render_their_ipam() {
    let mut v = vlan("v", 20, &["a"]);
    v.spec.mtu = Some(9000);
    v.spec.addresses = Some(Addresses { kind: AddressType::Dhcp, routes: vec![] });
    let c: Value = serde_json::from_str(&nad_config("v", &v.spec)).unwrap();
    assert_eq!(c["ipam"], json!({"type": "dhcp"}));
    assert_eq!(c["mtu"], 9000);
    assert!(c.get("capabilities").is_none());

    v.spec.addresses = Some(Addresses {
        kind: AddressType::Static,
        routes: vec![Route { dst: "10.0.0.0/8".into(), gw: Some("10.20.0.1".into()) }],
    });
    let c: Value = serde_json::from_str(&nad_config("v", &v.spec)).unwrap();
    assert_eq!(c["ipam"], json!({"type": "static", "routes": [{"dst": "10.0.0.0/8", "gw": "10.20.0.1"}]}));
    assert_eq!(c["capabilities"], json!({"ips": true}), "the pod's ips reach the plugin");
}

/// A change to a valid spec.
type Edit = Box<dyn Fn(&mut VlanSpec)>;

#[test]
fn invalid_specs_are_rejected_with_a_reason() {
    let cases: Vec<(Edit, &str)> = vec![
        (Box::new(|s| s.id = 0), "outside 1-4094"),
        (Box::new(|s| s.id = 4095), "outside 1-4094"),
        (Box::new(|s| s.bridge = Some("a-very-long-bridge".into())), "interface name"),
        (Box::new(|s| s.uplink = Some("eth0/x".into())), "interface name"),
        (Box::new(|s| s.mtu = Some(100)), "mtu"),
        (Box::new(|s| s.namespaces = vec!["Bad_NS".into()]), "namespace"),
        (
            Box::new(|s| {
                s.addresses = Some(Addresses {
                    kind: AddressType::Dhcp,
                    routes: vec![Route { dst: "10.0.0.0/8".into(), gw: None }],
                })
            }),
            "static",
        ),
        (
            Box::new(|s| {
                s.addresses = Some(Addresses {
                    kind: AddressType::Static,
                    routes: vec![Route { dst: "nope".into(), gw: None }],
                })
            }),
            "CIDR",
        ),
    ];
    for (edit, want) in cases {
        let mut v = vlan("v", 10, &["a"]);
        edit(&mut v.spec);
        let p = plan(&world(vec![v], &["a"]));
        assert!(p.apply.is_empty(), "an invalid VLAN renders nothing ({want})");
        let why = p.reports["v"].problems.join(";");
        assert!(why.contains(want), "{why:?} should mention {want:?}");
    }
    assert!(validate(&vlan("v", 1, &[]).spec).is_ok());
    assert!(validate(&vlan("v", 4094, &[]).spec).is_ok());
}

#[test]
fn one_id_per_bridge_and_the_older_vlan_keeps_it() {
    let mut old = vlan("zz-old", 10, &["a"]);
    old.metadata.creation_timestamp = Some(Time(chrono::DateTime::from_timestamp(100, 0).unwrap()));
    let mut new = vlan("aa-new", 10, &["a"]);
    new.metadata.creation_timestamp = Some(Time(chrono::DateTime::from_timestamp(200, 0).unwrap()));
    let mut other_bridge = vlan("other", 10, &["a"]);
    other_bridge.spec.bridge = Some("br1".into());

    let (ok, rejected) = accept(&[new.clone(), old.clone(), other_bridge.clone()]);
    let names: Vec<&str> = ok.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["other", "zz-old"]);
    assert!(rejected["aa-new"].contains("already VLAN zz-old"));
}

#[test]
fn a_deleted_or_rejected_vlans_nads_are_reaped_and_foreign_nads_are_left() {
    let mut w = world(vec![vlan("keep", 10, &["a"])], &["a", "b"]);
    w.nads = vec![
        ExistingNad { namespace: "a".into(), name: "keep".into(), vlan: Some("keep".into()), config: Some(nad_config("keep", &vlan("keep", 10, &["a"]).spec)) },
        // The VLAN is gone.
        ExistingNad { namespace: "a".into(), name: "gone".into(), vlan: Some("gone".into()), config: None },
        // The namespace is no longer allowed.
        ExistingNad { namespace: "b".into(), name: "keep".into(), vlan: Some("keep".into()), config: None },
        // Someone else's.
        ExistingNad { namespace: "b".into(), name: "theirs".into(), vlan: None, config: None },
    ];
    let p = plan(&w);
    assert!(p.apply.is_empty(), "an up-to-date NAD is not re-applied");
    assert_eq!(p.delete, vec![("a".to_string(), "gone".to_string()), ("b".to_string(), "keep".to_string())]);
}

#[test]
fn a_drifted_nad_is_put_back() {
    let mut w = world(vec![vlan("v", 10, &["a"])], &["a"]);
    w.nads = vec![ExistingNad { namespace: "a".into(), name: "v".into(), vlan: Some("v".into()), config: Some("{}".into()) }];
    assert_eq!(applied(&plan(&w)), vec!["NetworkAttachmentDefinition/a/v"]);
}

#[test]
fn a_nad_that_is_not_ours_is_never_overwritten() {
    let mut w = world(vec![vlan("v", 10, &["a"])], &["a"]);
    w.nads = vec![ExistingNad { namespace: "a".into(), name: "v".into(), vlan: None, config: Some("{}".into()) }];
    let p = plan(&w);
    assert!(p.apply.is_empty() && p.delete.is_empty());
    assert!(p.reports["v"].problems[0].contains("not overwriting"));
}

#[test]
fn a_missing_namespace_gets_its_nad_when_it_appears() {
    let p = plan(&world(vec![vlan("v", 10, &["a", "later"])], &["a"]));
    assert_eq!(applied(&p), vec!["NetworkAttachmentDefinition/a/v"]);
    assert_eq!(p.reports["v"].missing_namespaces, vec!["later"]);
}

#[test]
fn namespace_defaults_are_written_removed_and_never_doubled() {
    let mut a = vlan("a10", 10, &[]);
    a.spec.default_for = vec!["web".into()];
    let mut b = vlan("b20", 20, &[]);
    b.spec.default_for = vec!["web".into(), "db".into()];
    let mut w = world(vec![a, b], &["web", "db", "old"]);
    w.namespaces.insert("old".into(), Some("gone".into()));
    let p = plan(&w);
    assert_eq!(
        p.annotate,
        vec![
            ("db".to_string(), Some("b20".to_string())),
            ("old".to_string(), None),
            ("web".to_string(), Some("a10".to_string())),
        ]
    );
    assert!(p.reports["b20"].problems[0].contains("web's default is already VLAN a10"));
    // defaultFor implies allowed: the NAD is there for the default to name.
    assert!(applied(&p).contains(&"NetworkAttachmentDefinition/web/a10".to_string()));
}

#[test]
fn status_reports_nodes_pending_nodes_and_pods() {
    let mut w = world(vec![vlan("v10", 10, &["a"]), vlan("v20", 20, &["a"])], &["a"]);
    let state = |present: Vec<u16>| {
        Some(NodeState { bridge: DEFAULT_BRIDGE.into(), vlan_filtering: true, present, ..Default::default() })
    };
    w.nodes.insert("n1".into(), state(vec![10, 20]));
    let mut n2 = state(vec![10]).unwrap();
    n2.problems.insert(20, "no uplink port on bridge stormbr0".into());
    w.nodes.insert("n2".into(), Some(n2));
    w.nodes.insert("n3".into(), None);
    w.pods.insert("a/p1".into(), vec!["a/v10".into()]);
    w.pods.insert("a/p2".into(), vec!["a/v20".into(), "a/v10".into()]);
    w.pods.insert("b/p3".into(), vec!["b/v10".into()]);
    let p = plan(&w);

    let r10 = &p.reports["v10"];
    assert_eq!(r10.nodes, vec!["n1", "n2"]);
    assert!(r10.pending_nodes.is_empty() && r10.waiting.is_empty());
    assert_eq!(r10.pods, vec!["a/p1", "a/p2"], "b/v10 is not one of its NADs");

    let r20 = &p.reports["v20"];
    assert_eq!(r20.nodes, vec!["n1"]);
    assert_eq!(r20.pending_nodes, vec!["n2: no uplink port on bridge stormbr0"]);
    assert_eq!(r20.pod_count, 1);

    let now = Time(chrono::DateTime::from_timestamp(1000, 0).unwrap());
    let s10 = status(&VlanStatus::default(), r10, 3, &now);
    assert_eq!(s10.conditions[0].type_, "Ready");
    assert_eq!(s10.conditions[0].status, "True");
    assert_eq!(s10.node_count, 2);
    let s20 = status(&VlanStatus::default(), r20, 3, &now);
    assert_eq!((s20.conditions[0].status.as_str(), s20.conditions[0].reason.as_str()), ("False", "Waiting"));
    assert_eq!(s20.conditions[1].status, "False", "waiting is not degraded");
}

#[test]
fn no_agent_means_not_ready() {
    let p = plan(&world(vec![vlan("v", 10, &["a"])], &["a"]));
    assert_eq!(p.reports["v"].waiting, vec!["no node agent reports this bridge"]);
}

#[test]
fn unchanged_conditions_keep_their_transition_time() {
    let r = Report { nodes: vec!["n".into()], ..Default::default() };
    let t1 = Time(chrono::DateTime::from_timestamp(1000, 0).unwrap());
    let t2 = Time(chrono::DateTime::from_timestamp(2000, 0).unwrap());
    let first = status(&VlanStatus::default(), &r, 1, &t1);
    let second = status(&first, &r, 1, &t2);
    assert_eq!(first, second, "a pass that changes nothing writes nothing new");
    let broken = Report { problems: vec!["x".into()], ..r };
    let third = status(&second, &broken, 2, &t2);
    assert_eq!(third.conditions[0].last_transition_time, t2);
}

#[test]
fn network_status_names_resolve_to_namespaced_nads() {
    let s = r#"[{"name":"cilium","default":true},{"name":"a/v10","interface":"net1"},{"name":"v20"}]"#;
    assert_eq!(attached_nads("a", s), vec!["a/cilium", "a/v10", "a/v20"]);
    assert!(attached_nads("a", "garbage").is_empty());
}

#[test]
fn node_state_round_trips_as_the_annotation() {
    let mut s = NodeState { bridge: "stormbr0".into(), vlan_filtering: true, present: vec![10], ..Default::default() };
    s.owned.insert("eth0".into(), vec![10]);
    let text = serde_json::to_string(&s).unwrap();
    assert!(text.contains("\"vlanFiltering\":true"));
    assert_eq!(serde_json::from_str::<NodeState>(&text).unwrap(), s);
}

#[test]
fn a_hundred_vlans_plan_a_hundred_nads() {
    let vlans: Vec<Vlan> = (1..=100).map(|i| vlan(&format!("vlan{i}"), 100 + i, &["a"])).collect();
    let p = plan(&world(vlans, &["a"]));
    assert_eq!(p.apply.len(), 100);
    assert!(p.reports.values().all(|r| r.problems.is_empty()));
}
