use clap::{Parser, ValueEnum};
use compact_str::CompactString;

use crate::config;
use crate::config::types::EditSystem;

fn default_sandbox_backend() -> String {
    if cfg!(target_os = "windows") {
        "appcontainer".to_string()
    } else if cfg!(target_os = "macos") {
        "seatbelt".to_string()
    } else {
        "bwrap".to_string()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Parser, Debug, Default, Clone)]
#[command(name = "mini-agent", version, about = "Minimal coding agent")]
pub struct Cli {
    #[arg(short = 'p', long = "print", help = "Print response and exit")]
    pub print: bool,

    #[arg(
        long = "pure-stdout",
        help = "With -p: also print tool calls/results to stdout"
    )]
    pub pure_stdout: bool,

    #[arg(
        long = "output",
        value_name = "FORMAT",
        value_enum,
        requires = "print",
        conflicts_with = "pure_stdout",
        help = "With -p: output format (text or json)"
    )]
    pub output: Option<OutputFormat>,

    #[arg(long = "load-prompt", help = "Load a named prompt (same as /prompt)")]
    pub load_prompt: Option<String>,

    #[arg(long = "print-config", help = "Print resolved configuration and exit")]
    pub print_config: bool,

    #[arg(
        long = "config-preservation-check",
        help = "Verify config saves preserve unavailable and unknown fields, then exit"
    )]
    pub config_preservation_check: bool,

    #[arg(
        long = "project-config-trust-check",
        help = "Verify project-local executable and security settings require content-bound trust, then exit"
    )]
    pub project_config_trust_check: bool,

    #[cfg(feature = "js")]
    #[arg(
        long = "js-runtime-check",
        help = "Execute an offline JavaScript runtime self-check and exit"
    )]
    pub js_runtime_check: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-stats",
        conflicts_with_all = [
            "list_learned_skill_proposals",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Print learned-skill usage and estimated round trips saved, then exit"
    )]
    pub learned_skill_stats: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "list-learned-skill-proposals",
        conflicts_with_all = [
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "List learned-skill proposals awaiting an operator decision, then exit \
                (a verified proposal whose reason is held_out_suite_required is not \
                approvable until a matching held-out baseline is imported)"
    )]
    pub list_learned_skill_proposals: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-proposal",
        value_name = "SHA256",
        conflicts_with_all = [
            "learned_skill_stats",
            "list_learned_skill_proposals",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Print one learned-skill proposal's admission outcome, including the \
                reason_code and report_id of a rejected or deferred decision, then exit"
    )]
    pub learned_skill_proposal: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "purge-learned-skill",
        value_name = "SHA256",
        conflicts_with_all = [
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Permanently purge one learned-skill revision and its dependent records"
    )]
    pub purge_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "purge-learned-skill-force",
        requires = "purge_learned_skill",
        help = "Permit --purge-learned-skill to delete a revision that is not in a terminal \
                lifecycle status, or whose dependent revisions would be re-rooted"
    )]
    pub purge_learned_skill_force: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "compact-learned-skill-events",
        conflicts_with_all = [
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Compact learned-skill telemetry older than the retention window"
    )]
    pub compact_learned_skill_events: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-feedback",
        value_name = "SHA256",
        requires_all = [
            "learned_skill_feedback_kind",
            "learned_skill_feedback_reason",
            "learned_skill_feedback_key"
        ],
        conflicts_with_all = [
            "purge_learned_skill",
            "compact_learned_skill_events",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Submit authenticated local-owner feedback for one learned skill"
    )]
    pub learned_skill_feedback: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(long = "learned-skill-feedback-record", value_name = "SHA256",
        conflicts_with_all = ["learned_skill_stats", "list_learned_skill_proposals", "learned_skill_proposal", "purge_learned_skill", "compact_learned_skill_events", "learned_skill_feedback", "import_learned_skill", "install_learned_skill_seeds", "approve_learned_skill", "reject_learned_skill", "activate_learned_skill", "promote_learned_skill", "retire_learned_skill", "reevaluate_learned_skill", "list_learned_skill_suites", "disable_learned_skill_suite", "distill_learned_skill", "list_learned_skill_feedback", "correct_learned_skill_feedback"],
        help = "Inspect one feedback report without source or reason text")]
    pub learned_skill_feedback_record: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(long = "list-learned-skill-feedback", value_name = "SHA256",
        conflicts_with_all = ["learned_skill_stats", "list_learned_skill_proposals", "learned_skill_proposal", "purge_learned_skill", "compact_learned_skill_events", "learned_skill_feedback", "import_learned_skill", "install_learned_skill_seeds", "approve_learned_skill", "reject_learned_skill", "activate_learned_skill", "promote_learned_skill", "retire_learned_skill", "reevaluate_learned_skill", "list_learned_skill_suites", "disable_learned_skill_suite", "distill_learned_skill", "learned_skill_feedback_record", "correct_learned_skill_feedback"],
        help = "List up to 100 feedback reports for a skill with a continuation cursor")]
    pub list_learned_skill_feedback: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(long = "correct-learned-skill-feedback", num_args = 4, value_names = ["FEEDBACK_ID", "VERSION", "STATE", "REASON"],
        conflicts_with_all = ["learned_skill_stats", "list_learned_skill_proposals", "learned_skill_proposal", "purge_learned_skill", "compact_learned_skill_events", "learned_skill_feedback", "import_learned_skill", "install_learned_skill_seeds", "approve_learned_skill", "reject_learned_skill", "activate_learned_skill", "promote_learned_skill", "retire_learned_skill", "reevaluate_learned_skill", "list_learned_skill_suites", "disable_learned_skill_suite", "distill_learned_skill", "learned_skill_feedback_record", "list_learned_skill_feedback"],
        help = "Resolve or retract an active feedback report as the local owner; does not promote or release quarantine")]
    pub correct_learned_skill_feedback: Option<Vec<String>>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-feedback-after",
        value_name = "FEEDBACK_ID",
        requires = "list_learned_skill_feedback",
        help = "Continue after the previous page's next_after cursor"
    )]
    pub learned_skill_feedback_after: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-feedback-kind",
        value_name = "KIND",
        value_parser = ["positive", "negative", "severe"],
        requires = "learned_skill_feedback",
        help = "How the invocation behaved: positive, negative, or severe",
        long_help = "How the invocation behaved. `severe` is the containment signal: it quarantines \
                     a canary or active revision immediately, and only accepts one of the three \
                     safety reason codes. Report wrong output as `negative`."
    )]
    pub learned_skill_feedback_kind: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-feedback-reason",
        value_name = "CODE",
        requires = "learned_skill_feedback",
        help = "Why: severe accepts only integrity, permission_violation or unsafe_effect",
        long_help = "Why the invocation is being reported. For `severe` this must be exactly one \
                     of `integrity`, `permission_violation` or `unsafe_effect`. For `positive` and \
                     `negative` it is a free-form code of 1-64 bytes using lowercase letters and \
                     underscores."
    )]
    pub learned_skill_feedback_reason: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-feedback-key",
        value_name = "KEY",
        requires = "learned_skill_feedback",
        help = "Idempotency key, 1-128 bytes of [A-Za-z0-9._:-]",
        long_help = "Idempotency key for this report, 1-128 bytes of letters, digits and `. _ : -`. \
                     Resubmitting the same key with the same content is a no-op; reusing it with \
                     different content is rejected."
    )]
    pub learned_skill_feedback_key: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-feedback-invocation",
        value_name = "SHA256",
        requires = "learned_skill_feedback",
        help = "Attribute the report to one invocation id (64 lowercase hex)",
        long_help = "Attribute the report to a single invocation, as a 64-character lowercase hex \
                     id. The invocation must still be in raw telemetry, which is compacted after \
                     the retention window, and must belong to the skill being reported."
    )]
    pub learned_skill_feedback_invocation: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "import-learned-skill",
        value_name = "DIR_OR_JSON",
        conflicts_with_all = [
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Import and contained-verify learned-skill JSON package(s) for approval"
    )]
    pub import_learned_skill: Option<std::path::PathBuf>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "install-learned-skill-seeds",
        conflicts_with_all = [
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Import and contained-verify the bundled pure learned-skill seed library"
    )]
    pub install_learned_skill_seeds: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "approve-learned-skill",
        value_name = "SHA256",
        conflicts_with_all = [
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Approve an evaluated learned skill into non-retrievable canary state"
    )]
    pub approve_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "reject-learned-skill",
        value_name = "SHA256",
        conflicts_with_all = ["activate_learned_skill", "promote_learned_skill"],
        help = "Reject an evaluated learned skill as the authenticated local owner"
    )]
    pub reject_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "activate-learned-skill",
        value_name = "SHA256",
        conflicts_with = "promote_learned_skill",
        help = "Activate an approved root learned skill after its held-out baseline"
    )]
    pub activate_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "promote-learned-skill",
        value_name = "SHA256",
        help = "Promote an approved learned-skill replacement canary over its active or \
                quarantined predecessor as the authenticated local owner, superseding the \
                predecessor and preserving lineage"
    )]
    pub promote_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "retire-learned-skill",
        value_name = "SHA256",
        conflicts_with_all = [
            "learned_skill_stats",
            "list_learned_skill_proposals",
            "learned_skill_proposal",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill"
        ],
        help = "Retire an active learned skill as the authenticated local owner: an \
                administrative disable that preserves the revision and its lineage, \
                unlike --purge-learned-skill"
    )]
    pub retire_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "reevaluate-learned-skill",
        value_name = "SHA256",
        conflicts_with_all = [
            "learned_skill_stats",
            "list_learned_skill_proposals",
            "learned_skill_proposal",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill",
            "retire_learned_skill"
        ],
        help = "Requeue a parked learned-skill proposal as the authenticated local owner: one \
                verified with held_out_suite_required, or deferred after an infrastructure \
                outage or an exhausted attempt budget"
    )]
    pub reevaluate_learned_skill: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "list-learned-skill-suites",
        conflicts_with_all = [
            "learned_skill_stats",
            "list_learned_skill_proposals",
            "learned_skill_proposal",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill",
            "retire_learned_skill",
            "reevaluate_learned_skill",
            "distill_learned_skill",
            "disable_learned_skill_suite",
        ],
        help = "List trusted held-out suite IDs and enabled state without hidden cases, then exit"
    )]
    pub list_learned_skill_suites: bool,

    #[cfg(feature = "skills")]
    #[arg(
        long = "disable-learned-skill-suite",
        value_name = "SHA256",
        conflicts_with_all = [
            "learned_skill_stats",
            "list_learned_skill_proposals",
            "learned_skill_proposal",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill",
            "retire_learned_skill",
            "reevaluate_learned_skill",
            "distill_learned_skill",
            "list_learned_skill_suites",
        ],
        help = "Disable one trusted held-out suite as the local owner; validated reimport re-enables it"
    )]
    pub disable_learned_skill_suite: Option<String>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "distill-learned-skill",
        value_names = ["SESSION_ID", "TOOL_CALL_ID"],
        num_args = 2,
        conflicts_with_all = [
            "learned_skill_stats",
            "list_learned_skill_proposals",
            "learned_skill_proposal",
            "purge_learned_skill",
            "compact_learned_skill_events",
            "learned_skill_feedback",
            "import_learned_skill",
            "install_learned_skill_seeds",
            "approve_learned_skill",
            "reject_learned_skill",
            "activate_learned_skill",
            "promote_learned_skill",
            "retire_learned_skill",
            "reevaluate_learned_skill"
        ],
        help = "Distil the JavaScript of one persisted tool call into a learned-skill proposal \
                package for --import-learned-skill: reads the session's `js` tool call, asks the \
                configured provider once to generalize it, and writes the package. Nothing is \
                imported, verified or approved"
    )]
    pub distill_learned_skill: Option<Vec<String>>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "distill-learned-skill-out",
        value_name = "PATH",
        requires = "distill_learned_skill",
        help = "Write the distilled package to this path instead of the default under the \
                learned-skill data directory"
    )]
    pub distill_learned_skill_out: Option<std::path::PathBuf>,

    #[cfg(feature = "skills")]
    #[arg(
        long = "learned-skill-json",
        visible_alias = "json",
        help = "Emit learned-skill operator command results as one JSON object per line"
    )]
    pub learned_skill_json: bool,

    // Agent Skills are only ever read by the catalog, index and loader, which
    // exist only in a `skills` build. Without this gate the default release
    // binary accepted the flag and installed a tree nothing would ever read.
    #[cfg(feature = "skills")]
    #[arg(
        long = "import-agent-skill",
        value_name = "PATH",
        help = "Validate and install a local Agent Skills directory or ZIP archive"
    )]
    pub import_agent_skill: Option<std::path::PathBuf>,

    #[cfg(unix)]
    #[arg(
        long = "memory-editor-preservation-check",
        help = "Verify failed external memory edits preserve existing bytes, then exit"
    )]
    pub memory_editor_preservation_check: bool,

    #[arg(long = "setup", help = "Interactive setup wizard")]
    pub setup: bool,

    #[arg(long = "tutor", help = "Show getting started guide")]
    pub tutor: bool,

    #[arg(short = 'c', long = "continue", help = "Continue most recent session")]
    pub continue_session: bool,

    #[arg(short = 'r', long = "resume", help = "List recent sessions")]
    pub resume: bool,

    #[arg(long = "session", help = "Load session by ID prefix or exact name")]
    pub session: Option<String>,

    #[arg(
        long = "resume-provider",
        value_name = "PROVIDER",
        help = "Explicitly resume saved context with another provider/profile (privacy warning is displayed and the change is audited)"
    )]
    pub resume_provider: Option<String>,

    #[arg(
        long = "resume-model",
        value_name = "MODEL",
        help = "Explicitly resume with another model; uses the saved provider unless --resume-provider is also set"
    )]
    pub resume_model: Option<String>,

    #[arg(
        long = "resume-provider-safety-check",
        help = "Verify resume provider identity and explicit override semantics, then exit"
    )]
    pub resume_provider_safety_check: bool,

    #[arg(
        long = "acp-authentication-check",
        help = "Verify ACP TCP peer authentication rejects missing and replayed credentials, then exit"
    )]
    pub acp_authentication_check: bool,

    #[arg(
        long = "acp-permission-policy-check",
        help = "Verify headless ACP Ask permissions fail closed, then exit"
    )]
    pub acp_permission_policy_check: bool,

    #[cfg(all(feature = "loop", unix))]
    #[arg(
        long = "loop-verification-policy-check",
        help = "Verify workflow-only changes are relevant to headless loop verification, then exit"
    )]
    pub loop_verification_policy_check: bool,

    #[arg(long = "name", help = "Name for the session")]
    pub name: Option<String>,

    #[arg(long = "no-session", help = "Ephemeral mode, do not save")]
    pub no_session: bool,

    #[arg(long = "provider", env = "ZS_PROVIDER", help = "API provider")]
    pub provider: Option<String>,

    #[arg(long = "model", env = "ZS_MODEL", help = "Model name")]
    pub model: Option<String>,

    #[arg(long = "quick-model", help = "Use a named quick model from config")]
    pub quick_model: Option<String>,

    #[arg(
        long = "api-key",
        help = "API key for the provider (WARNING: visible to other users via ps/htop; prefer env vars)"
    )]
    pub api_key: Option<String>,

    #[arg(long = "max-tokens", help = "Maximum tokens in response")]
    pub max_tokens: Option<u64>,

    #[arg(long = "max-agent-turns", help = "Maximum agent turns")]
    pub max_agent_turns: Option<usize>,

    #[arg(long = "temperature", help = "Model temperature (0.0 to 2.0)")]
    pub temperature: Option<f64>,

    #[arg(
        short = 't',
        long = "tools",
        value_delimiter = ',',
        help = "Allowlist specific tools"
    )]
    pub tools: Vec<String>,

    #[arg(long = "no-tools", help = "Disable all tools")]
    pub no_tools: bool,

    #[arg(long = "no-color", help = "Disable colored TUI output")]
    pub no_color: bool,

    #[cfg(feature = "hooks")]
    #[arg(long = "no-hooks", help = "Disable all hooks")]
    pub no_hooks: bool,

    #[cfg(feature = "hooks")]
    #[arg(
        long = "hooks-test",
        value_name = "TOOL",
        help = "Dry-run PreToolUse hooks for TOOL with --hooks-test-input, print the merged decision, and exit"
    )]
    pub hooks_test: Option<String>,

    #[cfg(feature = "hooks")]
    #[arg(
        long = "hooks-test-input",
        help = "tool_input JSON for --hooks-test (default: {})"
    )]
    pub hooks_test_input: Option<String>,

    #[arg(long = "restrictive", short = 'R', help = "Ask for all operations")]
    pub restrictive: bool,

    #[arg(long = "read-only", help = "Allow reads only, deny everything else")]
    pub read_only: bool,

    #[arg(long = "guarded", help = "Allow reads, ask for all other operations")]
    pub guarded: bool,

    #[arg(
        long = "accept-all",
        help = "Auto-accept all operations within the working directory"
    )]
    pub accept_all: bool,

    #[arg(
        long = "yolo",
        help = "Allow all operations except destructive shell commands"
    )]
    pub yolo: bool,

    #[arg(
        long = "dangerously-skip-permissions",
        help = "Skip all permission checks (allow everything without any guard)"
    )]
    pub dangerously_skip_permissions: bool,

    #[arg(
        long = "sandbox",
        help = "Enforce the selected platform subprocess policy; fail closed if the backend is unavailable (capabilities vary by backend)"
    )]
    pub sandbox: bool,

    #[arg(
        long = "no-sandbox",
        conflicts_with = "sandbox",
        help = "Run subprocesses unsandboxed, overriding the default-on sandbox"
    )]
    pub no_sandbox: bool,

    #[arg(
        long = "sandbox-backend",
        help = "Sandbox backend: bwrap (Linux), seatbelt (macOS), appcontainer (Windows; restricted-token is a compatibility alias), or zerobox"
    )]
    pub sandbox_backend: Option<String>,

    #[arg(
        long = "windows-appcontainer-read-root",
        value_name = "PATH",
        help = "Add an explicit Windows AppContainer read/execute root (repeatable; relative paths resolve from the workspace)"
    )]
    pub windows_appcontainer_read_roots: Vec<std::path::PathBuf>,

    #[arg(
        long = "windows-appcontainer-write-root",
        value_name = "PATH",
        help = "Add an explicit Windows AppContainer read/write root (repeatable; relative paths resolve from the workspace)"
    )]
    pub windows_appcontainer_write_roots: Vec<std::path::PathBuf>,

    #[arg(
        long = "shell",
        help = "Executable for the shell tool: bash/sh, or PowerShell/pwsh on Windows"
    )]
    pub shell: Option<String>,

    #[arg(
        long = "edit-system",
        help = "Edit system (similarity or hashedit). Default: similarity"
    )]
    pub edit_system: Option<String>,

    #[arg(
        long = "no-context-files",
        short = 'n',
        help = "Disable AGENTS.md, CLAUDE.md, and ARCHITECTURE.md loading"
    )]
    pub no_context_files: bool,

    #[cfg(feature = "loop")]
    #[arg(
        long = "loop",
        help = "Run in headless loop mode (requires --loop-prompt or message)"
    )]
    pub loop_mode: bool,

    #[cfg(feature = "acp")]
    #[arg(
        long = "acp",
        help = "Enable ACP (Agent Communication Protocol) support"
    )]
    pub acp_enabled: bool,

    #[cfg(feature = "acp")]
    #[arg(long = "acp-host", help = "ACP TCP bind host [default: stdio mode]")]
    pub acp_host: Option<String>,

    #[cfg(feature = "acp")]
    #[arg(long = "acp-port", help = "ACP TCP bind port [default: 7243]")]
    pub acp_port: Option<u16>,

    #[cfg(feature = "loop")]
    #[arg(long = "loop-prompt", help = "Prompt for each loop iteration")]
    pub loop_prompt: Option<String>,

    #[cfg(feature = "loop")]
    #[arg(long = "loop-plan", help = "Plan file path [default: LOOP_PLAN.md]")]
    pub loop_plan: Option<std::path::PathBuf>,

    #[cfg(feature = "loop")]
    #[arg(long = "loop-max", help = "Maximum number of iterations")]
    pub loop_max: Option<u32>,

    #[cfg(feature = "loop")]
    #[arg(
        long = "loop-run",
        help = "Validation command to run after each iteration"
    )]
    pub loop_run: Option<String>,

    #[cfg(feature = "goal")]
    #[arg(
        long = "goal",
        help = "Persistent objective to work toward across turns"
    )]
    pub goal: Option<String>,

    #[cfg(feature = "goal")]
    #[arg(
        long = "goal-done",
        help = "Completion criterion for the goal (repeatable)"
    )]
    pub goal_done: Vec<String>,

    #[cfg(feature = "goal")]
    #[arg(
        long = "goal-check",
        help = "Command that must exit zero before the goal may be reported complete (repeatable)"
    )]
    pub goal_check: Vec<String>,

    #[cfg(feature = "goal")]
    #[arg(
        long = "goal-max-rounds",
        help = "Maximum goal rounds before wrapping up [default: 50]"
    )]
    pub goal_max_rounds: Option<u32>,

    #[cfg(feature = "goal")]
    #[arg(
        long = "goal-continuation",
        value_parser = ["continue", "restart"],
        help = "Whether each goal round keeps the conversation or starts fresh"
    )]
    pub goal_continuation: Option<String>,

    #[cfg(feature = "goal")]
    #[arg(
        long = "goal-replace",
        help = "Replace an unfinished goal instead of refusing"
    )]
    pub goal_replace: bool,

    #[cfg(feature = "git-worktree")]
    #[arg(long = "worktree", help = "Create a git worktree and cd into it")]
    pub worktree: Option<String>,

    #[cfg(feature = "git-worktree")]
    #[arg(long = "wt-auto-merge", help = "Auto-merge worktree branch on exit")]
    pub wt_auto_merge: bool,

    #[cfg(feature = "git-worktree")]
    #[arg(
        long = "parallel",
        help = "Create a worktree with timestamp name and auto-merge on exit"
    )]
    pub parallel: bool,

    #[cfg(feature = "git-worktree")]
    #[arg(
        long = "wt-base-dir",
        help = "Base directory for worktrees (default: parent of current repo)"
    )]
    pub wt_base_dir: Option<String>,

    #[cfg(feature = "advisor")]
    #[arg(
        long = "advisor",
        help = "Enable advisor tool (model can consult a stronger reviewer model)"
    )]
    pub advisor: bool,

    #[cfg(feature = "advisor")]
    #[arg(
        long = "advisor-model",
        help = "Advisor model name (e.g. 'claude-opus-4-8')"
    )]
    pub advisor_model: Option<String>,

    #[cfg(feature = "advisor")]
    #[arg(
        long = "advisor-max-uses",
        help = "Maximum advisor calls per request (0 = unlimited; uses config when omitted)"
    )]
    pub advisor_max_uses: Option<usize>,

    #[cfg(feature = "advisor")]
    #[arg(
        long = "advisor-human-handoff",
        help = "Route advisor calls to the user instead of a model",
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true
    )]
    pub advisor_human_handoff: Option<bool>,

    #[cfg(feature = "advisor")]
    #[arg(
        long = "advisor-kilobytes-limit",
        help = "Max total kilobytes of conversation context to send to the advisor (head: half, tail: half). Default: 256"
    )]
    pub advisor_kilobytes_limit: Option<u32>,

    #[cfg(feature = "status-signals")]
    #[arg(
        long = "status-socket",
        help = "Unix socket path for status signals (start/stop messages)"
    )]
    pub status_socket: Option<String>,

    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::SetTrue,
          help = "Enable full logging (trace level) to a timestamped log file under the data directory")]
    pub verbose: bool,

    #[arg(
        long = "log-file",
        help = "Write logs to this file (overrides verbose default path)"
    )]
    pub log_file: Option<std::path::PathBuf>,

    #[arg(
        long = "log-level",
        help = "Set stderr log level (trace, debug, info, warn, error)"
    )]
    pub log_level: Option<String>,

    #[arg(help = "Prompt message(s)")]
    pub message: Vec<String>,
}

