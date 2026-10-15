# Static musl Git payload

The two musl release targets use native C compilers inside an explicit Alpine
3.24.1 image, selected as `linux/amd64` or `linux/arm64`. The image index, curl
8.23.0 source hash, Git source hash and Git policy patch are pinned. ARM64 builds
on an x86-64 runner require the release workflow's pinned QEMU setup step.

Build the payload from the repository root:

```sh
python3 scripts/build-git-runtime-musl.py \
  --target aarch64-unknown-linux-musl --output target/native-git-bundle
MINI_AGENT_GIT_RUNTIME_BUNDLE=target/native-git-bundle \
  cargo install --path . --debug
```

The wrapper provisions its C toolchain image, then runs the source build offline
with a read-only source tree, explicit writable cache/output mounts, the caller's
UID/GID, no capabilities and no privilege escalation. Each checkout keeps its
own cache. A failed or cancelled client retires only its uniquely named build
container. Existing output directories are preserved and cause an error.

The TLS backend is Mbed TLS under its upstream GPL-2.0-or-later licensing option;
curl is built from its pinned source. Distribution packaging requires
verbatim dependency notices and corresponding source records. The native
bootstrap requires this selected backend for musl. It links the full static
libcurl closure and the upstream musl regex implementation. Git's `error()`
implementation receives a private symbol name to keep its variadic ABI separate
from GNU dependency libraries' unrelated `error(3)` implementation.

Static ELF validation checks the actual architecture, rejects an interpreter
and external dynamic dependencies, and permits self-relocation metadata in a
static PIE. The build cache includes the compiler, curl version/TLS backend and
actual bytes of static library inputs, including libc and compiler runtimes.
An unchanged version label cannot reuse bytes built before a library update.
The compiled application independently validates and embeds every payload byte.

The cross compiler container receives `MINI_AGENT_GIT_RUNTIME_BUNDLE` through
`Cross.toml`. Release workflows use a checkout-relative bundle path so the same
input is available inside that container and on the publishing host. Full and
lite application builds share the same target-specific payload.
