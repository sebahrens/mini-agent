#!/bin/bash
set -eux

install -Dm755 "${SRC_DIR}/mini-agent" "${PREFIX}/bin/mini-agent"
install -Dm644 "${SRC_DIR}/LICENSE" "${PREFIX}/share/licenses/${PKG_NAME}/LICENSE"
# Archives before 1.9.5 predate the third-party licence inventory.
if [[ -f "${SRC_DIR}/THIRD_PARTY_LICENSES" ]]; then
    install -Dm644 "${SRC_DIR}/THIRD_PARTY_LICENSES" "${PREFIX}/share/licenses/${PKG_NAME}/THIRD_PARTY_LICENSES"
fi
install -Dm644 "${SRC_DIR}/NOTICE" "${PREFIX}/share/doc/${PKG_NAME}/NOTICE"
install -Dm644 "${SRC_DIR}/SOURCE.md" "${PREFIX}/share/doc/${PKG_NAME}/SOURCE.md"
