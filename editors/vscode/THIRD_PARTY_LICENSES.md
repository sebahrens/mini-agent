# Third-party notices

The bundled extension JavaScript includes these direct runtime dependencies:

- `@agentclientprotocol/sdk` 1.3.0, Apache-2.0, source:
  <https://github.com/agentclientprotocol/typescript-sdk/tree/v1.3.0>
- `zod` 4.4.3, MIT, source: <https://github.com/colinhacks/zod/tree/v4.4.3>

This file covers the extension's JavaScript and the non-Cargo components of the
bundled native `mini-agent` executable. It is not the complete notice for that
executable: the executable is also built from several hundred Rust crates, and
their names, versions, license expressions, and verbatim license texts are in
`bin/<target>/THIRD_PARTY_LICENSES`, which ships next to the executable in this
extension and is the same inventory as in the matching release archive.

The bundled native `mini-agent` executable additionally embeds:

- QuickJS (the quickjs-ng engine vendored by the `rquickjs-sys` 0.12.2 crate),
  MIT, Copyright (c) 2017-2026 Fabrice Bellard, Copyright (c) 2017-2024 Charlie
  Gordon, Copyright (c) 2023-2026 Ben Noordhuis, Copyright (c) 2023-2026 Saúl
  Ibarra Corretgé; source: <https://github.com/DelSkayn/rquickjs/tree/v0.12.2>.
  Its license text is in `bin/<target>/THIRD_PARTY_LICENSES` under package
  `rquickjs-sys` (file `quickjs/LICENSE`), next to the `rquickjs` bindings' own
  MIT notice.
- `ajv` 8.12.0, MIT, source: <https://github.com/ajv-validator/ajv/tree/v8.12.0>
  (vendored at `src/extras/js/vendor/`, used for JSON Schema validation inside
  private JavaScript skill realms)

The license texts of the extension's JavaScript dependencies and of AJV follow.

## Apache License 2.0

The full Apache License 2.0 text is included as
`THIRD_PARTY_APACHE_LICENSE.txt` and is also available in the SDK source above.

## MIT License (Zod)

Copyright (c) 2020 Colin McDonnell

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## MIT License (AJV)

Copyright (c) 2015-2021 Evgeny Poberezkin

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
