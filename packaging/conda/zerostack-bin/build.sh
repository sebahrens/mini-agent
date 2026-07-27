#!/bin/bash
set -eux

BIN=$(find "${SRC_DIR}" -maxdepth 1 -type f -name "zerostack-*" ! -name "*.tar.gz" | head -1)
if [ -z "$BIN" ]; then
    BIN="${SRC_DIR}/zerostack"
fi

install -Dm755 "${BIN}" "${PREFIX}/bin/zerostack"
install -Dm644 "${SRC_DIR}/LICENSE" "${PREFIX}/share/licenses/${PKG_NAME}/LICENSE"
