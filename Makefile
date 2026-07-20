REGISTRY ?= ghcr.io/glennswest
TAG ?= latest
IMAGE := $(REGISTRY)/network-operator:$(TAG)

.PHONY: build test clippy crds golden image deploy dry-run

build:
	cargo build --release

test:
	cargo test

clippy:
	cargo clippy --all-targets -- -D warnings

# Regenerate the CRD manifest from the Rust types. Never hand-edit the output.
crds:
	cargo run -q --bin crdgen > deploy/crds/network.storm.io_networks.yaml

# Accept a deliberate change to what gets installed. Review the diff first.
golden:
	UPDATE_GOLDEN=1 cargo test --test golden

image:
	docker build -t $(IMAGE) .

deploy:
	kubectl apply -f deploy/crds/
	kubectl apply -f deploy/operator.yaml

# Render a Network manifest without a cluster: make dry-run FILE=examples/network-bgp.yaml
FILE ?= examples/network-overlay.yaml
dry-run:
	cargo run -q --bin network-operator -- dry-run $(FILE)

# Build the .rpm and .deb. Linux only — run on the build host (dev.g8.lo).
packages:
	packaging/build-packages.sh
