# Vendored AJV

`ajv.min.js` is AJV 8.12.0, MIT licensed. The npm release does not contain the browser bundle
named in the original tracker task, so this artifact was generated from the release's
`dist/ajv.js` using AJV's own v8.12.0 browserify/terser recipe:

```text
npm pack ajv@8.12.0
npm install ajv@8.12.0 browserify@17.0.0 terser@5.16.1
browserify node_modules/ajv/dist/ajv.js --standalone ajv7 --outfile ajv.bundle.js
terser ajv.bundle.js --ecma 2018 \
  --compress pure_getters=true,keep_infinity=true,unsafe_methods=true \
  --format 'preamble="/* ajv 8.12.0 (ajv7): Another JSON Schema Validator */"' \
  --output ajv.min.js
```

- npm tarball SHA-256: `00c7dc15d8db03adf835bdf045442ef3f39d6eb3b088112196290afcfed86a28`
- vendored bundle SHA-256: `a1347acbed2ea06d32ed94414b409f01dda8ca12ce112ab7cc950930af3baa10`
- upstream license: `AJV-LICENSE.txt`

The Rust loader wraps this unmodified UMD artifact in a private lexical scope; do not add the
bundle as a model-authored global or introduce `require()`/`import()` support.
