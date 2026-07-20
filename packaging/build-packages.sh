#!/usr/bin/env bash
# Build the RPM and DEB. Linux only — run it on the build host (dev.g8.lo),
# never on a dev laptop, or you package a binary for the wrong OS.
#
#   packaging/build-packages.sh [version]
#
# Artifacts land in dist/.
set -euo pipefail

cd "$(dirname "$0")/.."
REPO_ROOT="$PWD"
VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
DIST="$REPO_ROOT/dist"

case "$(uname -s)" in
  Linux) ;;
  *) echo "error: must run on Linux — packaging a $(uname -s) binary as .rpm/.deb is useless" >&2; exit 1 ;;
esac

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)  DEB_ARCH=amd64 ;;
  aarch64) DEB_ARCH=arm64 ;;
  *) echo "error: unsupported arch $ARCH" >&2; exit 1 ;;
esac

echo "==> building network-operator $VERSION for $ARCH"
rm -rf "$DIST"
mkdir -p "$DIST"

cargo build --release --bin network-operator --bin crdgen

# --- RPM -------------------------------------------------------------------
# Source from git so the package is built from committed state, not from
# whatever is dirty in the working tree.
echo "==> rpm"
RPMTOP="$(mktemp -d)"
trap 'rm -rf "$RPMTOP"' EXIT
mkdir -p "$RPMTOP"/{SOURCES,SPECS,BUILD,RPMS,SRPMS}

git archive --format=tar.gz \
    --prefix="network-operator-$VERSION/" \
    -o "$RPMTOP/SOURCES/network-operator-$VERSION.tar.gz" HEAD

rpmbuild -bb packaging/network-operator.spec \
    --define "_topdir $RPMTOP" \
    --define "_version $VERSION"

find "$RPMTOP/RPMS" -name '*.rpm' -exec cp {} "$DIST/" \;

# --- DEB -------------------------------------------------------------------
# Staged by hand rather than with cargo-deb, so the layout matches the spec
# above exactly and the build host needs nothing beyond dpkg-deb.
echo "==> deb"
STAGE="$(mktemp -d)"
PKG="$STAGE/network-operator_${VERSION}_${DEB_ARCH}"

install -Dpm0755 target/release/network-operator "$PKG/usr/bin/network-operator"
install -Dpm0755 target/release/crdgen "$PKG/usr/bin/network-operator-crdgen"
install -Dpm0644 deploy/crds/network.storm.io_networks.yaml \
    "$PKG/usr/share/network-operator/deploy/crds/network.storm.io_networks.yaml"
install -Dpm0644 deploy/operator.yaml "$PKG/usr/share/network-operator/deploy/operator.yaml"
for f in examples/*.yaml; do
    install -Dpm0644 "$f" "$PKG/usr/share/network-operator/$f"
done
install -Dpm0644 README.md "$PKG/usr/share/doc/network-operator/README.md"
install -Dpm0644 LICENSE "$PKG/usr/share/doc/network-operator/LICENSE"

mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: network-operator
Version: $VERSION
Section: admin
Priority: optional
Architecture: $DEB_ARCH
Maintainer: Glenn West <glennswest@neuralcloudcomputing.com>
Homepage: https://github.com/glennswest/network-operator
Description: Cluster Network Operator for the rustkube/stormcos stack
 network-operator owns the lifecycle of the cluster network (Cilium) —
 install, configure, upgrade, reconcile and health-report — driven by a
 single declarative Network custom resource. It is the rustkube/stormcos
 analog of OpenShift's Cluster Network Operator.
 .
 The operator normally runs as a cluster workload from a container image.
 This package exists for installers and air-gapped hosts that need the
 binary and the deployment manifests on disk: there is no systemd unit,
 because the operator is a Kubernetes Deployment, not a host daemon.
EOF

dpkg-deb --build --root-owner-group "$PKG" >/dev/null
cp "$STAGE"/*.deb "$DIST/"
rm -rf "$STAGE"

echo
echo "==> artifacts"
ls -la "$DIST"
