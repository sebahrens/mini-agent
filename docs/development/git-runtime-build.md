# Guarded Git runtime build inputs

The Git hardening work embeds a publisher-built runtime in the application;
installers and `cargo install` continue to carry one application executable.
`RuntimeApproval` contains the target, policy, file bytes and expected SHA-256
digests. A runtime-side JSON file cannot supply approval.

`scripts/prepare-git-runtime.py` inventories explicitly supplied build outputs
against the pinned Git source archive and repository policy patch. It copies
the Git program, HTTP and HTTPS helpers, and explicitly listed libraries into
fresh regular files. It does not build missing targets or attest that a supplied
binary was compiled from those sources: that is the trusted build pipeline's
responsibility. The seven supported release targets retain their existing scope.

The resulting `bundle.manifest` is a bounded UTF-8 inventory with fixed version,
target, source hash, policy-patch hash and policy headers, followed by ordered
`file=<leaf-name><TAB><SHA-256><TAB><x-or-r>` entries. Names cannot traverse paths,
collide by case, or use Windows device names. Required executable names acquire
`.exe` on Windows. Limits are 128 files, 64 MiB per file and 256 MiB total.

The build script reads `MINI_AGENT_GIT_RUNTIME_BUNDLE`, validates the target and
repository policy, and captures the actual checked payload into Cargo's output
directory. Generated Rust embeds those captured files rather than rereading the
input paths during compilation. The bundle identity also enters the existing
exact-build fingerprint. Input files and the environment selector are explicit
Cargo rebuild dependencies. An invalid explicit bundle fails the build.

Without an explicit bundle, the build script invokes the pinned-source native
bootstrap using Python 3. `MINI_AGENT_GIT_PYTHON` can select the interpreter.
The archive, policy, bootstrap and inventory producer bytes identify a separate
output directory. A nonbuilding preflight also includes the current compiler,
selected TLS/backend settings and actual static archive bytes in that identity;
those input paths become Cargo rebuild dependencies. An existing output is fully
revalidated and never repaired.
The Python bootstrap keeps its C build cache within this checkout's `target/`
directory, so feature rows can reuse the same validated Git payload. Default
builds embed the runtime rather than producing absent approval or falling back
to system Git. See [native build prerequisites](git-runtime-bootstrap.md).

Cross-target and Windows builds supply a matching bundle from their explicit
platform build step. HTTPS libraries must be included or supplied by the
supported target's trusted system loader; a GNU helper is not a substitute for
a static musl helper. Native platform execution remains required evidence for
each supported deployment row.
