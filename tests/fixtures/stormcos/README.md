# stormcos's Cilium manifests — the parity target for #9

Copied verbatim from glennswest/stormcos `deploy/manifests/` at 3dcf6d6
(2026-10-06): the files stormcos ships Cilium with, which stormcos-cilium
renders from the pinned 1.20.2 chart. `tests/parity.rs` checks that a
`Network` with Hubble on renders every object here, under the same names, and
that `cilium-config` agrees with `50-cilium-config.yaml` on every key both set.

Refresh by copying the same files again when stormcos-cilium moves its pin, and
add the new version's digests to `src/pins.rs` from its `pinned.txt`.
