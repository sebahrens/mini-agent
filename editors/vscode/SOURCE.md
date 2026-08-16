# Corresponding source

This platform-specific VSIX contains the GPL-3.0-only `mini-agent` executable.
The complete corresponding source for version 1.7.2 is the `v1.7.2` tree at
<https://github.com/sebahrens/mini-agent/tree/v1.7.2>. Release candidates also
ship `mini-agent-v1.7.2-source.tar.gz` beside the VSIX on the GitHub release.

Build instructions and the exact Rust toolchain are recorded in
`.github/workflows/release.yml`; VSIX assembly is performed by
`editors/vscode/scripts/package-target.mjs` and the same release workflow.
