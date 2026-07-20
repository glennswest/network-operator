%global debug_package %{nil}

Name:           network-operator
Version:        %{?_version}%{!?_version:0.1.0}
Release:        1%{?dist}
Summary:        Cluster Network Operator for the rustkube/stormcos stack

License:        Apache-2.0
URL:            https://github.com/glennswest/network-operator
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust

%description
network-operator owns the lifecycle of the cluster network (Cilium) — install,
configure, upgrade, reconcile and health-report — driven by a single
declarative Network custom resource. It is the rustkube/stormcos analog of
OpenShift's Cluster Network Operator.

The operator normally runs as a cluster workload from a container image. This
package exists for installers and air-gapped hosts that need the binary and the
deployment manifests on disk: there is no systemd unit, because the operator is
a Kubernetes Deployment, not a host daemon.

%prep
%autosetup

%build
cargo build --release --bin network-operator --bin crdgen

%install
install -Dpm0755 target/release/network-operator %{buildroot}%{_bindir}/network-operator
# crdgen is too generic a name for $PATH; it only ever emits this CRD.
install -Dpm0755 target/release/crdgen %{buildroot}%{_bindir}/network-operator-crdgen

install -Dpm0644 deploy/crds/network.storm.io_networks.yaml \
    %{buildroot}%{_datadir}/%{name}/deploy/crds/network.storm.io_networks.yaml
install -Dpm0644 deploy/operator.yaml \
    %{buildroot}%{_datadir}/%{name}/deploy/operator.yaml
for f in examples/*.yaml; do
    install -Dpm0644 "$f" %{buildroot}%{_datadir}/%{name}/"$f"
done

%files
%doc README.md
%{_bindir}/network-operator
%{_bindir}/network-operator-crdgen
%{_datadir}/%{name}/

%changelog
* Mon Jul 20 2026 Glenn West <glennswest@neuralcloudcomputing.com> - 0.1.0-1
- Initial package: Network CR reconciled into a Cilium install (P0-P2).
