# Cross-Platform Paths and Persistent Storage

**Status**: Pre-implementation
**Scope**: Normative foundation for every phase and every feature that persists files
**Target platforms**: Linux, macOS, and Windows MSVC

---

## Goal and authority

Every persistent artifact must resolve through one typed application-path service. Feature modules
must not call `dirs::*`, read path environment variables, or fall back to the current directory on
their own. This specification defines the target contract; current path behavior is not evidence
that a conflicting location is correct.

The implementation uses the platform conventions exposed by `dirs` 6.0.0: XDG base directories
on Linux, Application Support/Caches on macOS, and Known Folders on Windows. This specification
makes the application-level decisions that `dirs` deliberately leaves to callers, especially the
Windows Roaming-versus-Local split.

---

## Typed resolver

One immutable value is constructed during startup and passed to all storage owners:

```rust
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub local_data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub credentials_dir: PathBuf,
    pub project_dir: Option<PathBuf>,
}
```

The production constructor reads a `PathEnvironment` abstraction containing platform base
directories and relevant environment variables. Tests construct that abstraction directly; they
do not mutate process-global environment variables in parallel or infer expected paths from the
host running the test.

`project_dir` is `<workspace-root>/.zerostack`, not an operating-system user directory. The
workspace root is the same root used by the session permission system. Merely changing the process
current directory must not redirect user-global state after startup.

### Override precedence

| Root | Override | Default source |
|------|----------|----------------|
| Configuration | `ZS_CONFIG_DIR` | platform config directory |
| Portable durable data | `ZS_DATA_DIR` | platform data directory |
| Machine-local durable data | `ZS_LOCAL_DATA_DIR`, then `ZS_DATA_DIR` for backward compatibility | platform local-data directory |
| Runtime state | `ZS_STATE_DIR`, then `ZS_LOCAL_DATA_DIR`, then `ZS_DATA_DIR` | platform state/local-data directory |
| Rebuildable cache | `ZS_CACHE_DIR` | platform cache directory |
| Credentials | `ZS_CREDENTIALS_DIR` | `<local_data_dir>/credentials` |

Every override expands a leading `~` using the same helper and is then required to be absolute.
An unset override falls through to the next source in the table. A set-but-empty value, a missing
home directory needed for `~`, a relative path, or a required base-directory lookup failure returns
a typed startup error. Durable or sensitive state never falls back to `.` or another
current-working-directory-relative path.

`ZS_DATA_DIR` continues to redirect both portable and local data unless the more specific local
override is present. When `ZS_STATE_DIR` is absent, an explicit `ZS_LOCAL_DATA_DIR` or
`ZS_DATA_DIR` value is also the state root without an added `state` component, preserving existing
hermetic/test deployments. `ZS_DATA_DIR` no longer selects the config root: callers that need one
hermetic root set both variables. A config found under the old data-root behavior is a legacy
migration candidate. This intentional split prevents a fresh config and an existing config from
using different storage classes.

---

## Platform mapping

All rows append the application component `zerostack` to the operating-system base directory.

| Root | Linux | macOS | Windows |
|------|-------|-------|---------|
| `config_dir` | `$XDG_CONFIG_HOME/zerostack`, default `~/.config/zerostack` | `~/Library/Application Support/zerostack` | `%APPDATA%\zerostack` (Roaming) |
| `data_dir` | `$XDG_DATA_HOME/zerostack`, default `~/.local/share/zerostack` | `~/Library/Application Support/zerostack` | `%APPDATA%\zerostack` (Roaming) |
| `local_data_dir` | `$XDG_DATA_HOME/zerostack`, default `~/.local/share/zerostack` | `~/Library/Application Support/zerostack` | `%LOCALAPPDATA%\zerostack` |
| `state_dir` | `$XDG_STATE_HOME/zerostack`, default `~/.local/state/zerostack` | `~/Library/Application Support/zerostack/state` | `%LOCALAPPDATA%\zerostack\state` |
| `cache_dir` | `$XDG_CACHE_HOME/zerostack`, default `~/.cache/zerostack` | `~/Library/Caches/zerostack` | `%LOCALAPPDATA%\zerostack\cache` |
| `credentials_dir` | `<local_data_dir>/credentials` | `<local_data_dir>/credentials` | `%LOCALAPPDATA%\zerostack\credentials` |

Configuration and data base directories intentionally coincide on macOS and for Windows Roaming
data. Artifact-specific child directories still keep concerns separate. Linux must not place a
new config file under the data root merely because no config file exists yet.

---

## Artifact ownership