impl Cli {
    pub fn output_format(&self) -> OutputFormat {
        self.output.unwrap_or_default()
    }

    pub fn is_headless(&self) -> bool {
        self.print || {
            #[cfg(feature = "loop")]
            {
                self.loop_mode
            }
            #[cfg(not(feature = "loop"))]
            {
                false
            }
        }
    }

    pub fn resolve_quick_model<'a>(
        &self,
        cfg: &'a config::Config,
    ) -> Option<&'a config::QuickModelConfig> {
        let name = self.quick_model.as_deref()?;
        cfg.quick_models.as_ref().and_then(|m| m.get(name))
    }

    pub fn resolve_model(&self, cfg: &config::Config) -> CompactString {
        // CLI --model takes a raw model string.
        if let Some(m) = self.model.as_deref() {
            return CompactString::new(m);
        }
        // OPENROUTER_MODEL env var (higher priority than config file).
        if let Ok(m) = std::env::var("OPENROUTER_MODEL")
            && !m.is_empty()
        {
            return CompactString::new(m);
        }
        // Config model field references a quick model name; resolve it.
        if let Some(m) = cfg.model.as_deref() {
            let qm = config::quick_models_map(cfg);
            if let Some(q) = qm.get(m) {
                return q.model.clone();
            }
            return CompactString::new(m);
        }
        // No explicit model. If a provider was chosen explicitly, default to a
        // model valid for it so `--provider anthropic` does not keep the
        // OpenRouter default id; otherwise keep the historic deepseek default.
        if (self.provider.is_some() || cfg.provider.is_some())
            && let Some((model, _)) =
                crate::provider::default_model_for_provider(&self.resolve_provider(cfg), cfg)
        {
            return CompactString::new(model);
        }
        let qm = config::quick_models_map(cfg);
        qm.get("deepseek-v4-pro")
            .map(|q| q.model.clone())
            .unwrap_or_else(|| CompactString::new("deepseek/deepseek-v4-pro"))
    }

    pub fn resolve_provider(&self, cfg: &config::Config) -> CompactString {
        self.provider
            .as_deref()
            .or(cfg.provider.as_deref())
            .map(CompactString::new)
            .unwrap_or_else(|| {
                let qm = config::quick_models_map(cfg);
                qm.get("deepseek-v4-pro")
                    .map(|q| q.provider.clone())
                    .unwrap_or_else(|| CompactString::new("openrouter"))
            })
    }

    pub fn resolve_max_tokens(&self, cfg: &config::Config) -> u64 {
        self.max_tokens.or(cfg.max_tokens).unwrap_or(16384)
    }

    pub fn resolve_max_agent_turns(&self, cfg: &config::Config) -> usize {
        self.max_agent_turns.or(cfg.max_agent_turns).unwrap_or(200)
    }

    pub fn resolve_no_context_files(&self, cfg: &config::Config) -> bool {
        self.no_context_files || cfg.no_context_files.unwrap_or(false)
    }

    pub fn resolve_no_tools(&self, cfg: &config::Config) -> bool {
        self.no_tools || cfg.no_tools.unwrap_or(false)
    }

    pub(crate) fn tool_is_eligible(&self, cfg: &config::Config, name: &str) -> bool {
        !self.resolve_no_tools(cfg)
            && (self.tools.is_empty()
                || self
                    .tools
                    .iter()
                    .any(|allowed| canonical_tool_name(allowed) == canonical_tool_name(name)))
    }

    /// Operator-configured validation needs a shell even when the model has
    /// no shell tool. This does not change model tool eligibility.
    pub(crate) fn configured_validation_needs_shell(&self, cfg: &config::Config) -> bool {
        #[cfg(feature = "loop")]
        if self.loop_run.is_some() {
            return true;
        }
        #[cfg(feature = "goal")]
        if !self.goal_check.is_empty() || cfg.goal_checks.as_ref().is_some_and(|c| !c.is_empty()) {
            return true;
        }
        cfg.verify_command
            .as_deref()
            .is_some_and(|command| !command.trim().is_empty())
    }

    pub(crate) fn general_sandbox_is_eligible(&self, cfg: &config::Config) -> bool {
        self.tool_is_eligible(cfg, "shell")
            || cfg!(feature = "js") && self.tool_is_eligible(cfg, "js")
            || self.configured_validation_needs_shell(cfg)
    }

    #[cfg(feature = "mcp")]
    pub(crate) fn mcp_is_eligible(&self, cfg: &config::Config) -> bool {
        const BUILTIN_TOOLS: &[&str] = &[
            "read",
            "write",
            "edit",
            "grep",
            "find_files",
            "list_dir",
            "todo_write",
            "todo_read",
            "shell",
            "bash",
            #[cfg(feature = "js")]
            "js",
            #[cfg(feature = "subagents")]
            "task",
            #[cfg(feature = "memory")]
            "memory_write",
            #[cfg(feature = "memory")]
            "memory_edit",
            #[cfg(feature = "memory")]
            "memory_read",
            #[cfg(feature = "memory")]
            "memory_search",
            #[cfg(feature = "advisor")]
            "advisor",
            #[cfg(feature = "lsp")]
            "lsp_diagnostics",
        ];

        !self.resolve_no_tools(cfg)
            && (self.tools.is_empty()
                || self
                    .tools
                    .iter()
                    .any(|name| !BUILTIN_TOOLS.contains(&name.as_str())))
    }

    /// Sandboxing is on unless explicitly refused. `--no-sandbox` outranks
    /// config so a host without a working backend can still start.
    pub fn resolve_sandbox(&self, cfg: &config::Config) -> bool {
        if self.no_sandbox {
            return false;
        }
        self.sandbox || cfg.sandbox.unwrap_or(true)
    }

    /// Whether the operator asked for a sandbox or selected its backend rather
    /// than inheriting the complete default. An explicit request stays
    /// fail-closed when the backend is missing; an unavailable platform
    /// default may degrade with a warning where the platform policy permits.
    pub fn sandbox_explicitly_requested(&self, cfg: &config::Config) -> bool {
        !self.no_sandbox
            && (self.sandbox
                || cfg.sandbox == Some(true)
                || self.sandbox_backend.is_some()
                || cfg.sandbox_backend.is_some())
    }

    pub fn resolve_sandbox_backend(&self, cfg: &config::Config) -> String {
        self.sandbox_backend
            .clone()
            .or_else(|| cfg.sandbox_backend.clone())
            .unwrap_or_else(default_sandbox_backend)
    }

    pub fn resolve_windows_appcontainer_read_roots(
        &self,
        cfg: &config::Config,
    ) -> Vec<std::path::PathBuf> {
        if self.windows_appcontainer_read_roots.is_empty() {
            cfg.windows_appcontainer_read_roots.clone()
        } else {
            self.windows_appcontainer_read_roots.clone()
        }
    }

    pub fn resolve_windows_appcontainer_write_roots(
        &self,
        cfg: &config::Config,
    ) -> Vec<std::path::PathBuf> {
        if self.windows_appcontainer_write_roots.is_empty() {
            cfg.windows_appcontainer_write_roots.clone()
        } else {
            self.windows_appcontainer_write_roots.clone()
        }
    }

    pub fn resolve_shell(&self, cfg: &config::Config) -> String {
        self.shell
            .clone()
            .or_else(|| cfg.shell.clone())
            .unwrap_or_else(|| {
                if cfg!(target_os = "windows") {
                    "powershell.exe".to_string()
                } else {
                    "bash".to_string()
                }
            })
    }

    pub fn resolve_edit_system(&self, cfg: &config::Config) -> EditSystem {
        self.edit_system
            .as_deref()
            .and_then(|s| s.parse().ok())
            .or(cfg.edit_system)
            .unwrap_or_default()
    }

    #[cfg(feature = "git-worktree")]
    pub fn resolve_wt_auto_merge(&self, cfg: &config::Config) -> bool {
        self.wt_auto_merge || self.parallel || cfg.wt_auto_merge.unwrap_or(false)
    }

    #[cfg(feature = "git-worktree")]
    pub fn resolve_wt_base_dir(&self, cfg: &config::Config) -> Option<std::path::PathBuf> {
        self.wt_base_dir
            .clone()
            .or_else(|| cfg.wt_base_dir.clone())
            .map(std::path::PathBuf::from)
    }

    #[cfg(feature = "advisor")]
    pub fn resolve_advisor_enabled(&self, cfg: &config::Config) -> bool {
        if let Some(ref ac) = cfg.advisor {
            self.advisor || ac.enabled
        } else {
            self.advisor
        }
    }

    #[cfg(feature = "advisor")]
    pub fn resolve_advisor_model(&self, cfg: &config::Config) -> String {
        self.advisor_model
            .clone()
            .or_else(|| {
                cfg.advisor
                    .as_ref()
                    .and_then(|a| a.model.clone())
                    .map(|m| m.to_string())
            })
            .unwrap_or_else(|| "deepseek-v4-pro".to_string())
    }

    #[cfg(feature = "advisor")]
    pub fn resolve_advisor_max_uses(&self, cfg: &config::Config) -> Option<usize> {
        self.advisor_max_uses
            .or_else(|| cfg.advisor.as_ref().and_then(|a| a.max_uses))
            .filter(|&limit| limit != 0)
    }

    #[cfg(feature = "advisor")]
    pub fn resolve_advisor_human_handoff(&self, cfg: &config::Config) -> bool {
        self.advisor_human_handoff.unwrap_or_else(|| {
            cfg.advisor
                .as_ref()
                .map(|a| a.human_handoff)
                .unwrap_or(false)
        })
    }

    #[cfg(feature = "advisor")]
    pub fn resolve_advisor_kilobytes_limit(&self, cfg: &config::Config) -> u32 {
        self.advisor_kilobytes_limit
            .or_else(|| cfg.advisor.as_ref().map(|a| a.advisor_kilobytes_limit))
            .unwrap_or(256)
    }
}

