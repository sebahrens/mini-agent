# Corresponding source

This platform-specific VSIX contains the GPL-3.0-only `mini-agent` executable.
The complete corresponding source for version 1.9.4 is the `v1.9.4` tree at
<https://github.com/sebahrens/mini-agent/tree/v1.9.4>. Release candidates also
ship `mini-agent-v1.9.4-source.tar.gz` beside the VSIX on the GitHub release.

Build instructions and the exact Rust toolchain are recorded in
`.github/workflows/release.yml`; VSIX assembly is performed by
`editors/vscode/scripts/package-target.mjs` and the same release workflow.

## npm dependencies bundled into `dist/extension.js`

`dist/extension.js` is an esbuild bundle that includes the extension's npm
runtime dependencies. The source archive therefore also carries, under
`vendor-npm/`, the registry tarball of every package pinned by
`editors/vscode/package-lock.json` (runtime dependencies and the build and
test tools, including each platform's esbuild binary package).
`vendor-npm/npm-sources.json` lists each tarball with its lockfile path,
version, resolved URL and the lockfile's `integrity` hash; the release
packager verifies every tarball against that hash and rebuilds the bundle
offline from them before publishing the archive.

To rebuild `dist/extension.js` without network access, install the Node
version in `editors/vscode/.nvmrc` and the npm named by `packageManager` in
`editors/vscode/package.json`, then from the extracted archive root run:

```bash
npm cache add --cache "$PWD/.npm-offline-cache" vendor-npm/*.tgz
cd editors/vscode
npm ci --offline --ignore-scripts --cache "$PWD/../../.npm-offline-cache"
npm run build
```

npm verifies each installed package against the `integrity` hash in
`package-lock.json`.
