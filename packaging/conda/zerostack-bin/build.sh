#!/bin/bash
set -eux

install -Dm755 "${SRC_DIR}/mini-agent" "${PREFIX}/bin/mini-agent"
install -Dm644 "${SRC_DIR}/LICENSE" "${PREFIX}/share/licenses/${PKG_NAME}/LICENSE"
install -Dm644 "${SRC_DIR}/NOTICE" "${PREFIX}/share/doc/${PKG_NAME}/NOTICE"
install -Dm644 "${SRC_DIR}/SOURCE.md" "${PREFIX}/share/doc/${PKG_NAME}/SOURCE.md"