fn canonical_tool_name(name: &str) -> &str {
    if name == "bash" { "shell" } else { name }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, OutputFormat, default_sandbox_backend};
    use crate::config;

    #[cfg(feature = "advisor")]
    #[test]
    fn advisor_handoff_cli_config_precedence() {
        for (source, configured) in [
            ("", false),
            ("[advisor]", true),
            ("[advisor]\nhuman_handoff = false", false),
            ("[advisor]\nhuman_handoff = true", true),
        ] {
            let cfg: config::Config = toml::from_str(source).unwrap();
            for (args, explicit) in [
                (&[][..], None),
                (&["--advisor-human-handoff"][..], Some(true)),
                (&["--advisor-human-handoff=true"][..], Some(true)),
                (&["--advisor-human-handoff=false"][..], Some(false)),
            ] {
                let cli =
                    Cli::try_parse_from(std::iter::once("mini-agent").chain(args.iter().copied()))
                        .unwrap();
                assert_eq!(
                    cli.resolve_advisor_human_handoff(&cfg),
                    explicit.unwrap_or(configured),
                    "{source:?}, {args:?}"
                );
            }
        }
    }

    #[cfg(feature = "advisor")]
    #[test]
    fn advisor_context_limit_cli_config_precedence() {
        for (source, configured) in [
            ("", 256),
            ("[advisor]", 256),
            ("[advisor]\nadvisor_kilobytes_limit = 128", 128),
            ("[advisor]\nadvisor_kilobytes_limit = 0", 0),
        ] {
            let cfg: config::Config = toml::from_str(source).unwrap();
            for explicit in [None, Some(0u32), Some(256), Some(512), Some(u32::MAX)] {
                let mut args = vec!["mini-agent".to_owned()];
                if let Some(limit) = explicit {
                    args.extend(["--advisor-kilobytes-limit".to_owned(), limit.to_string()]);
                }
                let cli = Cli::try_parse_from(&args).unwrap();
                assert_eq!(
                    cli.resolve_advisor_kilobytes_limit(&cfg),
                    explicit.unwrap_or(configured),
                    "{source:?}, {args:?}"
                );
            }
        }
    }

    #[cfg(feature = "advisor")]
    #[test]
    fn advisor_call_limit_cli_config_precedence() {
        for (source, configured) in [
            ("", None),
            ("[advisor]", Some(3)),
            ("[advisor]\nmax_uses = 0", None),
            ("[advisor]\nmax_uses = 7", Some(7)),
        ] {
            let cfg: config::Config = toml::from_str(source).unwrap();
            for (explicit, expected) in [
                (None, configured),
                (Some(0), None),
                (Some(3), Some(3)),
                (Some(9), Some(9)),
            ] {
                let mut args = vec!["mini-agent".to_owned()];
                if let Some(limit) = explicit {
                    args.extend(["--advisor-max-uses".to_owned(), limit.to_string()]);
                }
                let cli = Cli::try_parse_from(&args).unwrap();
                assert_eq!(
                    cli.resolve_advisor_max_uses(&cfg),
                    expected,
                    "{source:?}, {args:?}"
                );
            }
        }
    }

    #[test]
    fn print_output_format_defaults_to_text_and_accepts_json() {
        let text = Cli::try_parse_from(["mini-agent", "-p", "hello"]).unwrap();
        assert_eq!(text.output_format(), OutputFormat::Text);

        let json = Cli::try_parse_from(["mini-agent", "-p", "--output", "json", "hello"]).unwrap();
        assert_eq!(json.output_format(), OutputFormat::Json);
        assert!(json.is_headless());
    }

    #[test]
    fn structured_output_requires_print_and_conflicts_with_pure_stdout() {
        assert!(Cli::try_parse_from(["mini-agent", "--output", "json"]).is_err());
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "-p",
                "--output",
                "json",
                "--pure-stdout",
                "hello",
            ])
            .is_err()
        );
    }

    #[test]
    fn command_name_matches_canonical_cargo_binary() {
        assert_eq!(Cli::command().get_name(), "mini-agent");
    }

    #[test]
    fn tool_eligibility_honors_global_and_named_suppression() {
        let cfg = config::Config::default();
        let all = Cli::default();
        assert!(all.tool_is_eligible(&cfg, "bash"));
        assert!(all.tool_is_eligible(&cfg, "js"));
        assert!(all.general_sandbox_is_eligible(&cfg));

        let bash_only = Cli {
            tools: vec!["bash".to_string()],
            ..Cli::default()
        };
        assert!(bash_only.tool_is_eligible(&cfg, "bash"));
        assert!(!bash_only.tool_is_eligible(&cfg, "js"));
        assert!(bash_only.general_sandbox_is_eligible(&cfg));

        let js_only = Cli {
            tools: vec!["js".to_string()],
            ..Cli::default()
        };
        assert!(!js_only.tool_is_eligible(&cfg, "bash"));
        assert!(js_only.tool_is_eligible(&cfg, "js"));
        assert_eq!(
            js_only.general_sandbox_is_eligible(&cfg),
            cfg!(feature = "js")
        );

        let read_only = Cli {
            tools: vec!["read".to_string()],
            ..Cli::default()
        };
        assert!(!read_only.tool_is_eligible(&cfg, "bash"));
        assert!(!read_only.tool_is_eligible(&cfg, "js"));
        assert!(!read_only.general_sandbox_is_eligible(&cfg));

        let no_tools = Cli {
            no_tools: true,
            tools: vec!["bash".to_string(), "js".to_string()],
            ..Cli::default()
        };
        assert!(!no_tools.tool_is_eligible(&cfg, "bash"));
        assert!(!no_tools.tool_is_eligible(&cfg, "js"));
        assert!(!no_tools.general_sandbox_is_eligible(&cfg));

        #[cfg(feature = "mcp")]
        {
            assert!(all.mcp_is_eligible(&cfg));
            assert!(!read_only.mcp_is_eligible(&cfg));
            assert!(!bash_only.mcp_is_eligible(&cfg));
            assert!(!no_tools.mcp_is_eligible(&cfg));
            let mcp_only = Cli {
                tools: vec!["github_search".to_string()],
                ..Cli::default()
            };
            assert!(mcp_only.mcp_is_eligible(&cfg));
        }
    }

    #[test]
    fn sandbox_backend_default_is_platform_appropriate_and_overridable() {
        let mut cli = Cli::default();
        let mut cfg = config::Config::default();
        assert_eq!(cli.resolve_sandbox_backend(&cfg), default_sandbox_backend());

        cfg.sandbox_backend = Some("configured".to_string());
        assert_eq!(cli.resolve_sandbox_backend(&cfg), "configured");

        cli.sandbox_backend = Some("command-line".to_string());
        assert_eq!(cli.resolve_sandbox_backend(&cfg), "command-line");
    }

    #[test]
    fn windows_appcontainer_roots_are_explicit_scoped_and_cli_overrides_config() {
        let mut cfg = config::Config {
            windows_appcontainer_read_roots: vec!["configured-read".into()],
            windows_appcontainer_write_roots: vec!["configured-write".into()],
            ..Default::default()
        };
        let mut cli = Cli::default();
        assert_eq!(
            cli.resolve_windows_appcontainer_read_roots(&cfg),
            vec![std::path::PathBuf::from("configured-read")]
        );
        assert_eq!(
            cli.resolve_windows_appcontainer_write_roots(&cfg),
            vec![std::path::PathBuf::from("configured-write")]
        );

        cli.windows_appcontainer_read_roots = vec!["cli-read".into()];
        cli.windows_appcontainer_write_roots = vec!["cli-write".into()];
        assert_eq!(
            cli.resolve_windows_appcontainer_read_roots(&cfg),
            vec![std::path::PathBuf::from("cli-read")]
        );
        assert_eq!(
            cli.resolve_windows_appcontainer_write_roots(&cfg),
            vec![std::path::PathBuf::from("cli-write")]
        );

        cfg.windows_appcontainer_read_roots.clear();
        cfg.windows_appcontainer_write_roots.clear();
        assert!(!cli.resolve_windows_appcontainer_read_roots(&cfg).is_empty());
    }

    #[test]
    fn sandbox_is_on_by_default_and_refusable() {
        let mut cli = Cli::default();
        let mut cfg = config::Config::default();
        assert!(cli.resolve_sandbox(&cfg));

        cfg.sandbox = Some(false);
        assert!(!cli.resolve_sandbox(&cfg));

        cli.sandbox = true;
        assert!(cli.resolve_sandbox(&cfg));

        // --no-sandbox is the escape hatch the startup error points users to,
        // so it has to outrank both the flag and the config.
        cli.no_sandbox = true;
        assert!(!cli.resolve_sandbox(&cfg));
        cfg.sandbox = Some(true);
        assert!(!cli.resolve_sandbox(&cfg));
    }

    #[test]
    fn selecting_a_sandbox_backend_is_an_explicit_fail_closed_request() {
        let cli = Cli {
            sandbox_backend: Some("selected-backend".to_string()),
            ..Cli::default()
        };
        assert!(cli.sandbox_explicitly_requested(&config::Config::default()));

        let cfg = config::Config {
            sandbox_backend: Some("configured-backend".to_string()),
            ..config::Config::default()
        };
        assert!(Cli::default().sandbox_explicitly_requested(&cfg));

        let refused = Cli {
            no_sandbox: true,
            sandbox_backend: Some("ignored-backend".to_string()),
            ..Cli::default()
        };
        assert!(!refused.sandbox_explicitly_requested(&cfg));
    }

    #[cfg(feature = "skills")]
    #[test]
    fn feedback_management_flags_require_complete_exclusive_actions() {
        let id = "a".repeat(64);
        for action in [
            vec!["--learned-skill-feedback-record", &id],
            vec!["--list-learned-skill-feedback", &id],
            vec![
                "--correct-learned-skill-feedback",
                &id,
                "1",
                "resolved",
                "mistaken_report",
            ],
        ] {
            let mut args = vec!["mini-agent"];
            args.extend(action);
            assert!(Cli::try_parse_from(&args).is_ok());
            for other in [
                vec!["--learned-skill-stats"],
                vec!["--promote-learned-skill", &id],
                vec!["--list-learned-skill-suites"],
                vec!["--distill-learned-skill", "session", "call"],
            ] {
                let mut conflicting = args.clone();
                conflicting.extend(other);
                assert!(Cli::try_parse_from(conflicting).is_err());
            }
        }
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--correct-learned-skill-feedback",
                &id,
                "1",
                "resolved"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["mini-agent", "--learned-skill-feedback-after", &id]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--list-learned-skill-feedback",
                &id,
                "--learned-skill-feedback-after",
                &id
            ])
            .is_ok()
        );
    }

    #[cfg(feature = "skills")]
    #[test]
    fn learned_skill_operator_flags_require_complete_non_conflicting_inputs() {
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--learned-skill-feedback",
                &"a".repeat(64),
                "--learned-skill-feedback-kind",
                "severe",
                "--learned-skill-feedback-reason",
                "integrity",
                "--learned-skill-feedback-key",
                "feedback-1",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["mini-agent", "--learned-skill-feedback", &"a".repeat(64),])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--purge-learned-skill",
                &"a".repeat(64),
                "--compact-learned-skill-events",
            ])
            .is_err()
        );
        let imported =
            Cli::try_parse_from(["mini-agent", "--import-learned-skill", "skill.json"]).unwrap();
        assert_eq!(
            imported.import_learned_skill,
            Some(std::path::PathBuf::from("skill.json"))
        );
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--approve-learned-skill",
                &"b".repeat(64),
                "--reject-learned-skill",
                &"b".repeat(64),
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["mini-agent", "--install-learned-skill-seeds"]).is_ok());
        assert!(Cli::try_parse_from(["mini-agent", "--learned-skill-stats"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--learned-skill-stats",
                "--install-learned-skill-seeds",
            ])
            .is_err()
        );
        let listed = Cli::try_parse_from(["mini-agent", "--list-learned-skill-proposals"]).unwrap();
        assert!(listed.list_learned_skill_proposals);
        let promoted =
            Cli::try_parse_from(["mini-agent", "--promote-learned-skill", &"c".repeat(64)])
                .unwrap();
        assert_eq!(promoted.promote_learned_skill, Some("c".repeat(64)));
        for conflicting in [
            "--learned-skill-stats",
            "--list-learned-skill-proposals",
            "--compact-learned-skill-events",
            "--install-learned-skill-seeds",
        ] {
            assert!(
                Cli::try_parse_from([
                    "mini-agent",
                    "--promote-learned-skill",
                    &"c".repeat(64),
                    conflicting,
                ])
                .is_err(),
                "{conflicting} must conflict with --promote-learned-skill"
            );
        }
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--promote-learned-skill",
                &"c".repeat(64),
                "--activate-learned-skill",
                &"c".repeat(64),
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--list-learned-skill-proposals",
                "--approve-learned-skill",
                &"c".repeat(64),
            ])
            .is_err()
        );
    }

    #[cfg(feature = "skills")]
    #[test]
    fn learned_skill_purge_force_retire_proposal_and_json_flags_parse() {
        // Purge is destructive, so its override is only meaningful alongside it.
        assert!(Cli::try_parse_from(["mini-agent", "--purge-learned-skill-force"]).is_err());
        let forced = Cli::try_parse_from([
            "mini-agent",
            "--purge-learned-skill",
            &"a".repeat(64),
            "--purge-learned-skill-force",
        ])
        .unwrap();
        assert!(forced.purge_learned_skill_force);
        assert!(
            !Cli::try_parse_from(["mini-agent", "--purge-learned-skill", &"a".repeat(64)])
                .unwrap()
                .purge_learned_skill_force
        );

        let retired =
            Cli::try_parse_from(["mini-agent", "--retire-learned-skill", &"b".repeat(64)]).unwrap();
        assert_eq!(retired.retire_learned_skill, Some("b".repeat(64)));
        for conflicting in [
            "--purge-learned-skill",
            "--activate-learned-skill",
            "--promote-learned-skill",
        ] {
            assert!(
                Cli::try_parse_from([
                    "mini-agent",
                    "--retire-learned-skill",
                    &"b".repeat(64),
                    conflicting,
                    &"b".repeat(64),
                ])
                .is_err(),
                "{conflicting} must conflict with --retire-learned-skill"
            );
        }

        let queried =
            Cli::try_parse_from(["mini-agent", "--learned-skill-proposal", &"c".repeat(64)])
                .unwrap();
        assert_eq!(queried.learned_skill_proposal, Some("c".repeat(64)));
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--learned-skill-proposal",
                &"c".repeat(64),
                "--list-learned-skill-proposals",
            ])
            .is_err()
        );

        let id = "d".repeat(64);
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--list-learned-skill-suites",
                "--learned-skill-json"
            ])
            .unwrap()
            .list_learned_skill_suites
        );
        assert_eq!(
            Cli::try_parse_from(["mini-agent", "--disable-learned-skill-suite", &id])
                .unwrap()
                .disable_learned_skill_suite,
            Some(id.clone())
        );
        for new_mode in [
            vec!["--list-learned-skill-suites"],
            vec!["--disable-learned-skill-suite", id.as_str()],
        ] {
            for other_mode in [
                vec!["--learned-skill-stats"],
                vec!["--import-learned-skill", "package.json"],
                vec!["--reevaluate-learned-skill", id.as_str()],
                vec!["--distill-learned-skill", "session", "call"],
            ] {
                let mut args = vec!["mini-agent"];
                args.extend(new_mode.iter().copied());
                args.extend(other_mode);
                assert!(
                    Cli::try_parse_from(args).is_err(),
                    "suite operation must not be silently ignored"
                );
            }
        }
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--list-learned-skill-suites",
                "--disable-learned-skill-suite",
                &id
            ])
            .is_err()
        );

        // `--json` is a modifier, not a mode: it composes with any command.
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--install-learned-skill-seeds",
                "--learned-skill-json",
            ])
            .unwrap()
            .learned_skill_json
        );
        assert!(
            Cli::try_parse_from(["mini-agent", "--list-learned-skill-proposals", "--json"])
                .unwrap()
                .learned_skill_json
        );
        assert!(
            !Cli::try_parse_from(["mini-agent"])
                .unwrap()
                .learned_skill_json
        );
    }

    #[cfg(feature = "skills")]
    #[test]
    fn distill_learned_skill_takes_a_session_and_tool_call_and_conflicts_with_its_siblings() {
        let distilled = Cli::try_parse_from([
            "mini-agent",
            "--distill-learned-skill",
            "session-1",
            "toolu_01",
        ])
        .unwrap();
        assert_eq!(
            distilled.distill_learned_skill,
            Some(vec!["session-1".to_string(), "toolu_01".to_string()])
        );
        assert!(distilled.distill_learned_skill_out.is_none());

        // Both values are required: one identifier cannot name a tool call.
        assert!(
            Cli::try_parse_from(["mini-agent", "--distill-learned-skill", "session-1"]).is_err()
        );

        let redirected = Cli::try_parse_from([
            "mini-agent",
            "--distill-learned-skill",
            "session-1",
            "toolu_01",
            "--distill-learned-skill-out",
            "draft.json",
        ])
        .unwrap();
        assert_eq!(
            redirected.distill_learned_skill_out,
            Some(std::path::PathBuf::from("draft.json"))
        );
        // The destination is meaningless without the command that fills it.
        assert!(
            Cli::try_parse_from(["mini-agent", "--distill-learned-skill-out", "draft.json"])
                .is_err()
        );

        for conflicting in [
            "--learned-skill-stats",
            "--list-learned-skill-proposals",
            "--compact-learned-skill-events",
            "--install-learned-skill-seeds",
        ] {
            assert!(
                Cli::try_parse_from([
                    "mini-agent",
                    "--distill-learned-skill",
                    "session-1",
                    "toolu_01",
                    conflicting,
                ])
                .is_err(),
                "{conflicting} must conflict with --distill-learned-skill"
            );
        }
        assert!(
            Cli::try_parse_from([
                "mini-agent",
                "--distill-learned-skill",
                "session-1",
                "toolu_01",
                "--import-learned-skill",
                "package.json",
            ])
            .is_err(),
            "distillation writes the package --import-learned-skill later reads; they are two \
             separate operator steps"
        );
    }
}
