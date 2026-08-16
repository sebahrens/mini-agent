# Vendored AJV

`ajv.min.js` is AJV 8.12.0, MIT licensed. The npm release does not contain the browser bundle
named in the original tracker task, so this artifact comes from the AJV maintainers' official
[`ajv-dist` v8.12.0 browser-bundle tag](https://github.com/ajv-validator/ajv-dist/blob/v8.12.0/dist/ajv7.min.js).
The vendored copy adds only a conventional final newline.

- npm tarball SHA-256: `00c7dc15d8db03adf835bdf045442ef3f39d6eb3b088112196290afcfed86a28`
- upstream Git blob SHA-1: `82b6aeee559dfc393c6c10213afb1d1065808428`
- vendored bundle SHA-256: `2866583ce03b97b6a6c04ffae0cc5399cf54444cc5e2b098449e7a85b372afa1`
- upstream license: `AJV-LICENSE.txt`

The Rust loader wraps this UMD artifact in a trusted lexical scope before hardening and exposes only
a frozen string-bridged facade to stored skill source; do not add the bundle as a model-authored
global or introduce `require()`/`import()` support.