| Artifact | Canonical root | Reason |
|----------|----------------|--------|
| `config.toml`/YAML/JSON, `SUFFIX.md`, global `AGENTS.md`, global hook `settings.json` | `config_dir` | User-authored configuration |
| Project config, project prompts, project Agent Skills | `project_dir` | Repository-scoped, reviewable configuration |
| Global prompts, themes, docs, imported portable Agent Skill trees | `data_dir` | Durable user content that may roam |
| Learned JS `skills.db`, embeddings, held-out suites, lifecycle/evidence DB | `local_data_dir/skills` | SQLite and mutable indexes are machine-local and unsafe to roam concurrently |
| Sessions, transcripts, tool output, loop state, turn telemetry, crash state, logs | `state_dir` | Durable operational state, not configuration or skill evidence |
| Embedding model downloads, query cache, rebuildable dense snapshots, import staging | `cache_dir` | Safe to delete and reconstruct |
| MCP OAuth refresh/access tokens and future secret material | `credentials_dir` | Requires stronger access controls and must not roam by default |
| System-managed hook settings | `/etc/zerostack` (Linux), `/Library/Application Support/zerostack` (macOS), `%ProgramData%\zerostack` (Windows) | Explicit read-only administrator-policy exception; no user override |

An artifact has exactly one owner and one canonical root. A module may receive a fully resolved
artifact path or an `AppPaths` reference; it may not reinterpret another root. Rebuildable cache
loss must never delete the authoritative skill database or disable primitive JS execution.

### Phase 3 skill storage

The canonical database is `<local_data_dir>/skills/skills.db`, not a config file. Embedding model
downloads live below `<cache_dir>/models`. An immutable in-memory `SkillIndex` is built from the
database; an optional serialized snapshot lives under `<cache_dir>/skills` and is always validated
against database/model generations before use.

---

## Portable Agent Skills archives

The portable semantic format is the open Agent Skills directory specification: one directory with
a required `SKILL.md`, YAML frontmatter containing `name` and `description`, and optional
`scripts/`, `references/`, `assets/`, and other resources. ZIP is a transport, not an identity or
a required literal filename. The importer accepts a local skill directory or a `.zip` containing
one skill tree; archives named `skill.zip` and any other `.zip` name behave identically.

The preferred archive layout contains one top-level directory whose name matches the `name` in
`SKILL.md`. For interoperability, an archive with `SKILL.md` at its root is normalized into that
named directory after frontmatter validation. Multiple skill roots, multiple `SKILL.md` files,
case-folded duplicate paths, or a frontmatter/directory-name mismatch are rejected.

Import is staged below `cache_dir`, validated without execution, content-hashed, and atomically
installed under `<data_dir>/agent-skills/<name>/<tree-digest>/`. The catalog points to exactly one
validated digest. Reimporting the same tree is idempotent; changing any file creates a new digest.

Archive validation is fail-closed:

- reject absolute paths, drive prefixes, UNC prefixes, `..`, NUL/control characters, alternate
  data stream syntax, symlinks, hard links, junction/reparse-point escapes, and non-regular files;
- reject Windows reserved device names, forbidden characters, trailing dots/spaces, and
  case-insensitive or Unicode-normalized path collisions on every platform;
- bound compressed bytes, expanded bytes, entry count, per-file bytes, path depth, component
  length, and compression ratio before extraction;
- write only beneath a newly created private staging directory and revalidate containment at the
  final no-follow write; and
- delete failed staging trees without following attacker-controlled links.

`allowed-tools` is experimental metadata. Importing it never grants permission. Existing session
policy, host capability manifests, sandboxing, and MCP permission checks remain authoritative.
Bundled `scripts/*.js` are Agent Skill resources, not learned JS exports: they are not injected into
QuickJS or admitted to the self-learning library unless separately proposed and verified through
Phases 3–5.

---

## Discovery and MCP composition

Agent Skill metadata and learned JS skill metadata use progressive disclosure and one prompt-time
query embedding per user turn. The query embedding may be shared by typed indexes, but each
catalog has a separate result and prompt budget:

- Agent Skills inject selected `SKILL.md` instructions and expose resources on demand.
- Learned JS skills expose a compact manifest to the model and bind exact verified JS source only
  inside the frozen `TurnSkillBundle`.

The current user prompt plus bounded deterministic context is the query. Stored skill vectors are
precomputed. SQLite is never scanned in the request path, generated JS is never a retrieval query,
and retries reuse the same bundle. Phase 3's exact pre-normalized dense scan, FTS5/BM25 fusion,
score floor, dedupe, and 5 ms p99 index-search target remain mandatory at 100,000 revisions.

