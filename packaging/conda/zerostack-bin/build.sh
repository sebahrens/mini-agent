#!/bin/bash
set -eux

install -Dm755 "${SRC_DIR}/mini-agent" "${PREFIX}/bin/mini-agent"
install -Dm644 "${SRC_DIR}/LICENSE" "${PREFIX}/share/licenses/${PKG_NAME}/LICENSE"
