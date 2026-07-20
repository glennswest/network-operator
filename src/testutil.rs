//! Shared test fixtures. A resolved config per mode, built through the real
//! [`crate::modes::resolve`] so the fixtures can never drift from validation.

use crate::crd::{
    BgpPeer, BgpSpec, CiliumSpec, LoadBalancerSpec, Mode, NetworkSpec,
};
use crate::modes::{resolve, EffectiveConfig};

/// The example clusters in `examples/`, as a spec.
pub fn spec_for(mode: Mode) -> NetworkSpec {
    let load_balancer = match mode {
        Mode::Bgp => Some(LoadBalancerSpec {
            pools: Some(vec!["192.168.8.240/28".into()]),
            bgp: Some(BgpSpec {
                local_asn: Some(64512),
                peers: vec![BgpPeer { address: "192.168.8.1".into(), asn: 64512 }],
            }),
            ..Default::default()
        }),
        Mode::BareMetal => Some(LoadBalancerSpec {
            pools: Some(vec!["192.168.8.240/28".into()]),
            ..Default::default()
        }),
        _ => None,
    };

    NetworkSpec {
        mode,
        cluster_network: vec!["10.244.0.0/16".into()],
        service_network: vec!["10.96.0.0/12".into()],
        cilium: Some(CiliumSpec {
            k8s_service_host: Some("192.168.8.98".into()),
            k8s_service_port: Some(6443),
            load_balancer,
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub fn cfg_for(mode: Mode) -> EffectiveConfig {
    resolve(&spec_for(mode)).expect("test fixtures must be valid")
}