MCP remains an independent, default-enabled Cargo feature. Standard Agent Skill instructions may
direct the model to configured MCP tools, but cannot create trusted MCP identities or bypass
`mcp_tool` permission checks. The `mcp`, `js`, `skills`, and combined `mcp,js,skills` feature rows
must compile and have production-wiring tests. Skill retrieval must neither remove MCP tools from
the agent nor duplicate MCP credentials into skill manifests, telemetry, or archives.

---

## Filesystem and credential safety

Generated persistent filenames use opaque full digests or validated fixed identifiers; they are
never derived by lossy replacement of untrusted display names. In particular, MCP server names,
project slugs, provider names, and archive entry names cannot become path components without the
shared validator. Collision checks use Unicode normalization plus case folding even on a
case-sensitive host so an artifact remains portable to Windows and default macOS filesystems.

On Unix, private roots are created as `0700` and credential/config/session files and all temporary
files as `0600` from creation. On Windows, credential files disable inherited broad access and use
a DACL that grants the current user (and the minimum operating-system principal required for
normal operation) access while excluding `Everyone` and ordinary `Users`. Unix mode-bit calls are
not treated as Windows protection.

For MCP OAuth, `canonical-server-identity` is the versioned, length-prefixed tuple of the exact
UTF-8 config map key, normalized absolute HTTP(S) URL, and explicit OAuth client ID (or empty).
URL normalization lowercases scheme/IDNA host, removes a default port, strips a fragment, and
preserves path/query bytes that affect the endpoint. A changed key, endpoint, or client ID gets a
different credential record. Scopes remain inside the bounded record and refresh flow rather than
the filename identity.

MCP OAuth files use
`<credentials_dir>/mcp-oauth/<sha256(canonical-server-identity)>.json`; the display name is metadata,
not a filename. Legacy migration computes old sanitized candidates from every configured server;
if two identities map to one legacy filename, migration reports a conflict instead of assigning
the token. Writes are exclusive, no-follow, bounded, atomic, and durable according to the shared
secure-write contract. Errors and logs never include tokens.

---

## Legacy discovery and migration

After resolving canonical paths, startup checks legacy locations only when the canonical artifact
is absent. Known legacy candidates include prior config-under-data, config-under-XDG-config,
session/data, hook trust, OAuth, log, and Phase 3 draft skill-database locations.

Migration rules:

1. Never silently choose between two different existing candidates. Report both and require an
   explicit selection or documented deterministic equivalence check.
2. Copy into a private temporary path, validate content and permissions, sync, then atomically
   publish. Do not delete the legacy source in the same release.
3. Record a versioned migration marker containing paths and non-secret digests. Restart is
   idempotent.
4. Never import durable/sensitive data from the current directory as a fallback.
5. OAuth migration re-applies the credential policy before the canonical file becomes visible.

An interactive client reports every conflicting candidate and requires an explicit user choice.
Headless/ACP startup never prompts or chooses: a required config conflict aborts startup with a
typed error, while an optional feature conflict disables only that feature and emits a diagnostic.
Neither path creates a new canonical artifact until the conflict is resolved.

User documentation may describe the target locations as supported only after the migration and
platform test gates pass. Until then, Windows storage/security claims remain qualified.

---

## Required tests

All resolution tests use injected platform/environment fixtures and isolated temporary roots.

- Linux matrix: every XDG override, each default, missing home/base directories, and precedence.
- macOS matrix: Application Support versus Caches and the derived state directory.
- Windows matrix: Roaming config/data versus Local data/state/cache/credentials, drive/UNC forms,
  long paths, reserved names, trailing dot/space, and case-fold collisions.
- Uniform override behavior: tilde expansion, absolute-path requirement, and no CWD fallback.
- Owner matrix: every persistent artifact resolves to the root in this specification.
- Migration: zero/one/multiple legacy candidates, restart, conflict, failure, and rollback source.
- Secure creation: permissive Unix umask, Windows ACL inspection, no-follow writes, and atomicity.
- Agent Skill imports: folder and ZIP forms, root normalization, malformed frontmatter, zip-slip,
  symlink/reparse entries, archive bombs, duplicate/colliding names, idempotency, and rollback.
- Feature matrix: default, `mcp`, `js`, `skills`, and `mcp,js,skills` production discovery.

Named validation targets:

```bash
cargo test app_paths_matrix
cargo test persistent_artifact_ownership
cargo test legacy_path_migration
cargo test portable_filename_policy
cargo test --features mcp mcp_oauth_storage_security
cargo test --features js,skills agent_skill_import
cargo test --features mcp,js,skills skill_mcp_composition
cargo test
cargo install --path . --debug
```

Before committing, run `cargo fmt`. Do not use `cargo build`, `cargo check`, or `--release`.
