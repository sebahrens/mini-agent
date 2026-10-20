# Google Cloud Auth 1.17.0 host patch

Official crate: https://crates.io/crates/google-cloud-auth/1.17.0
Repository: https://github.com/googleapis/google-cloud-rust
Original crate checksum: ac9870b8ea484519aa8601e14886f2efc148ea490034e43d04bef1d246170cc9
Original source revision: 373bb9cc698f45e1f63e41beef090924af2ac2a4

Copyright Google LLC; upstream Apache-2.0 file headers and package metadata are retained.

The credential source selection, OAuth endpoints, token conversion and refresh policy are upstream.
The host amendment races the complete token-cache worker against its last receiver closing and
caps each token-provider future at 30 seconds. Refresh remains client-owned; cancelling a model
turn only cancels its token waiter. Dropping the final credential client cancels held fetches and
sleeps. Optional executable ADC configurations, including nested impersonation sources, are
rejected before cache construction. The public build marker prevents accidental registry fallback.
No credential HTTP byte-cap guarantee is claimed.

Root Cargo.lock pins the path package and its registry dependencies. The production package now
includes this source tree/build script and license attribution; builds need it alongside rig-core.
Owning local controls in src/provider_vertex_adc_tests.rs verify held-fetch last-client drop, controlled
30-second timeout, ADC refresh and wire behavior. See docs/agent/PROVIDERS.md for product scope.

TLS amendment (mini-agent-vb5vw.56): the reqwest dependency drops the `rustls-no-provider` feature.
With native-tls enabled in the root crate, reqwest always selects native-tls for default clients and
this crate never calls `use_rustls_tls()`, so the rustls client layer (hyper-rustls, tokio-rustls,
rustls-platform-verifier) was linked unused. The `rustls` crate dependency stays for RSA JWT signing.
