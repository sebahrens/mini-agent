#!/bin/bash
set -eux

cargo auditable install --locked --no-track --bins --root "${PREFIX}" --path .
cargo-bundle-licenses --format yaml --output ./THIRDPARTY.yml
install -Dm644 THIRDPARTY.yml "${PREFIX}/THIRDPARTY.yml"
# The same per-target inventory as the release archives, from the vendored
# crates. Source archives before 1.9.5 predate the generator.
if [[ -f scripts/third_party_licenses.py ]]; then
    python3 scripts/third_party_licenses.py generate \
        --target "${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}" \
        --output ./THIRD_PARTY_LICENSES
    install -Dm644 THIRD_PARTY_LICENSES "${PREFIX}/share/doc/${PKG_NAME}/THIRD_PARTY_LICENSES"
fi
install -Dm644 NOTICE "${PREFIX}/share/doc/${PKG_NAME}/NOTICE"
install -Dm644 SOURCE.md "${PREFIX}/share/doc/${PKG_NAME}/SOURCE.md"
