#!/bin/bash
set -eux

cargo auditable install --locked --no-track --bins --root "${PREFIX}" --path .
cargo-bundle-licenses --format yaml --output ./THIRDPARTY.yml
install -Dm644 THIRDPARTY.yml "${PREFIX}/THIRDPARTY.yml"
install -Dm644 NOTICE "${PREFIX}/share/doc/${PKG_NAME}/NOTICE"
install -Dm644 SOURCE.md "${PREFIX}/share/doc/${PKG_NAME}/SOURCE.md"
