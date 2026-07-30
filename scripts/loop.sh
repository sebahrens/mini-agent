#!/bin/bash
# mini-agent Loop - Autonomous AI coding loop
# Tailored for the mini-agent (minimalistic coding agent with built-in JS engine) project
#
# Usage:
#   ./scripts/loop.sh                  # Build mode - implement :READY: beads
#   ./scripts/loop.sh plan             # Plan mode - create tasks from specs (one-shot)
#   ./scripts/loop.sh decompose        # Decompose mode - drill specs into bead hierarchy (10 rounds)
#   ./scripts/loop.sh decompose 5      # Decompose with cap of 5 rounds
#   ./scripts/loop.sh review bugs      # Review: single domain
#   ./scripts/loop.sh review all       # Review: ALL domains in tiered sequence
#   ./scripts/loop.sh 50               # Build with max 50 iterations
#   ./scripts/loop.sh review security 3  # Review security with 3 passes
#   ./scripts/loop.sh codex                      # Build with codex exec (default codex model)
#   ./scripts/loop.sh codex decompose            # Decompose with codex exec
#   ./scripts/loop.sh --model o3                 # Build with codex exec, model o3
#   ./scripts/loop.sh --model claude-opus-4-8    # Build with claude, specific model
#   ./scripts/loop.sh --codex-verify             # Opt-in: Codex second-opinion after each build iteration
#   ./scripts/loop.sh review all --codex-verify  # Opt-in: Codex cross-checks during review
#   ./scripts/loop.sh decompose --codex-verify   # Opt-in: Codex QC review after each decompose round
#
# Executor selection: pass `codex` or use --model with a non-claude ID to route
# through `codex exec`; otherwise the claude CLI stream-json path is used.
# Codex second-opinion (--codex-verify) is OPT-IN and independent of the executor.
#
# Review domains (run individually or via 'all'):
#   Tier 1 (independent):  bugs, security, perf, orphans, missing, quality
#   Tier 2 (cross-cutting): arch, deps, compound
#   Tier 3 (QC):           debate, synthesis
#
# Decompose mode (without --codex-verify): exits on no-op round or two
#   consecutive <5% growth rounds.
# Decompose mode (with --codex-verify): additionally consults Codex QC, which
#   emits VERDICT: STOP | CONTINUE | CONTINUE_AFTER_FIXES — two consecutive
#   STOPs also exit.
#
# Prompt files (create alongside this script in scripts/):
#   scripts/PROMPT_build.md     - build mode agent instructions
#   scripts/PROMPT_plan.md      - plan mode agent instructions
#   scripts/PROMPT_decompose.md - decompose mode agent instructions
#   scripts/PROMPT_decompose_qc.md - Codex QC prompt (--codex-verify only)
#   scripts/PROMPT_review_<domain>.md - per-domain review prompts

set -e

# Safety: always unset ANTHROPIC_API_KEY so every claude invocation in this
# script (and any subprocess it spawns) uses the subscription, never API credits.
unset ANTHROPIC_API_KEY

# NOTE: `pipefail` is enabled only locally around the agent pipeline (see
# run_iteration). Enabling it globally breaks benign pipelines like `... | head -1`
# where the downstream consumer legitimately closes early (SIGPIPE → non-zero).

# Ensure Rust toolchain is on PATH (rustup installs to ~/.cargo/bin)
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
export PATH="$HOME/.cargo/bin:$PATH"

# Cap cargo parallelism so per-iteration verification doesn't saturate the box.
# Default = max(2, ncpu-2) — leaves headroom for the agent, MCP children,
# rust-analyzer, dolt, etc. Set CARGO_BUILD_JOBS in the env to override.
if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
    if command -v sysctl >/dev/null 2>&1; then
        _ncpu=$(sysctl -n hw.ncpu 2>/dev/null || echo 4)
    elif command -v nproc >/dev/null 2>&1; then
        _ncpu=$(nproc 2>/dev/null || echo 4)
    else
        _ncpu=4
    fi
    _jobs=$((_ncpu - 2))
    [ "$_jobs" -lt 2 ] && _jobs=2
    export CARGO_BUILD_JOBS="$_jobs"
    unset _ncpu _jobs
fi

# --- Single-instance guard ---
# Two concurrent loop.sh runs in the same repo are catastrophic: each spawns
# its own claude session (~900MB rust-analyzer + 5 MCP servers) and they
# contend on rust/target/ build locks. Lock keyed by the absolute repo path
# so different repos can still run loops.
LOOP_LOCK_KEY="$(echo "$PWD" | shasum | cut -c1-8)"
LOOP_LOCK_DIR="/tmp/mini-agent-loop-${LOOP_LOCK_KEY}.lock"
if ! mkdir "$LOOP_LOCK_DIR" 2>/dev/null; then
    existing_pid="$(cat "$LOOP_LOCK_DIR/pid" 2>/dev/null || echo '')"
    if [ -n "$existing_pid" ] && kill -0 "$existing_pid" 2>/dev/null; then
        echo "[loop] another loop.sh is already running for $PWD (pid $existing_pid)" >&2
        echo "[loop] refusing to start a second instance — stop the other one first" >&2
        exit 1
    fi
    echo "[loop] stale lock at $LOOP_LOCK_DIR (pid '$existing_pid' not alive) — clearing" >&2
    rm -rf "$LOOP_LOCK_DIR"
    mkdir "$LOOP_LOCK_DIR" || { echo "[loop] failed to claim lock" >&2; exit 1; }
fi
echo "$$" > "$LOOP_LOCK_DIR/pid"
# Cleanup on any exit, including SIGINT/SIGTERM (set -e catches errors too).
# Also flushes dolt — the per-iteration push is now batched (every N iters)
# to reduce GC churn, so the final flush guarantees nothing is left unsynced.
_loop_exit_cleanup() {
    local exit_status=$?
    # This handler exits explicitly to preserve the original status (or turn a
    # failed safety reopen into failure), so prevent recursive EXIT handling.
    trap - EXIT
    set +e

    if [ "${MODE:-}" = build ] \
            && [ -n "${PICKED_ID:-}" ] \
            && [ "${BUILD_ACCEPTANCE_ENFORCED:-true}" != true ]; then
        echo "[loop] exit during acceptance window — reopening ${PICKED_ID}" >&2
        if declare -F reopen_build_bead >/dev/null 2>&1 \
                && reopen_build_bead "$PICKED_ID" 1 unavailable; then
            BUILD_ACCEPTANCE_ENFORCED=true
        else
            echo "[loop] EXIT SAFETY FAILURE: could not confirm ${PICKED_ID} open" >&2
            [ "$exit_status" -ne 0 ] || exit_status=1
        fi
    fi

    if command -v bd >/dev/null 2>&1; then
        bd dolt push 2>/dev/null || true
    fi
    rm -rf "$LOOP_LOCK_DIR"
    exit "$exit_status"
}
trap _loop_exit_cleanup EXIT

# --- Configuration ---
DEFAULT_MAX_ITERATIONS=30
DEFAULT_DECOMPOSE_ROUNDS=10
# Diminishing-returns cutoff: round-over-round bead growth below this percentage
# (when accumulated for two rounds in a row) exits the decompose loop.
DECOMPOSE_LOW_GROWTH_PCT=5
AGENT_CMD="${AGENT_CMD:-claude}"
# Array form so paths with spaces survive word-splitting. Override via env:
#   CODEX_COMPANION_CMD=(node /path/with\ space/companion.mjs)
if [ -z "${CODEX_COMPANION_CMD+x}" ]; then
    CODEX_COMPANION_CMD=(node "$HOME/.claude/plugins/marketplaces/openai-codex/plugins/codex/scripts/codex-companion.mjs")
fi
CODEX_VERIFY="${CODEX_VERIFY:-false}"  # Set to true or pass --codex-verify to enable

# Wall-clock ceiling for a single agent invocation. Unblocks the loop when
# claude's post-stream SessionEnd hooks or a leaked stdout-holding MCP child
# stall the loop indefinitely after a successful "type":"result" frame.
# Override with AGENT_TIMEOUT_SECS=0 to disable.
AGENT_TIMEOUT_SECS="${AGENT_TIMEOUT_SECS:-3600}"

# Codex exec support. CODEX_HOME is a workspace-local copy of ~/.codex so
# codex exec can find auth without touching the global home.
# HARD_TIMEOUT is the wall-clock kill-switch for codex exec (separate from
# AGENT_TIMEOUT_SECS which governs the claude stream watchdog).
CODEX_HOME="${CODEX_HOME:-}"
HARD_TIMEOUT="${HARD_TIMEOUT:-$AGENT_TIMEOUT_SECS}"

# Stuck-loop detector state (mutated inside run_iteration as globals).
# Reset on successful iteration; incremented on the agent-failure path.
# When the same PICKED_ID fails STUCK_LOOP_THRESHOLD iterations in a row,
# the loop bails with a P0 bead so it doesn't burn budget on a wedged task.
CONSEC_FAILURES=0
LAST_FAILED_PICKED_ID=""
STUCK_LOOP_THRESHOLD="${STUCK_LOOP_THRESHOLD:-3}"
# Interrupts must reopen a selected build bead until its acceptance state has
# been explicitly enforced after the agent returns.
BUILD_ACCEPTANCE_ENFORCED=true

# Pick a `timeout` binary. macOS doesn't ship one; Homebrew coreutils provides
# either /usr/local/bin/timeout (alias) or gtimeout. If neither is installed,
# fall back to no timeout — preserves prior behavior on bare systems.
if command -v timeout >/dev/null 2>&1; then
    TIMEOUT_BIN="timeout"
elif command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT_BIN="gtimeout"
else
    TIMEOUT_BIN=""
fi

# Bead ID regex (POSIX ERE). Sub-beads use a dotted-numeric suffix
# (e.g. `mini-agent-dtk.5`); without the optional `(\.[0-9]+)*` group, `grep -o`
# strips the suffix and returns the parent ID, which then trips the
# "already closed" guard and stalls the loop. Used wherever we EXTRACT or
# match a full bead ID — not for substring-only presence checks like
# `grep -q 'mini-agent-'`, which work either way.
BEAD_ID_RE='mini-agent-[a-z0-9]+(\.[0-9]+)*'

# Review domain tiers — order matters: later tiers read beads from earlier ones
REVIEW_TIER1=(bugs security perf orphans missing quality)
REVIEW_TIER2=(arch deps compound)
REVIEW_TIER3=(debate synthesis)
REVIEW_ALL_DOMAINS=("${REVIEW_TIER1[@]}" "${REVIEW_TIER2[@]}" "${REVIEW_TIER3[@]}")

# --- Parse arguments ---
MODE="build"
REVIEW_DOMAIN=""
MAX_ITERATIONS=$DEFAULT_MAX_ITERATIONS
USER_SET_ITERATIONS=false

args=("$@")
VALID_DOMAINS_RE='^(bugs|security|perf|orphans|missing|quality|arch|deps|compound|debate|synthesis|all)$'
for i in "${!args[@]}"; do
    arg="${args[$i]}"
    case $arg in
        plan) MODE="plan" ;;
        decompose) MODE="decompose" ;;
        review)
            MODE="review"
            # Only accept the next token as a domain if it's actually a known
            # domain name — not a flag (--codex-verify) and not an iteration count.
            next="${args[$((i+1))]:-}"
            if [[ -n "$next" && "$next" =~ $VALID_DOMAINS_RE ]]; then
                REVIEW_DOMAIN="$next"
            fi
            ;;
        bugs|security|perf|orphans|missing|quality|arch|deps|compound|debate|synthesis|all)
            ;; # captured by review case
        --codex-verify) CODEX_VERIFY=true ;;
        codex) AGENT_EXECUTOR="codex" ;;
        --model)
            next="${args[$((i+1))]:-}"
            [ -n "$next" ] && AGENT_MODEL="$next"
            ;;
        *)
            # Strict integer match — "10foo" must not be accepted as 10.
            if [[ "$arg" =~ ^[0-9]+$ ]]; then
                MAX_ITERATIONS=$arg
                USER_SET_ITERATIONS=true
            fi
            ;;
    esac
done

# Build --model flag array for claude invocations (empty when not set)
AGENT_MODEL_ARGS=()
[ -n "${AGENT_MODEL:-}" ] && AGENT_MODEL_ARGS=(--model "$AGENT_MODEL")

# Executor resolution: explicit `codex` arg wins; otherwise auto-detect from
# model name — non-claude IDs (o3, o4-mini, gpt-4o, …) use codex exec,
# claude-* and the default use the claude CLI stream-json path.
if [ "${AGENT_EXECUTOR:-}" != "codex" ]; then
    if [ -n "${AGENT_MODEL:-}" ] && [[ ! "$AGENT_MODEL" =~ ^claude- ]]; then
        AGENT_EXECUTOR="codex"
    else
        AGENT_EXECUTOR="claude"
    fi
fi

# Review defaults to 1 iteration per domain
if [ "$MODE" = "review" ] && [ "$USER_SET_ITERATIONS" = false ]; then
    MAX_ITERATIONS=1
fi

# Decompose defaults to DEFAULT_DECOMPOSE_ROUNDS unless user-overridden
if [ "$MODE" = "decompose" ] && [ "$USER_SET_ITERATIONS" = false ]; then
    MAX_ITERATIONS=$DEFAULT_DECOMPOSE_ROUNDS
fi

# --- Colors ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

CURRENT_ITERATION=0
CURRENT_DOMAIN=""
USE_BEADS=false
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOTAL_REVIEW_FINDINGS=0

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Codex Exec Support                                              ║
# ╚══════════════════════════════════════════════════════════════════╝

# Claude -> Codex CLI mapping:
#   claude -p                          -> codex exec -
#   --dangerously-skip-permissions     -> --dangerously-bypass-approvals-and-sandbox
#   --output-format stream-json        -> -o "$temp_out" (file) + process exit status
#   cd "$dir" && claude ...            -> codex exec -C "$dir" ...
#   env -u ANTHROPIC_API_KEY           -> n/a; codex reads ~/.codex/auth.json

setup_codex_home() {
    local source_home="$HOME/.codex"

    if ! command -v codex >/dev/null 2>&1; then
        echo -e "${RED}Error: codex CLI not installed (required for non-claude models)${NC}" >&2
        exit 1
    fi

    if [ -z "$CODEX_HOME" ]; then
        CODEX_HOME="$PWD/.codex"
    fi
    export CODEX_HOME
    mkdir -p "$CODEX_HOME/sessions"

    if [ ! -f "$CODEX_HOME/.gitignore" ]; then
        printf '*\n!.gitignore\n' > "$CODEX_HOME/.gitignore"
    fi

    if [ -d "$source_home" ] && [ "$CODEX_HOME" != "$source_home" ]; then
        local f
        for f in auth.json config.toml AGENTS.md version.json installation_id; do
            [ -f "$source_home/$f" ] && [ ! -e "$CODEX_HOME/$f" ] && \
                cp "$source_home/$f" "$CODEX_HOME/$f" 2>/dev/null || true
        done
        local d
        for d in skills prompts rules; do
            [ -d "$source_home/$d" ] && [ ! -e "$CODEX_HOME/$d" ] && \
                { ln -s "$source_home/$d" "$CODEX_HOME/$d" 2>/dev/null || \
                  cp -R "$source_home/$d" "$CODEX_HOME/$d" 2>/dev/null || true; } || true
        done
    fi
    return 0
}

# show_codex_progress — reads codex --json JSONL from stdin, prints ONLY tool-call
# activity (shell commands, file edits, MCP/web-search calls) with wall-clock
# timestamps and the turn they belong to. Reasoning and agent prose are dropped so
# codex output doesn't bury the loop's own banners.
#
# DISPLAY ONLY: always returns 0. Codex's real exit code is captured via PIPESTATUS
# in run_with_codex_exec and is the authoritative success signal.
show_codex_progress() {
    local turn=0 tool_count=0 printed=" " have_jq=false
    command -v jq >/dev/null 2>&1 && have_jq=true

    while IFS= read -r line; do
        case "$line" in
            *'"type":"turn.started"'*)  turn=$((turn + 1)); continue ;;
            *'"type":"turn.completed"'*)
                if [ "$have_jq" = true ]; then
                    local toks
                    toks=$(printf '%s' "$line" \
                        | jq -r '.usage | "\(.input_tokens)in/\(.output_tokens)out"' \
                        2>/dev/null) || true
                    [ -n "$toks" ] && [ "$toks" != "null in/null out" ] \
                        && echo -e "  ${DIM}turn $turn done · ${toks} tok${NC}"
                fi
                continue ;;
            # Pre-gate: only pass lines that might contain actionable tool items.
            *'"command_execution"'*|*'"file_change"'*|*'"mcp_tool_call"'*|*'"web_search"'*) ;;
            *) continue ;;
        esac

        if [ "$have_jq" != true ]; then
            echo -e "  ${DIM}[$(date '+%H:%M:%S')] turn $turn${NC} ${CYAN}tool call${NC}"
            continue
        fi

        # One jq pass per actionable line → "<id>\t<itype>\t<summary>" or nothing.
        local parsed id itype summary
        parsed=$(printf '%s' "$line" | jq -r '
            (.item // empty) as $i
            | select($i.type == "command_execution" or $i.type == "file_change"
                     or $i.type == "mcp_tool_call" or $i.type == "web_search")
            | ( if $i.type == "command_execution" then
                    (if ($i.command | type) == "array" then ($i.command | join(" "))
                     else ($i.command // "") end)
                elif $i.type == "file_change" then
                    ([$i.changes[]? | "\(.kind) \(.path | split("/") | last)"] | join(", "))
                elif $i.type == "mcp_tool_call" then
                    (($i.server // "?") + "/" + ($i.tool // "?"))
                elif $i.type == "web_search" then
                    ($i.query // "")
                else "" end ) as $summary
            | [ ($i.id // "?"), $i.type, $summary ] | @tsv
        ' 2>/dev/null) || continue
        [ -n "$parsed" ] || continue

        IFS=$'\t' read -r id itype summary <<< "$parsed"
        # item.started and item.completed carry the same id — print once.
        case "$printed" in *" $id "*) continue ;; esac
        printed="$printed$id "
        tool_count=$((tool_count + 1))

        # Tidy the shell preamble and clamp to a single readable line.
        summary=${summary#/bin/zsh -lc }
        summary=${summary#/bin/bash -lc }
        summary=$(printf '%s' "$summary" | tr '\n' ' ')
        [ "${#summary}" -gt 100 ] && summary="${summary:0:99}…"

        echo -e "  ${DIM}[$(date '+%H:%M:%S')] turn $turn${NC} ${CYAN}${itype}${NC} ${summary}"
    done

    [ "$tool_count" -gt 0 ] && echo -e "  ${DIM}codex: ${tool_count} tool call(s)${NC}"
    return 0
}

# run_with_codex_exec <prompt_content> <temp_out> [model]
# Streams --json JSONL events through show_codex_progress (live tool-call display)
# while writing the final message to temp_out via -o. Codex's exit code is the
# authoritative success signal (captured via PIPESTATUS); show_codex_progress is
# display-only and always returns 0.
# Set MINI_LOOP_CODEX_RAW=1 to bypass the filter and see raw codex output.
run_with_codex_exec() {
    local prompt_content="$1"
    local temp_out="$2"
    local model="${3:-}"

    mkdir -p "$CODEX_HOME/sessions"

    local -a codex_cmd=(
        codex exec
        --dangerously-bypass-approvals-and-sandbox
        --skip-git-repo-check
        --ephemeral
        -C "$PWD"
        -o "$temp_out"
    )
    [ -n "$model" ] && codex_cmd+=(--model "$model")

    local agent_prefix=()
    if [ -n "$TIMEOUT_BIN" ] && [ "${HARD_TIMEOUT:-0}" -gt 0 ] 2>/dev/null; then
        agent_prefix=("$TIMEOUT_BIN" "--kill-after=30" "$HARD_TIMEOUT")
    fi

    if [ "${MINI_LOOP_CODEX_RAW:-0}" = "1" ]; then
        # Raw mode: stream codex's full human-readable output for debugging.
        printf '%s' "$prompt_content" | "${agent_prefix[@]}" "${codex_cmd[@]}"
        return $?
    fi

    # JSON mode: pipe through show_codex_progress (display-only, always returns 0).
    # Pipe stages: [0] printf (always 0) | [1] codex (real exit) | [2] show_codex_progress (0).
    # PIPESTATUS[1] is codex's real exit code.
    codex_cmd+=(--json -)
    printf '%s' "$prompt_content" \
        | "${agent_prefix[@]}" "${codex_cmd[@]}" \
        | show_codex_progress
    return "${PIPESTATUS[1]}"
}

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Helper Functions                                                ║
# ╚══════════════════════════════════════════════════════════════════╝

timestamp() { date '+%Y-%m-%d %H:%M:%S'; }

count_lines() {
    # Count bead-ID lines. Uses BEAD_ID_RE to avoid matching header rows
    # or title text that happens to contain the bead prefix.
    # grep -c prints "0" AND exits 1 on no matches, so `|| echo 0` would
    # append a second "0", producing "0\n0". head -1 keeps it single-line.
    { grep -cE "$BEAD_ID_RE" 2>/dev/null || echo 0; } | head -1
}

# Check whether a bead is in CLOSED state. Centralized so the display-format
# coupling (the "· CLOSED]" token in `bd show`'s first line) is in one place.
bead_is_closed() {
    local id="$1"
    [ -z "$id" ] && return 1
    bd show "$id" 2>/dev/null | head -1 | grep -qE '·[[:space:]]*CLOSED\]'
}

# Fail-closed state query for acceptance enforcement. Unlike bead_is_closed,
# this never treats a failed or ambiguous query as evidence that a bead is open.
bead_enforcement_status() {
    local id="$1" status
    if [ -z "$id" ] || ! command -v jq >/dev/null 2>&1; then
        echo unavailable
        return
    fi
    status=$(bd show "$id" --json 2>/dev/null \
        | jq -er 'if type == "array" then .[0].status else .status end | strings | ascii_downcase' \
            2>/dev/null) || status=""
    case "$status" in
        open) echo open ;;
        in_progress) echo in_progress ;;
        closed) echo closed ;;
        *) echo unavailable ;;
    esac
}

# Extract just the bead IDs from whatever bead-listing command is piped in.
# Used by review-mode delta accounting to diff actual ID sets, not raw counts.
extract_bead_ids() {
    grep -oE "$BEAD_ID_RE" | sort -u
}

# Labels that disqualify a bead from autonomous selection. The build loop must
# never pick a bead carrying any of these — they signal "human-driven work" or
# "gated until further notice".
BEAD_LABEL_BLOCKLIST=("no-auto-loop" "manual-gate")

# Build a jq expression that drops any bead carrying a blocklisted label.
# Centralised so the in_progress filter and the ready filter agree.
_jq_label_filter() {
    local labels_json
    labels_json=$(printf '%s\n' "${BEAD_LABEL_BLOCKLIST[@]}" \
        | jq -R . | jq -sc .)
    cat <<JQ
        . as \$bead
        | (\$bead.labels // []) as \$labels
        | ($labels_json) as \$blocked
        | select(([\$labels[] | IN(\$blocked[])] | any) | not)
JQ
}

# Fallback grep alternation built from BEAD_LABEL_BLOCKLIST, used when jq
# isn't installed. Keeps the no-jq path in lockstep with the jq path.
_blocklist_grep_alt() {
    local IFS='|'
    printf '%s' "${BEAD_LABEL_BLOCKLIST[*]}"
}

# Decide whether an epic should be a valid pick. An epic with at least one
# in_progress descendant is real work-in-flight; one with no started/in_progress
# sub-work is purely organizational and must be skipped.
_epic_has_in_progress_subwork() {
    local epic_id="$1"
    [ -z "$epic_id" ] && return 1
    if command -v jq >/dev/null 2>&1; then
        local n
        n=$(bd --json list --parent "$epic_id" --status in_progress 2>/dev/null \
            | jq 'length' 2>/dev/null || echo 0)
        [ "${n:-0}" -gt 0 ] 2>/dev/null
    else
        local n
        n=$( (bd list --parent "$epic_id" --status in_progress 2>/dev/null || true) \
            | count_lines)
        [ "${n:-0}" -gt 0 ] 2>/dev/null
    fi
}

# pick_ready_bead — sets PICKED_ID and PICKED_TITLE for the build loop.
# Selection rules:
#   1. Resume any single in_progress bead first (modulo label blocklist).
#   2. Otherwise pick the first `bd ready` candidate that:
#        - does NOT carry any BEAD_LABEL_BLOCKLIST label;
#        - is NOT a type=epic with zero in_progress sub-work.
# Empty PICKED_ID means caller should treat the round as "no pickable work".
pick_ready_bead() {
    PICKED_ID=""
    PICKED_TITLE=""

    if command -v jq >/dev/null 2>&1; then
        local label_filter
        label_filter=$(_jq_label_filter)

        # 1) In-progress first.
        local picked_json
        picked_json=$(bd --json list --limit 0 --status in_progress 2>/dev/null \
            | jq -c "[.[] | $label_filter] | .[0] // empty" 2>/dev/null || true)
        if [ -n "$picked_json" ] && [ "$picked_json" != "null" ]; then
            local picked_id
            picked_id=$(printf '%s' "$picked_json" | jq -r '.id // empty' 2>/dev/null)
            PICKED_TITLE=$(printf '%s' "$picked_json" | jq -r '.title // empty' 2>/dev/null)
            if [ -n "$picked_id" ]; then
                BUILD_ACCEPTANCE_ENFORCED=false
                PICKED_ID="$picked_id"
                return 0
            fi
        fi

        # 2) Ready, with label + epic filters.
        local ready_json
        ready_json=$(bd --json ready 2>/dev/null || echo '[]')
        local candidates
        candidates=$(printf '%s' "$ready_json" \
            | jq -c "[.[] | $label_filter] | .[] | {id, title, issue_type}" \
            2>/dev/null || true)

        local cand id itype title
        while IFS= read -r cand; do
            [ -z "$cand" ] && continue
            id=$(printf '%s' "$cand" | jq -r '.id // empty')
            itype=$(printf '%s' "$cand" | jq -r '.issue_type // empty')
            title=$(printf '%s' "$cand" | jq -r '.title // empty')
            [ -z "$id" ] && continue
            if [ "$itype" = "epic" ] && ! _epic_has_in_progress_subwork "$id"; then
                continue
            fi
            BUILD_ACCEPTANCE_ENFORCED=false
            PICKED_ID="$id"
            PICKED_TITLE="$title"
            return 0
        done <<< "$candidates"

        return 1
    fi

    # ─── jq missing: degraded text-mode fallback ─────────────────────────
    local blocklist_alt
    blocklist_alt=$(_blocklist_grep_alt)

    local picked_line
    picked_line=$( (bd list --limit 0 --status in_progress 2>/dev/null || true) \
        | grep -E "$BEAD_ID_RE" | head -1 || true)

    if [ -z "$picked_line" ]; then
        local ready_output
        ready_output=$(bd ready --exclude-type=epic 2>/dev/null || bd ready 2>/dev/null || true)
        picked_line=$(printf '%s\n' "$ready_output" \
            | grep -E "$BEAD_ID_RE" \
            | grep -viE "$blocklist_alt" \
            | head -1 || true)
    fi

    [ -z "$picked_line" ] && return 1
    local picked_id
    picked_id=$(echo "$picked_line" | grep -oE "$BEAD_ID_RE" | head -1 || true)
    [ -z "$picked_id" ] && return 1
    BUILD_ACCEPTANCE_ENFORCED=false
    PICKED_ID="$picked_id"
    PICKED_TITLE=$(echo "$picked_line" | sed 's/^.*] - //' | sed 's/^.*] //')
    return 0
}

: "${BEADS_PREVIEW:=8}"  # how many ready beads to preview per banner

print_open_beads() {
    if [ "$USE_BEADS" != true ]; then return; fi

    local total_open ready in_progress blocked closed
    total_open=$( (bd list --limit 0 --status open 2>/dev/null || true) | count_lines)
    ready=$( (bd ready 2>/dev/null || true) | count_lines)
    in_progress=$( (bd list --limit 0 --status in_progress 2>/dev/null || true) | count_lines)
    blocked=$( (bd blocked 2>/dev/null || true) | count_lines)
    closed=$( (bd list --limit 0 --status closed 2>/dev/null || true) | count_lines)

    echo ""
    echo -e "${BOLD}┌──────────────────────────────────────────────────────────────────┐${NC}"
    printf "${BOLD}│${NC} ${DIM}%s${NC}  Open: ${YELLOW}%-4s${NC} Ready: ${GREEN}%-4s${NC} Active: ${CYAN}%-4s${NC} Blocked: ${RED}%-4s${NC} Closed: ${DIM}%-4s${NC}${BOLD}│${NC}\n" \
        "$(timestamp)" "$total_open" "$ready" "$in_progress" "$blocked" "$closed"

    local shown=0
    if [ "$ready" != "0" ]; then
        echo -e "${BOLD}├──────────────────────────────────────────────────────────────────┤${NC}"
        echo -e "${BOLD}│${NC}  ${GREEN}Next ready (top $BEADS_PREVIEW):${NC}"
        while IFS= read -r line; do
            [ "$shown" -ge "$BEADS_PREVIEW" ] && break
            echo "$line" | grep -q 'mini-agent-' || continue
            echo -e "${BOLD}│${NC}    $line"
            shown=$((shown + 1))
        done < <(bd ready 2>/dev/null || true)
        if [ "$ready" -gt "$BEADS_PREVIEW" ] 2>/dev/null; then
            echo -e "${BOLD}│${NC}    ${DIM}… $((ready - BEADS_PREVIEW)) more — \`bd ready\` to see all${NC}"
        fi
    fi

    if [ "$in_progress" != "0" ]; then
        echo -e "${BOLD}├──────────────────────────────────────────────────────────────────┤${NC}"
        echo -e "${BOLD}│${NC}  ${CYAN}In progress:${NC}"
        (bd list --limit 0 --status in_progress 2>/dev/null || true) | while IFS= read -r line; do
            echo "$line" | grep -q 'mini-agent-' && echo -e "${BOLD}│${NC}    ${CYAN}▶${NC} $line" || true
        done
    fi

    echo -e "${BOLD}└──────────────────────────────────────────────────────────────────┘${NC}"
}

print_picking_up() {
    local bead_id="$1" bead_title="$2"
    echo ""
    echo -e "${GREEN}╔══════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║  PICKING UP: ${BOLD}$bead_id${NC}${GREEN}                                        ║${NC}"
    echo -e "${GREEN}║  $bead_title${NC}"
    echo -e "${GREEN}╚══════════════════════════════════════════════════════════════════╝${NC}"
    echo ""
}

print_completed() {
    local bead_id="$1" bead_title="$2" status="$3"
    echo ""
    if [ "$status" = "closed" ]; then
        echo -e "${GREEN}┌──────────────────────────────────────────────────────────────────┐${NC}"
        echo -e "${GREEN}│  COMPLETED: ${BOLD}$bead_id${NC}${GREEN}  at $(timestamp)              │${NC}"
        echo -e "${GREEN}│  $bead_title${NC}"
        echo -e "${GREEN}└──────────────────────────────────────────────────────────────────┘${NC}"
    else
        echo -e "${YELLOW}┌──────────────────────────────────────────────────────────────────┐${NC}"
        echo -e "${YELLOW}│  PARTIAL: ${BOLD}$bead_id${NC}${YELLOW}  at $(timestamp)                │${NC}"
        echo -e "${YELLOW}│  $bead_title${NC}"
        echo -e "${YELLOW}│  Still in_progress — will resume next iteration                 │${NC}"
        echo -e "${YELLOW}└──────────────────────────────────────────────────────────────────┘${NC}"
    fi
    echo ""
    local changed_files
    changed_files=$(git diff --stat HEAD~1 2>/dev/null || echo "(no commit to diff)")
    echo -e "${DIM}  Files changed this iteration:${NC}"
    echo "$changed_files" | while IFS= read -r line; do echo -e "    ${DIM}$line${NC}"; done
    echo ""
}

# Ad-hoc sign newly-built test binaries so that taskgated/AMFI can cache
# the exec assessment by cdhash. On macOS (Tahoe) Intel, an unsigned Rust
# test binary pays 35-42s of taskgated overhead per `exec`. Ad-hoc signing
# seeds the kernel's assessment cache; warm execs drop to 0.00s.
sign_test_binaries() {
    local cargo_dir="$1" bins_file="$2"
    local stamp="$cargo_dir/target/.loop-codesign-stamp"
    local tmp="$stamp.tmp.$$"

    command -v codesign >/dev/null 2>&1 || return 0
    [ -s "$bins_file" ] || return 0

    : > "$tmp"
    local signed=0 reused=0 failed=0
    local bin cur prev
    while IFS= read -r bin; do
        [ -n "$bin" ] || continue
        [ -x "$bin" ] || continue
        cur=$(/usr/bin/shasum -a 256 "$bin" 2>/dev/null | cut -c1-16)
        [ -z "$cur" ] && continue
        if [ -f "$stamp" ]; then
            prev=$(grep -F -- "${bin}"$'\t' "$stamp" 2>/dev/null | head -1 | cut -f2)
        else
            prev=""
        fi
        if [ "$prev" = "$cur" ]; then
            reused=$((reused + 1))
        else
            if codesign -s - --force "$bin" 2>/dev/null; then
                cur=$(/usr/bin/shasum -a 256 "$bin" 2>/dev/null | cut -c1-16)
                signed=$((signed + 1))
            else
                failed=$((failed + 1))
                printf '[loop] codesign failed: %s\n' "$bin" >&2
            fi
        fi
        printf '%s\t%s\n' "$bin" "$cur" >> "$tmp"
    done < "$bins_file"

    mv "$tmp" "$stamp" 2>/dev/null || rm -f "$tmp"
    echo -e "${BOLD}│${NC}  ${DIM}codesign: signed=${signed} reused=${reused} failed=${failed}${NC}"
}

report_verification_failure() {
    local errors="$1" source_bead="${PICKED_ID:-unknown}"
    [ ${#errors} -gt 3000 ] && errors="[truncated]..${errors: -3000}"

    if bd create --title="Fix build errors from iteration $CURRENT_ITERATION ($source_bead)" \
            --type=bug --priority=0 \
            --description="Post-agent verification failed. Fix before any other work.$'\n\n'$errors" \
            >/dev/null 2>&1; then
        echo -e "${RED}  Filed P0 bug bead — next iteration will pick it up${NC}"
    else
        echo -e "${RED}  Failed to file P0 verification bead; original build bead will remain open${NC}" >&2
    fi
}

run_verification() {
    local range_base="${1:-}" range_head="${2:-}" profile="${3:-headless}" surfaces="${4:-rust}"
    [ "$profile" = packaged-artifact ] && [ "$surfaces" = rust ] && surfaces="rust,packaging"
    # Only verify in build mode — review/plan don't write code
    if [ "$MODE" != "build" ]; then return 0; fi

    # Callers should provide the whole iteration range. Retain the historical
    # one-commit range only for standalone/legacy calls.
    if [ -z "$range_base" ] || [ -z "$range_head" ]; then
        range_base="HEAD~1"
        range_head="HEAD"
    fi

    # mini-agent: Cargo.toml is at repo root (no rust/ subdirectory)
    # real_failure=1 means an actual build/test/syntax error — triggers a new P0.
    # failed=1 but real_failure=0 means "no relevant code changed" — reopen PICKED
    # bead silently; do NOT spawn a new child P0 (that causes the runaway chain).
    local failed=0 real_failure=0 errors="" cargo_dir="."
    if [ ! -f "Cargo.toml" ]; then
        echo -e "${RED}FAIL: verification requires root Cargo.toml${NC}" >&2
        return 1
    fi

    # Ensure cargo is on PATH — try multiple sources
    if ! command -v cargo &>/dev/null; then
        [ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
    local CARGO
    CARGO=$(command -v cargo 2>/dev/null || echo "$HOME/.cargo/bin/cargo")
    if [ ! -x "$CARGO" ]; then
        echo -e "${RED}FAIL: verification requires an executable cargo command${NC}" >&2
        return 1
    fi

    echo ""
    echo -e "${BOLD}┌──────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${BOLD}│  POST-AGENT VERIFICATION                                         │${NC}"
    echo -e "${BOLD}├──────────────────────────────────────────────────────────────────┤${NC}"

    # Every accepted implementation range gets syntax checks plus the complete
    # Cargo verification suite, including non-Rust packaging/script changes.
    local changed_files="" diff_check_output="" relevant_files=0
    if ! git rev-parse "$range_base" "$range_head" >/dev/null 2>&1 \
            || ! changed_files=$(git diff --name-only "$range_base" "$range_head" 2>/dev/null); then
        failed=1; real_failure=1
        errors+="=== verification range ===$'\n'Unable to inspect $range_base..$range_head$'\n\n'"
    elif [ -z "$changed_files" ]; then
        failed=1
        # No files at all were committed — not a build error, just nothing changed.
        errors+="=== verification range ===$'\n'No files changed in $range_base..$range_head$'\n\n'"
    fi

    echo -e "${BOLD}│${NC}  ${CYAN}git diff --check...${NC}"
    if diff_check_output=$(git diff --check "$range_base" "$range_head" 2>&1); then
        echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} git diff --check"
    else
        failed=1; real_failure=1
        echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} git diff --check"
        errors+="=== diff errors ===$'\n'${diff_check_output}$'\n\n'"
    fi

    local changed_file syntax_output first_line relevant shell_file
    while IFS= read -r changed_file; do
        [ -n "$changed_file" ] && [ -f "$changed_file" ] || continue
        relevant=false
        if path_is_relevant_for_profile "$changed_file" "$profile" "$surfaces"; then
            relevant=true
            relevant_files=$((relevant_files + 1))
        fi

        shell_file=false
        case "$changed_file" in
            *.sh) shell_file=true ;;
            *)
                IFS= read -r first_line < "$changed_file" || first_line=""
                [[ "$first_line" =~ ^'#!'.*(bash|/sh|zsh|ksh) ]] && shell_file=true
                ;;
        esac
        if [ "$shell_file" = true ]; then
            echo -e "${BOLD}│${NC}  ${CYAN}bash -n ${changed_file}...${NC}"
            if syntax_output=$(bash -n "$changed_file" 2>&1); then
                echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} bash -n ${changed_file}"
            else
                failed=1; real_failure=1
                echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} bash -n ${changed_file}"
                errors+="=== shell syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
            fi
            continue
        fi

        case "$changed_file" in
            *.rs|*.toml|Cargo.lock)
                ;;
            *.json)
                if ! command -v jq >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== JSON verification unavailable ===$'\n'jq is required for ${changed_file}$'\n\n'"
                elif ! syntax_output=$(jq empty "$changed_file" 2>&1); then
                    failed=1; real_failure=1
                    errors+="=== JSON syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                fi
                ;;
            *.yaml|*.yml)
                if command -v ruby >/dev/null 2>&1; then
                    syntax_output=$(ruby -e 'require "psych"; Psych.parse_file(ARGV.fetch(0))' "$changed_file" 2>&1) || {
                        failed=1; real_failure=1
                        errors+="=== YAML syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                    }
                elif command -v python3 >/dev/null 2>&1 \
                        && python3 -c 'import yaml' >/dev/null 2>&1; then
                    syntax_output=$(python3 -c 'import sys, yaml; yaml.safe_load(open(sys.argv[1], encoding="utf-8"))' "$changed_file" 2>&1) || {
                        failed=1; real_failure=1
                        errors+="=== YAML syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                    }
                else
                    failed=1; real_failure=1
                    errors+="=== YAML verification unavailable ===$'\n'No Ruby Psych or Python PyYAML for ${changed_file}$'\n\n'"
                fi
                ;;
            *.js|*.mjs|*.cjs)
                if ! command -v node >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== JavaScript verification unavailable ===$'\n'node is required for ${changed_file}$'\n\n'"
                elif ! syntax_output=$(node --check "$changed_file" 2>&1); then
                    failed=1; real_failure=1
                    errors+="=== JavaScript syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                fi
                ;;
            *.rb)
                if ! command -v ruby >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== Ruby verification unavailable ===$'\n'ruby is required for ${changed_file}$'\n\n'"
                elif ! syntax_output=$(ruby -c "$changed_file" 2>&1); then
                    failed=1; real_failure=1
                    errors+="=== Ruby syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                fi
                ;;
            *.py)
                if ! command -v python3 >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== Python verification unavailable ===$'\n'python3 is required for ${changed_file}$'\n\n'"
                else
                    mkdir -p target/.loop-pycache
                    if ! syntax_output=$(PYTHONPYCACHEPREFIX="$PWD/target/.loop-pycache" python3 -m py_compile "$changed_file" 2>&1); then
                        failed=1; real_failure=1
                        errors+="=== Python syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                    fi
                fi
                ;;
            *.ps1)
                if ! command -v pwsh >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== PowerShell verification unavailable ===$'\n'pwsh is required for ${changed_file}$'\n\n'"
                elif ! syntax_output=$(pwsh -NoProfile -Command '$errors = $null; [void][System.Management.Automation.Language.Parser]::ParseFile($args[0], [ref]$null, [ref]$errors); if ($errors) { $errors | Out-String | Write-Error; exit 1 }' "$changed_file" 2>&1); then
                    failed=1; real_failure=1
                    errors+="=== PowerShell syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                fi
                ;;
            *.nix)
                if ! command -v nix-instantiate >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== Nix verification unavailable ===$'\n'nix-instantiate is required for ${changed_file}$'\n\n'"
                elif ! syntax_output=$(nix-instantiate --parse "$changed_file" 2>&1); then
                    failed=1; real_failure=1
                    errors+="=== Nix syntax errors (${changed_file}) ===$'\n'${syntax_output}$'\n\n'"
                fi
                ;;
            justfile)
                if ! command -v just >/dev/null 2>&1; then
                    failed=1; real_failure=1
                    errors+="=== justfile verification unavailable ===$'\n'just is required for ${changed_file}$'\n\n'"
                elif ! syntax_output=$(just --summary --justfile "$changed_file" 2>&1); then
                    failed=1; real_failure=1
                    errors+="=== justfile syntax errors ===$'\n'${syntax_output}$'\n\n'"
                fi
                ;;
            *)
                if [ "$relevant" = true ]; then
                    failed=1; real_failure=1
                    errors+="=== verifier missing ===$'\n'No automated checker is configured for relevant file ${changed_file} (profile=${profile})$'\n\n'"
                fi
                ;;
        esac
    done <<< "$changed_files"

    if [ "$relevant_files" -eq 0 ]; then
        failed=1
        # Do NOT set real_failure=1: "no relevant code changed" is not a build
        # error. The PICKED bead will be reopened for retry but no new child P0
        # is spawned — that was the source of the runaway "fix build errors" chain.
        errors+="=== relevance allowlist ===$'\n'No ${profile} production implementation file changed for declared surfaces: ${surfaces}$'\n\n'"
    fi

    # mini-agent workspace: root package (mini-agent) and spike/.
    # Determine which packages were touched so we scope clippy/test tightly.
    local existing_crates=""
    if git diff --name-only "$range_base" "$range_head" 2>/dev/null \
            | grep -vE '^spike/' | grep -qE '\.(rs|toml)$|^Cargo\.lock$'; then
        existing_crates+="mini-agent"$'\n'
    fi
    if git diff --name-only "$range_base" "$range_head" 2>/dev/null \
            | grep -qE '^spike/.*\.(rs|toml)$'; then
        if [ -f "spike/Cargo.toml" ]; then
            existing_crates+="spike"$'\n'
        fi
    fi
    existing_crates=$(printf '%s' "$existing_crates" | sed '/^$/d')

    local fmt_output clippy_output test_output

    # Auto-fix cosmetic churn before verify — avoids spurious P0 bug beads for
    # trivial rustfmt/clippy-fixable style violations.
    # Opt out via LOOP_VERIFY_NO_AUTOFIX=1.
    if [ "${LOOP_VERIFY_NO_AUTOFIX:-0}" != "1" ]; then
        echo -e "${BOLD}│${NC}  ${DIM}auto-fix: cargo fmt${NC}"
        (cd "$cargo_dir" && "$CARGO" fmt 2>/dev/null) || true
        if [ -n "$existing_crates" ]; then
            local -a fix_args=("clippy" "--fix" "--allow-dirty" "--allow-staged")
            local c
            for c in $existing_crates; do fix_args+=("-p" "$c"); done
            fix_args+=("--all-targets")
            echo -e "${BOLD}│${NC}  ${DIM}auto-fix: cargo clippy --fix -p $(echo $existing_crates | tr '\n' ' ')${NC}"
            (cd "$cargo_dir" && "$CARGO" "${fix_args[@]}" 2>/dev/null) || true
        else
            echo -e "${BOLD}│${NC}  ${DIM}auto-fix: cargo clippy --fix --workspace${NC}"
            (cd "$cargo_dir" && "$CARGO" clippy --fix --allow-dirty --allow-staged --workspace 2>/dev/null) || true
        fi
        (cd "$cargo_dir" && "$CARGO" fmt 2>/dev/null) || true
    fi

    echo -e "${BOLD}│${NC}  ${CYAN}cargo fmt --check...${NC}"
    if fmt_output=$(cd "$cargo_dir" && "$CARGO" fmt --check 2>&1); then
        echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} cargo fmt --check"
    else
        failed=1; real_failure=1; echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} cargo fmt --check"
        errors+="=== cargo fmt errors ===$'\n'${fmt_output}$'\n\n'"
    fi

    # Scope clippy to touched crates when possible; fall back to workspace.
    local clippy_cmd_label
    local -a clippy_cmd_args
    clippy_cmd_args=("clippy")
    if [ -n "$existing_crates" ]; then
        local c
        for c in $existing_crates; do clippy_cmd_args+=("-p" "$c"); done
        clippy_cmd_args+=("--all-targets" "--" "-D" "warnings")
        clippy_cmd_label="cargo clippy --all-targets -p $(echo $existing_crates | tr '\n' ' ')"
    else
        clippy_cmd_args+=("--workspace" "--" "-D" "warnings")
        clippy_cmd_label="cargo clippy --workspace"
    fi

    echo -e "${BOLD}│${NC}  ${CYAN}${clippy_cmd_label}...${NC}"
    if clippy_output=$(cd "$cargo_dir" && "$CARGO" "${clippy_cmd_args[@]}" 2>&1); then
        echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} ${clippy_cmd_label}"
    else
        failed=1; real_failure=1; echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} ${clippy_cmd_label}"
        errors+="=== cargo clippy errors ===$'\n'${clippy_output}$'\n\n'"
    fi

    # Every accepted Rust iteration executes all unit and integration tests for
    # the affected package. Acceptance never uses a type-check-only tier.
    # Both workspace packages are binary-only, so requesting --lib makes Cargo
    # fail with "no library targets found" before it can build any tests.
    local test_tier="full"
    if [ "${LOOP_TEST_TIER:-full}" != full ]; then
        echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} LOOP_TEST_TIER may not skip integration tests"
        failed=1; real_failure=1
        errors+="=== test policy ===$'\n'Every Rust acceptance requires the full test tier$'\n\n'"
    else
        local test_log="$cargo_dir/target/.loop-test.log"
        mkdir -p "$(dirname "$test_log")"
        : > "$test_log"

        local test_cmd_label
        local -a test_cmd_args
        case "$test_tier" in
            libbins) test_cmd_args=("test" "--bins") ;;
            full)    test_cmd_args=("test" "--bins" "--tests") ;;
        esac
        if [ -n "$existing_crates" ]; then
            local c
            for c in $existing_crates; do test_cmd_args+=("-p" "$c"); done
            local _crates_joined
            _crates_joined=$(echo $existing_crates | tr '\n' ' ')
            case "$test_tier" in
                libbins) test_cmd_label="cargo test --bins -p $_crates_joined" ;;
                full)    test_cmd_label="cargo test --bins --tests -p $_crates_joined" ;;
            esac
        else
            test_cmd_args+=("--workspace")
            case "$test_tier" in
                libbins) test_cmd_label="cargo test --workspace --bins" ;;
                full)    test_cmd_label="cargo test --workspace --bins --tests" ;;
            esac
        fi
        echo -e "${BOLD}│${NC}  ${DIM}test tier: ${test_tier} (iter ${CURRENT_ITERATION})${NC}"

        # Phase 1: cargo --no-run, capture test binary paths from JSON output.
        local bins_file="$cargo_dir/target/.loop-test-bins.txt"
        local norun_log="$cargo_dir/target/.loop-test-norun.log"
        : > "$bins_file"
        : > "$norun_log"

        echo -e "${BOLD}│${NC}  ${CYAN}${test_cmd_label} --no-run (build only)...${NC}"
        local _have_bins=false
        if command -v jq >/dev/null 2>&1; then
            if (cd "$cargo_dir" && "$CARGO" "${test_cmd_args[@]}" --no-run --message-format=json 2>"target/.loop-test-norun.log" \
                  | jq -r 'select(.profile?.test == true) | .executable // empty' \
                  > "target/.loop-test-bins.txt"; exit "${PIPESTATUS[0]}"); then
                echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} build only ($(wc -l < "$bins_file" | tr -d ' ') test binaries)"
                _have_bins=true
            else
                failed=1; real_failure=1; echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} ${test_cmd_label} --no-run"
                test_output=$(cat "$norun_log" 2>/dev/null || echo "(norun log missing)")
                errors+="=== test build errors (${test_cmd_label} --no-run) ===$'\n'${test_output}$'\n\n'"
            fi
        else
            echo -e "${BOLD}│${NC}  ${YELLOW}SKIP${NC} jq missing — cache disabled, falling back to cargo test"
            if (cd "$cargo_dir" && "$CARGO" "${test_cmd_args[@]}" 2>&1 | tee "target/.loop-test.log" >&2; exit "${PIPESTATUS[0]}"); then
                echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} ${test_cmd_label}"
            else
                failed=1; real_failure=1; echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} ${test_cmd_label}"
                test_output=$(cat "$test_log" 2>/dev/null || echo "(test log missing)")
                errors+="=== test errors (${test_cmd_label}) ===$'\n'${test_output}$'\n\n'"
            fi
        fi

        # Phase 1.5: enumerate every built test binary for execution.
        local pass_cache="$cargo_dir/target/.loop-test-pass-cache"
        # Execute every freshly built test binary. Hash caching previously let
        # runtime regressions escape acceptance when an integration binary was
        # unchanged or a cached test became flaky.
        local cache_ttl=0
        local bins_to_run="$cargo_dir/target/.loop-test-bins-to-run.txt"
        local cached=0 to_run_count=0
        local now_ts
        now_ts=$(date +%s)

        if [ "$failed" = "0" ] && [ "$_have_bins" = true ]; then
            : > "$bins_to_run"

            local fixtures_touched=false
            if git diff --name-only "$range_base" "$range_head" 2>/dev/null \
                | grep -qE '/tests/(fixtures|data)/|/testdata/|\.snap$'; then
                fixtures_touched=true
                : > "$pass_cache"
            fi

            local _bin _cur _cached_line _cached_ts
            while IFS= read -r _bin; do
                [ -x "$_bin" ] || continue
                _cur=$(/usr/bin/shasum -a 256 "$_bin" 2>/dev/null | cut -c1-16)
                if [ -z "$_cur" ]; then
                    printf '%s\n' "$_bin" >> "$bins_to_run"
                    to_run_count=$((to_run_count + 1))
                    continue
                fi
                _cached_line=$(grep -F -- "$_bin"$'\t'"$_cur"$'\t' "$pass_cache" 2>/dev/null | head -1)
                if [ "$cache_ttl" -gt 0 ] && [ -n "$_cached_line" ]; then
                    _cached_ts=$(printf '%s' "$_cached_line" | cut -f4)
                    if [ -n "$_cached_ts" ] && [ $((now_ts - _cached_ts)) -lt "$cache_ttl" ]; then
                        cached=$((cached + 1))
                        continue
                    fi
                fi
                printf '%s\n' "$_bin" >> "$bins_to_run"
                to_run_count=$((to_run_count + 1))
            done < "$bins_file"

            local _total=$((cached + to_run_count))
            local _cache_note=""
            [ "$fixtures_touched" = true ] && _cache_note=" (fixtures touched — cache busted)"
            echo -e "${BOLD}│${NC}  ${DIM}cache: ${cached}/${_total} cached, ${to_run_count} to run${_cache_note}${NC}"
        fi

        # Phase 2: sign only the binaries we'll exec.
        if [ "$failed" = "0" ] && [ -s "$bins_to_run" ]; then
            sign_test_binaries "$cargo_dir" "$bins_to_run"
        fi

        # Phase 3: exec uncached binaries directly.
        if [ "$failed" = "0" ] && [ -s "$bins_to_run" ]; then
            echo -e "${BOLD}│${NC}  ${CYAN}exec uncached binaries (streaming to ${test_log})...${NC}"
            local pass_tmp="$pass_cache.tmp.$$"
            if [ -f "$pass_cache" ]; then
                awk -v BTR="$bins_to_run" '
                    BEGIN { while ((getline l < BTR) > 0) skip[l] = 1 }
                    !($1 in skip)
                ' "$pass_cache" > "$pass_tmp" 2>/dev/null || : > "$pass_tmp"
            else
                : > "$pass_tmp"
            fi
            local _bin _cur _bin_ok
            while IFS= read -r _bin; do
                [ -x "$_bin" ] || continue
                _cur=$(/usr/bin/shasum -a 256 "$_bin" 2>/dev/null | cut -c1-16)
                _bin_ok=true
                (
                    set -o pipefail
                    "$_bin" --color=always 2>&1 | tee -a "$test_log" >&2
                ) || _bin_ok=false
                if [ "$_bin_ok" = true ]; then
                    printf '%s\t%s\tPASS\t%s\n' "$_bin" "$_cur" "$now_ts" >> "$pass_tmp"
                else
                    failed=1; real_failure=1
                fi
            done < "$bins_to_run"
            mv "$pass_tmp" "$pass_cache"
            if [ "$failed" = "0" ]; then
                echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} ${to_run_count} binary exec (${cached} cache hits)"
            else
                echo -e "${BOLD}│${NC}  ${RED}FAIL${NC} test exec (${cached} cache hits before failure)"
                test_output=$(cat "$test_log" 2>/dev/null || echo "(test log missing)")
                errors+="=== test errors (${test_cmd_label}) ===$'\n'${test_output}$'\n\n'"
            fi
        elif [ "$failed" = "0" ] && [ "$_have_bins" = true ]; then
            echo -e "${BOLD}│${NC}  ${GREEN}PASS${NC} all ${cached} binaries cached — nothing to exec"
        fi
    fi

    if [ "$failed" = "0" ]; then
        echo -e "${BOLD}│${NC}  ${GREEN}All checks passed${NC}"
        echo -e "${BOLD}└──────────────────────────────────────────────────────────────────┘${NC}"
        return 0
    fi

    if [ "$real_failure" = "1" ]; then
        echo -e "${BOLD}│${NC}  ${RED}Verification FAILED — filing P0 bug bead${NC}"
        echo -e "${BOLD}└──────────────────────────────────────────────────────────────────┘${NC}"
        report_verification_failure "$errors"
    else
        # Only "no relevant code changed" — do NOT spawn a child P0.  The
        # PICKED bead will be reopened so the agent can try again without a
        # runaway "fix build errors" chain clogging the queue.
        echo -e "${BOLD}│${NC}  ${YELLOW}No relevant code changed — reopening PICKED bead (no P0 filed)${NC}"
        echo -e "${BOLD}└──────────────────────────────────────────────────────────────────┘${NC}"
    fi
    return 1
}

show_agent_progress() {
    local tool_count=0 start_time result_status=missing
    start_time=$(date +%s)

    while IFS= read -r line; do
        case "$line" in
            *'"tool_use"'*)
                local tool_name
                tool_name=$(echo "$line" | grep -o '"name":"[^"]*"' | head -1 | sed 's/"name":"//;s/"//') || true
                if [ -n "$tool_name" ]; then
                    tool_count=$((tool_count + 1))
                    local elapsed=$(( $(date +%s) - start_time ))
                    echo -e "  ${DIM}[$(printf '%02d:%02d' $((elapsed/60)) $((elapsed%60)))] #${tool_count} ${tool_name}${NC}"
                fi
                ;;
            *'"type":"result"'*)
                local elapsed=$(( $(date +%s) - start_time ))
                if command -v jq >/dev/null 2>&1 \
                        && printf '%s\n' "$line" \
                            | jq -e '.type == "result"
                                    and ((.is_error // false) == false)
                                    and ((.subtype // "success") == "success")' >/dev/null 2>&1; then
                    result_status=success
                    echo -e "\n  ${GREEN}Agent finished — ${tool_count} tool calls in ${elapsed}s${NC}"
                else
                    result_status=error
                    echo -e "\n  ${RED}Agent returned a terminal error — ${tool_count} tool calls in ${elapsed}s${NC}" >&2
                fi
                local cost turns
                cost=$(printf '%s\n' "$line" | jq -r '.cost_usd // empty' 2>/dev/null) || true
                turns=$(printf '%s\n' "$line" | jq -r '.num_turns // empty' 2>/dev/null) || true
                [ -n "$cost" ] && [ "$cost" != "null" ] && echo -e "  ${DIM}Cost: \$${cost} · Turns: ${turns}${NC}"
                break
                ;;
        esac
    done
    # A terminal frame is necessary but not sufficient: explicit error and
    # malformed result frames fail just like a missing terminal frame.
    [ "$result_status" = success ]
}

run_with_claude_agent() {
    local prompt_content="$1"
    shift
    local -a agent_prefix=("$@") pipeline_status

    printf '%s\n' "$prompt_content" \
        | env -u ANTHROPIC_API_KEY "${agent_prefix[@]}" $AGENT_CMD \
            --dangerously-skip-permissions --verbose --output-format stream-json \
            "${AGENT_MODEL_ARGS[@]}" -p - \
        | show_agent_progress
    pipeline_status=("${PIPESTATUS[@]}")

    [ "${#pipeline_status[@]}" -eq 3 ] \
        && [ "${pipeline_status[0]}" -eq 0 ] \
        && [ "${pipeline_status[1]}" -eq 0 ] \
        && [ "${pipeline_status[2]}" -eq 0 ]
}

generate_evidence_token() {
    local token=""
    if [ -r /dev/urandom ]; then
        token=$(LC_ALL=C od -An -N16 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n') || token=""
    fi
    if ! printf '%s' "$token" | grep -Eq '^[0-9a-f]{32}$'; then
        token=$(openssl rand -hex 16 2>/dev/null || true)
    fi
    printf '%s' "$token" | grep -Eq '^[0-9a-f]{32}$' || return 1
    printf '%s\n' "$token"
}

bead_verification_profile() {
    local issue_id="$1" issue_file profile
    command -v jq >/dev/null 2>&1 || return 1
    issue_file=$(mktemp) || return 1
    if ! bd show "$issue_id" --json > "$issue_file" 2>/dev/null; then
        rm -f "$issue_file"
        return 1
    fi
    profile=$(jq -er '
        (if type == "array" then .[0] else . end) as $bead
        | select(($bead | type) == "object")
        | (($bead.labels // []) | map(tostring | ascii_downcase)) as $labels
        | ([ $bead.title // "", $bead.description // "", $bead.acceptance_criteria // "" ]
            | map(tostring) | join(" ") | ascii_downcase) as $scope
        | (($bead.title // "") | tostring | ascii_downcase) as $title
        | if (($labels | any(. == "packaging" or . == "release" or . == "release-archive" or . == "distribution-archive"))
              or ($title | test("\\b(packag(e|ed|ing)|release|installer|homebrew)\\b"))
              or ($scope | test("\\b(cargo package|release archive|distribution archive|packaged binary|package artifact)\\b"))) then
              "packaged-artifact"
          elif (($labels | any(. == "tui" or . == "terminal-ui" or . == "interactive"))
                or ($scope | test("\\b(tui|terminal ui|tmux|interactive (ui|flow|picker))\\b"))) then
              "tmux-tui"
          elif (($labels | any(. == "connectivity" or . == "provider-connectivity"))
                or ($title | test("\\b(openrouter|provider|api)?[ -]*(connectivity|connection) smoke\\b|\\bbasic provider connectivity\\b"))) then
              "connectivity"
          else "headless"
          end
    ' "$issue_file" 2>/dev/null) || profile=""
    rm -f "$issue_file"
    case "$profile" in
        headless|connectivity|tmux-tui|packaged-artifact) printf '%s\n' "$profile" ;;
        *) return 1 ;;
    esac
}

bead_implementation_surfaces() {
    local issue_id="$1" profile="$2" issue_file surfaces
    command -v jq >/dev/null 2>&1 || return 1
    issue_file=$(mktemp) || return 1
    if ! bd show "$issue_id" --json > "$issue_file" 2>/dev/null; then
        rm -f "$issue_file"
        return 1
    fi
    surfaces=$(jq -er --arg profile "$profile" '
        (if type == "array" then .[0] else . end) as $bead
        | select(($bead | type) == "object")
        | (($bead.labels // []) | map(tostring | ascii_downcase)) as $labels
        | ([ $bead.title // "", $bead.description // "", $bead.acceptance_criteria // "" ]
            | map(tostring) | join(" ") | ascii_downcase) as $scope
        | (($bead.title // "") | tostring | ascii_downcase) as $title
        | (["rust"]
            + (if $profile == "packaged-artifact" then ["packaging"] else [] end)
            + (if (($labels | any(. == "script" or . == "shell" or . == "automation" or . == "loop"))
                    or ($title | test("\\b(loop(\\.sh)?|shell|automation|script)\\b"))
                    or ($scope | test("\\b(shell script|build script|agent loop|loop\\.sh|prompt file)\\b")))
                then ["script"] else [] end)
            + (if (($labels | any(. == "data" or . == "fixture" or . == "catalog"))
                    or ($title | test("\\b(data|fixture|catalog|registry)\\b")))
                then ["data"] else [] end)
            + (if (($labels | any(. == "asset" or . == "image" or . == "icon"))
                    or ($title | test("\\b(asset|image|icon|logo|banner)\\b")))
                then ["asset"] else [] end)
            + (if (($labels | any(. == "cargo-config" or . == "build-config"))
                    or ($title | test("\\b(cargo config|build config|rust toolchain)\\b")))
                then ["cargo-config"] else [] end))
        | unique | join(",")
        | select(length > 0)
    ' "$issue_file" 2>/dev/null) || surfaces=""
    rm -f "$issue_file"
    [ -n "$surfaces" ] || return 1
    printf '%s\n' "$surfaces"
}

real_binary_evidence_payload() {
    local issue_id="$1" token="$2" comments_file payload
    command -v jq >/dev/null 2>&1 || { printf '{"state":"unavailable"}\n'; return; }
    comments_file=$(mktemp) || { printf '{"state":"unavailable"}\n'; return; }
    if ! bd comments "$issue_id" --json > "$comments_file" 2>/dev/null; then
        rm -f "$comments_file"
        printf '{"state":"unavailable"}\n'
        return
    fi
    payload=$(jq -c --arg token "$token" '
        (if type == "array" then .
         elif (.comments? | type) == "array" then .comments
         else error("unexpected comments JSON") end)
        | to_entries
        | map({index: .key,
               text: ((.value.text // .value.body // .value.comment // .value.content // "") | tostring)})
        | map(select(.text | contains("Token: " + $token))) as $mentions
        | if ($mentions | length) == 0 then {state: "missing"}
          else ($mentions | sort_by(.index) | .[-1]) as $comment
          | (($comment.text
              | capture("\\A\\[REAL-BINARY EVIDENCE\\]\\nToken: (?<token>[0-9a-f]{32})\\nScenario: (?<scenario>[^\\n]+)\\nInterface: (?<interface>[^\\n]+)\\nArtifact: (?<artifact>[^\\n]+)\\nCommands: (?<commands>[^\\n]+)\\nExpected: (?<expected>[^\\n]+)\\nObserved: (?<observed>[^\\n]+)\\nResult: (?<result>PASS|FAIL|BLOCKED)\\z"; "")) // null) as $e
          | if $e == null or $e.token != $token then {state: "invalid"}
            else {state: "structured", evidence: $e}
            end
          end
    ' "$comments_file" 2>/dev/null) || payload='{"state":"invalid"}'
    rm -f "$comments_file"
    printf '%s\n' "$payload"
}

stdin_assertion_is_safe() {
    local assertion="$1"
    # The fixed-string assertion receives only the producer's stdout. `--` is
    # mandatory so agent-controlled text cannot be interpreted as an option.
    printf '%s\n' "$assertion" \
        | grep -Eq "^grep[[:space:]]+-Fq[[:space:]]+--[[:space:]]+('[^']{3,}'|\"[^\"]{3,}\"|[A-Za-z0-9_.:/-]{3,})$"
}

single_driver_pipeline_is_safe() {
    local commands="$1" executable="$2" driver assertion remainder
    driver=${commands%%|*}
    [ "$driver" != "$commands" ] || return 1
    assertion=${commands#*|}
    [[ "$assertion" != *'|'* ]] || return 1
    driver=$(printf '%s' "$driver" | sed 's/[[:space:]]*$//')
    assertion=$(printf '%s' "$assertion" | sed 's/^[[:space:]]*//')
    remainder=${driver#"$executable"}
    [ "$remainder" != "$driver" ] || return 1
    [ -z "$remainder" ] || [[ "$remainder" == ' '* ]] || return 1
    stdin_assertion_is_safe "$assertion"
}

tmux_driver_is_safe() {
    local commands="$1" session="" segment assertion assertion_literal i count
    local -a parts=()
    while IFS= read -r segment; do
        parts+=("$(printf '%s' "$segment" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')")
    done < <(printf '%s\n' "$commands" | sed 's/[[:space:]]*&&[[:space:]]*/\
/g')
    count=${#parts[@]}
    [ "$count" -ge 4 ] || return 1

    session=$(printf '%s\n' "${parts[0]}" \
        | sed -nE 's/^tmux new-session -d -s ([A-Za-z0-9_-]+)( -x [0-9]+ -y [0-9]+)?$/\1/p')
    [ -n "$session" ] || return 1
    printf '%s\n' "${parts[1]}" \
        | grep -Eq "^tmux send-keys -t ${session} ('mini-agent([^']*)'|\"mini-agent([^\"]*)\") Enter$" \
        || return 1
    [[ "${parts[1]}" != *'|'* ]] || return 1

    i=2
    while [ "$i" -lt "$((count - 2))" ]; do
        segment=${parts[$i]}
        if printf '%s\n' "$segment" | grep -Eq '^sleep [1-9][0-9]?$'; then
            :
        elif printf '%s\n' "$segment" \
                | grep -Eq "^tmux send-keys -t ${session} ('[^']+'|\"[^\"]+\"|([A-Za-z]+[[:space:]]*)+) Enter$"; then
            :
        else
            return 1
        fi
        i=$((i + 1))
    done

    segment=${parts[$((count - 2))]}
    [ "${segment%%|*}" != "$segment" ] || return 1
    [ "$(printf '%s' "${segment%%|*}" | sed 's/[[:space:]]*$//')" = "tmux capture-pane -t $session -p" ] || return 1
    assertion=$(printf '%s' "${segment#*|}" | sed 's/^[[:space:]]*//')
    stdin_assertion_is_safe "$assertion" || return 1
    assertion_literal=${assertion#grep -Fq -- }
    case "$assertion_literal" in
        \'*\') assertion_literal=${assertion_literal#\'}; assertion_literal=${assertion_literal%\'} ;;
        \"*\") assertion_literal=${assertion_literal#\"}; assertion_literal=${assertion_literal%\"} ;;
    esac
    i=2
    while [ "$i" -lt "$((count - 2))" ]; do
        segment=${parts[$i]}
        case "$segment" in
            tmux\ send-keys\ *) [[ "$segment" != *"$assertion_literal"* ]] || return 1 ;;
        esac
        i=$((i + 1))
    done
    [ "${parts[$((count - 1))]}" = "tmux kill-session -t $session" ]
}

rewrite_tmux_scenario() {
    local commands="$1" original_session="$2" session_name="$3" tmux_path="$4"
    local socket_label="$5" installed_canonical="$6" segment launch i count rewritten=""
    local -a parts=()
    while IFS= read -r segment; do
        parts+=("$(printf '%s' "$segment" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')")
    done < <(printf '%s\n' "$commands" | sed 's/[[:space:]]*&&[[:space:]]*/\
/g')
    count=${#parts[@]}
    [ "$count" -ge 4 ] || return 1

    launch=${parts[1]#"tmux send-keys -t $original_session "}
    launch=${launch% Enter}
    case "$launch" in
        "'mini-agent"*|\"mini-agent*) ;;
        *) return 1 ;;
    esac
    launch=${launch/mini-agent/exec $installed_canonical}

    segment=${parts[0]/" -s $original_session"/" -s $session_name"}
    segment=${segment/"tmux new-session"/"$tmux_path -L $socket_label -f /dev/null new-session"}
    rewritten="$segment $launch && sleep 1"

    # The declarative initial send-keys segment is intentionally consumed: the
    # loop starts the app as the pane command so its command text cannot satisfy
    # the later pane assertion. Remaining send-keys are feature interactions.
    i=2
    while [ "$i" -lt "$count" ]; do
        segment=${parts[$i]/" -t $original_session"/" -t $session_name"}
        case "$segment" in
            tmux\ send-keys\ *)
                segment=${segment/"tmux send-keys"/"$tmux_path -L $socket_label send-keys"}
                rewritten="$rewritten && $segment && sleep 1"
                ;;
            tmux\ capture-pane\ *)
                segment=${segment/"tmux capture-pane"/"$tmux_path -L $socket_label capture-pane"}
                rewritten="$rewritten && $segment"
                ;;
            tmux\ kill-session\ *)
                segment=${segment/"tmux kill-session"/"$tmux_path -L $socket_label kill-session"}
                rewritten="$rewritten && $segment"
                ;;
            sleep\ *) rewritten="$rewritten && $segment" ;;
            *) return 1 ;;
        esac
        i=$((i + 1))
    done
    printf '%s\n' "$rewritten"
}

evidence_commands_are_safe() {
    local commands="$1" profile="$2" artifact="$3" without_and
    [ -n "$commands" ] || return 1

    # The loop accepts only a literal feature-driving pipeline (or a strictly
    # shaped tmux chain). It does not execute setup commands supplied by an
    # agent. This keeps installation, executable identity, and assertions under
    # loop ownership.
    case "$commands" in
        *$'\n'*|*';'*|*'||'*|*'`'*|*'$'*|*'('*|*')'*|*'{'*|*'}'*|*'<'*|*'>'*|*'#'*|*'!'*|*'*'*|*'?'*|*'['*) return 1 ;;
    esac
    printf '%s' "$commands" | grep -F '\' >/dev/null && return 1
    without_and=${commands//&&/}
    [[ "$without_and" == *'&'* ]] && return 1
    printf '%s\n' "$commands" | grep -Eq '(^|[[:space:]|&])[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=' && return 1

    case "$profile" in
        headless|connectivity)
            [[ "$commands" != *'&&'* ]] && [[ "$commands" != *"$PWD"* ]] \
                && single_driver_pipeline_is_safe "$commands" mini-agent
            ;;
        tmux-tui)
            [[ "$commands" != *"$PWD"* ]] && tmux_driver_is_safe "$commands"
            ;;
        packaged-artifact)
            printf '%s' "$artifact" | grep -Eq '^(\./|/)[A-Za-z0-9._/-]+/mini-agent(\.exe)?$' \
                && [[ "$commands" != *'&&'* ]] \
                && [[ "${commands#"$artifact"}" != *"$PWD"* ]] \
                && single_driver_pipeline_is_safe "$commands" "$artifact"
            ;;
        *) return 1 ;;
    esac
}

real_binary_evidence_status() {
    local issue_id="$1" token="$2" profile="$3" payload state result artifact commands combined
    case "$profile" in
        headless|connectivity|tmux-tui|packaged-artifact) ;;
        *) echo invalid; return ;;
    esac
    payload=$(real_binary_evidence_payload "$issue_id" "$token")
    state=$(printf '%s\n' "$payload" | jq -r '.state // "invalid"' 2>/dev/null) || state=invalid
    case "$state" in
        missing|invalid|unavailable) echo "$state"; return ;;
        structured) ;;
        *) echo invalid; return ;;
    esac

    result=$(printf '%s\n' "$payload" | jq -r '.evidence.result' 2>/dev/null) || { echo invalid; return; }
    if [ "$result" != PASS ]; then
        printf '%s\n' "$result" | tr '[:upper:]' '[:lower:]'
        return
    fi
    if printf '%s\n' "$payload" | jq -e '
            [.evidence.scenario, .evidence.interface,
             .evidence.commands, .evidence.expected, .evidence.observed]
            | any(ascii_downcase
                  | test("^\\s*(<[^>]*>|todo|tbd|n/?a|none|unknown|pass|passed|success|works?|feature output)\\s*$"))
        ' >/dev/null 2>&1; then
        echo invalid
        return
    fi

    artifact=$(printf '%s\n' "$payload" | jq -r '.evidence.artifact')
    commands=$(printf '%s\n' "$payload" | jq -r '.evidence.commands')
    combined=$(printf '%s\n' "$payload" | jq -r '[.evidence.scenario, .evidence.commands, .evidence.expected, .evidence.observed] | join(" ") | ascii_downcase')
    if printf '%s\n' "$combined" | grep -Eq '\bhello\b|hello world|generic hello|one[- ]word' \
            && [ "$profile" != connectivity ]; then
        echo invalid
        return
    fi
    case "$profile" in
        headless|connectivity) [ "$(printf '%s\n' "$payload" | jq -r '.evidence.interface')" = headless ] && [ "$artifact" = none ] || { echo invalid; return; } ;;
        tmux-tui) [ "$(printf '%s\n' "$payload" | jq -r '.evidence.interface')" = tmux-tui ] && [ "$artifact" = none ] || { echo invalid; return; } ;;
        packaged-artifact) [ "$(printf '%s\n' "$payload" | jq -r '.evidence.interface')" = packaged-artifact ] && [ "$artifact" != none ] || { echo invalid; return; } ;;
    esac
    if evidence_commands_are_safe "$commands" "$profile" "$artifact"; then echo pass; else echo invalid; fi
}

run_with_hard_timeout() {
    local seconds="$1"
    shift
    # External timeout binaries cannot invoke functions defined in this shell.
    # Use the watchdog below for function-backed commands such as the clean
    # install/replay environment wrappers.
    if [ -n "${TIMEOUT_BIN:-}" ] && ! declare -F "$1" >/dev/null; then
        "$TIMEOUT_BIN" --kill-after=30 "$seconds" "$@"
        return $?
    fi

    local marker command_pid watchdog_pid status
    marker=$(mktemp) || return 1
    "$@" &
    command_pid=$!
    (
        sleep "$seconds"
        if kill -0 "$command_pid" 2>/dev/null; then
            printf timeout > "$marker"
            command -v pkill >/dev/null 2>&1 && pkill -TERM -P "$command_pid" 2>/dev/null || true
            kill -TERM "$command_pid" 2>/dev/null || true
            sleep 2
            kill -KILL "$command_pid" 2>/dev/null || true
        fi
    ) &
    watchdog_pid=$!
    if wait "$command_pid"; then status=0; else status=$?; fi
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
    if [ -s "$marker" ]; then status=124; fi
    rm -f "$marker"
    return "$status"
}

hash_file_sha256() {
    local path="$1" digest
    if command -v shasum >/dev/null 2>&1; then
        digest=$(shasum -a 256 "$path" 2>/dev/null | awk '{print $1}') || return 1
    elif command -v sha256sum >/dev/null 2>&1; then
        digest=$(sha256sum "$path" 2>/dev/null | awk '{print $1}') || return 1
    else
        return 1
    fi
    printf '%s' "$digest" | grep -Eq '^[0-9a-f]{64}$' || return 1
    printf '%s\n' "$digest"
}

hash_text_sha256() {
    local text="$1" digest
    if command -v shasum >/dev/null 2>&1; then
        digest=$(printf '%s' "$text" | shasum -a 256 2>/dev/null | awk '{print $1}') || return 1
    elif command -v sha256sum >/dev/null 2>&1; then
        digest=$(printf '%s' "$text" | sha256sum 2>/dev/null | awk '{print $1}') || return 1
    else
        return 1
    fi
    printf '%s' "$digest" | grep -Eq '^[0-9a-f]{64}$' || return 1
    printf '%s\n' "$digest"
}

canonical_existing_path() {
    local path="$1" dir base
    [ -e "$path" ] || return 1
    if command -v realpath >/dev/null 2>&1; then
        realpath "$path" 2>/dev/null
        return $?
    fi
    if command -v python3 >/dev/null 2>&1; then
        python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$path" 2>/dev/null
        return $?
    fi
    dir=$(dirname "$path")
    base=$(basename "$path")
    dir=$(cd "$dir" 2>/dev/null && pwd -P) || return 1
    printf '%s/%s\n' "$dir" "$base"
}

file_mtime_epoch() {
    local path="$1" mtime
    mtime=$(stat -f %m "$path" 2>/dev/null || stat -c %Y "$path" 2>/dev/null) || return 1
    printf '%s\n' "$mtime"
}

repository_state_digest() {
    local snapshot path file_sha
    snapshot=$(mktemp) || return 1
    {
        git rev-parse HEAD || exit 1
        git diff --binary HEAD -- . ':(exclude).beads/**' || exit 1
        # Build outputs are intentionally ignored here because Cargo mutates
        # target/. The feature scenario itself runs from a private HEAD archive,
        # so it cannot rely on or normally mutate those repository-local files.
        while IFS= read -r path; do
            case "$path" in .beads|.beads/*) continue ;; esac
            [ -f "$path" ] || continue
            file_sha=$(hash_file_sha256 "$path") || exit 1
            printf 'untracked %s %s\n' "$path" "$file_sha"
        done < <(git ls-files --others --exclude-standard | LC_ALL=C sort)
        while IFS= read -r path; do
            case "$path" in
                target|target/*|.beads|.beads/*|.dolt|.dolt/*|.codex|.codex/*|.superpowers|.superpowers/*) continue ;;
            esac
            [ -f "$path" ] || continue
            file_sha=$(hash_file_sha256 "$path") || exit 1
            printf 'ignored %s %s\n' "$path" "$file_sha"
        done < <(git ls-files --others --ignored --exclude-standard | LC_ALL=C sort)
    } > "$snapshot" 2>/dev/null || { rm -f "$snapshot"; return 1; }
    file_sha=$(hash_file_sha256 "$snapshot") || { rm -f "$snapshot"; return 1; }
    rm -f "$snapshot"
    printf '%s\n' "$file_sha"
}

run_in_clean_replay_environment() {
    local runtime_path="$1" name value
    shift
    local -a clean_env=(env -i)
    while IFS='=' read -r name value; do
        case "$name" in
            HOME|USER|USERNAME|LOGNAME|HOST|HOSTNAME|TERM|COLORTERM|LANG|LC_*|TZ|NO_COLOR|EDITOR|VISUAL|\
            XDG_RUNTIME_DIR|XDG_CACHE_HOME|XDG_CONFIG_HOME|XDG_DATA_HOME|XDG_STATE_HOME|\
            *_API_KEY|*_TOKEN|OPENROUTER_MODEL|MCP_FIXTURE_*|ZS_*|\
            HTTPS_PROXY|HTTP_PROXY|ALL_PROXY|NO_PROXY|SSL_CERT_FILE|SSL_CERT_DIR)
                clean_env+=("$name=$value")
                ;;
        esac
    done < <(env)
    clean_env+=("PATH=$runtime_path" "SHELL=/bin/bash")
    "${clean_env[@]}" "$@"
}

run_in_clean_install_environment() {
    local install_root="$1" cargo_path="$2" rustc_path="$3" name
    shift 3
    local -a clean_env=(env -i)
    mkdir -p "$install_root/cargo-home" "$install_root/cargo-target" || return 1
    for name in HOME USER LOGNAME TMPDIR TMP TEMP \
            HTTPS_PROXY HTTP_PROXY ALL_PROXY NO_PROXY SSL_CERT_FILE SSL_CERT_DIR \
            MACOSX_DEPLOYMENT_TARGET SDKROOT DEVELOPER_DIR; do
        if [ "${!name+x}" = x ]; then
            clean_env+=("$name=${!name}")
        fi
    done
    clean_env+=("PATH=$(dirname "$cargo_path"):$(dirname "$rustc_path"):/usr/bin:/bin:/usr/sbin:/sbin")
    clean_env+=("CARGO_INSTALL_ROOT=$install_root" "CARGO_HOME=$install_root/cargo-home")
    clean_env+=("CARGO_TARGET_DIR=$install_root/cargo-target" "RUSTC=$rustc_path" "SHELL=/bin/bash")
    # Every evidence replay starts with an empty Cargo home, so it must be able to
    # populate its private registry even when the parent loop runs offline. The
    # target is also empty, so favor one-shot parallel codegen over incremental
    # artifacts that cannot be reused by a later replay.
    clean_env+=("CARGO_NET_RETRY=10" "CARGO_INCREMENTAL=0")
    clean_env+=("CARGO_PROFILE_DEV_CODEGEN_UNITS=256")
    "${clean_env[@]}" "$cargo_path" "$@"
}

cargo_config_free_ancestor_chain() {
    local directory="$1" parent
    directory=$(canonical_existing_path "$directory") || return 1
    while :; do
        [ ! -e "$directory/.cargo/config" ] && [ ! -e "$directory/.cargo/config.toml" ] || return 1
        [ "$directory" = / ] && break
        parent=$(dirname "$directory")
        [ "$parent" != "$directory" ] || break
        directory="$parent"
    done
}

artifact_path_is_link_free() {
    local artifact="$1" root="$2"
    command -v python3 >/dev/null 2>&1 || return 1
    python3 - "$artifact" "$root" <<'PY'
import os
import stat
import sys

path = os.path.abspath(sys.argv[1])
root = os.path.abspath(sys.argv[2])
try:
    if os.path.commonpath([path, root]) != root:
        raise ValueError("outside repository")
    relative = os.path.relpath(path, root)
    current = root
    for component in relative.split(os.sep):
        current = os.path.join(current, component)
        if stat.S_ISLNK(os.lstat(current).st_mode):
            raise ValueError("symlink component")
except (OSError, ValueError):
    raise SystemExit(1)
PY
}

record_replay_attestation() {
    local issue_id="$1" evidence_nonce="$2" profile="$3" result="$4" reason="$5"
    local binary_sha="$6" artifact_sha="$7" commands_sha="$8" transcript_sha="$9"
    bd comments add "$issue_id" "[LOOP REAL-BINARY REPLAY]
Evidence nonce: $evidence_nonce
Profile: $profile
Result: $result
Reason: $reason
Installed SHA-256: ${binary_sha:-unavailable}
Artifact SHA-256: ${artifact_sha:-none}
Commands SHA-256: ${commands_sha:-unavailable}
Transcript SHA-256: ${transcript_sha:-unavailable}" >/dev/null 2>&1
}

replay_real_binary_evidence() {
    local issue_id="$1" token="$2" profile="$3" iteration_start_epoch="$4"
    local cargo_path="$5" trusted_cargo_sha="$6"
    local rustc_path="$7" trusted_rustc_sha="$8"
    local trusted_tmux_path="${9:-}" trusted_tmux_sha="${10:-}"
    local payload commands artifact commands_sha transcript="" scenario_file="" trace_nonce replay_status=1
    local install_root="" install_workspace="" runtime_path installed_canonical="" installed_mtime installed_sha=""
    local artifact_canonical="" artifact_mtime artifact_sha="none" artifact_copy=""
    local transcript_sha="" reason="replay-failed" repo_before repo_after scenario_commands=""
    local original_session="" session_name="" socket_label="" scenario_workspace="" repo_canonical=""
    payload=$(real_binary_evidence_payload "$issue_id" "$token")
    commands=$(printf '%s\n' "$payload" | jq -er 'select(.state == "structured") | .evidence.commands' 2>/dev/null) || return 1
    artifact=$(printf '%s\n' "$payload" | jq -er '.evidence.artifact' 2>/dev/null) || return 1
    evidence_commands_are_safe "$commands" "$profile" "$artifact" || return 1
    if [ "$profile" = tmux-tui ]; then
        original_session=$(printf '%s\n' "$commands" \
            | sed -nE 's/^tmux new-session -d -s ([A-Za-z0-9_-]+).*/\1/p')
        [ -n "$original_session" ] || return 1
        session_name="rb-${token:0:12}-$$"
    fi
    commands_sha=$(hash_text_sha256 "$commands") || return 1
    case "$cargo_path" in /*) ;; *) return 1 ;; esac
    [ -x "$cargo_path" ] && [ "$(hash_file_sha256 "$cargo_path" 2>/dev/null || true)" = "$trusted_cargo_sha" ] \
        || return 1
    case "$rustc_path" in /*) ;; *) return 1 ;; esac
    [ -x "$rustc_path" ] && [ "$(hash_file_sha256 "$rustc_path" 2>/dev/null || true)" = "$trusted_rustc_sha" ] \
        || return 1
    if [ "$profile" = tmux-tui ]; then
        case "$trusted_tmux_path" in /*) ;; *) return 1 ;; esac
        [ -x "$trusted_tmux_path" ] \
            && [ "$(hash_file_sha256 "$trusted_tmux_path" 2>/dev/null || true)" = "$trusted_tmux_sha" ] \
            || return 1
    fi
    transcript=$(mktemp) || return 1
    install_root=$(mktemp -d) || { rm -f "$transcript"; return 1; }
    install_root=$(canonical_existing_path "$install_root") \
        || { rm -f "$transcript"; return 1; }
    runtime_path="$install_root/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    repo_before=$(repository_state_digest) || reason="repository-snapshot-failed"
    if [ "$reason" = replay-failed ]; then
        install_workspace=$(mktemp -d) || reason="install-workspace-failed"
    fi
    if [ "$reason" = replay-failed ] \
            && ! (set -o pipefail
                git archive --format=tar HEAD | tar -xf - -C "$install_workspace"); then
        reason="install-workspace-copy-failed"
    fi
    if [ "$reason" = replay-failed ]; then
        # Acceptance pins Cargo/rustc itself. Do not let committed project Cargo
        # config reintroduce wrappers, runners, aliases, or source replacement.
        rm -f "$install_workspace/.cargo/config" "$install_workspace/.cargo/config.toml" \
            || reason="install-config-sanitization-failed"
        if [ "$reason" = replay-failed ] \
                && ! cargo_config_free_ancestor_chain "$install_workspace"; then
            reason="install-ancestor-cargo-config"
        fi
    fi

    if [ "$reason" = replay-failed ] \
            && ! (cd "$install_workspace" || exit 1
                # Cargo registry metadata and compiler artifacts routinely exceed
                # the evidence scenario's 2 MiB output-file limit.
                run_with_hard_timeout "${LOOP_INSTALL_TIMEOUT_SECS:-900}" \
                    run_in_clean_install_environment "$install_root" "$cargo_path" "$rustc_path" \
                        install --path . --debug --locked) > "$transcript" 2>&1; then
        reason="cargo-install-failed"
    elif [ "$reason" = replay-failed ]; then
        if ! cargo_config_free_ancestor_chain "$install_workspace"; then
            reason="install-ancestor-cargo-config-changed"
        elif [ "$(hash_file_sha256 "$cargo_path" 2>/dev/null || true)" != "$trusted_cargo_sha" ]; then
            reason="cargo-executable-changed"
        elif [ "$(hash_file_sha256 "$rustc_path" 2>/dev/null || true)" != "$trusted_rustc_sha" ]; then
            reason="rustc-executable-changed"
        fi
        repo_after=$(repository_state_digest) || reason="repository-post-install-snapshot-failed"
        if [ "$reason" = replay-failed ] && [ "$repo_before" != "$repo_after" ]; then
            reason="cargo-install-mutated-repository"
        fi
    fi

    if [ "$reason" = replay-failed ]; then
        installed_canonical=$(canonical_existing_path "$install_root/bin/mini-agent" 2>/dev/null || true)
        installed_mtime=$(file_mtime_epoch "$installed_canonical" 2>/dev/null || echo 0)
        if [ "$installed_canonical" != "$install_root/bin/mini-agent" ] || [ ! -x "$installed_canonical" ]; then
            reason="installed-binary-unavailable"
        elif [ "$installed_mtime" -lt "$iteration_start_epoch" ] 2>/dev/null; then
            reason="installed-binary-stale"
        elif ! installed_sha=$(hash_file_sha256 "$installed_canonical"); then
            reason="installed-binary-hash-failed"
        else
            reason="preflight-passed"
        fi
    fi

    if [ "$reason" = preflight-passed ] && [ "$profile" = packaged-artifact ]; then
        repo_canonical=$(canonical_existing_path "$PWD" 2>/dev/null || true)
        artifact_canonical=$(canonical_existing_path "$artifact" 2>/dev/null || true)
        artifact_mtime=$(file_mtime_epoch "$artifact_canonical" 2>/dev/null || echo 0)
        case "$artifact_canonical" in
            "$repo_canonical"/*) ;;
            *) reason="artifact-outside-repository" ;;
        esac
        if [ "$reason" = preflight-passed ] \
                && ! artifact_path_is_link_free "$artifact" "$PWD" >/dev/null 2>&1; then
            reason="artifact-path-has-symlink"
        elif [ "$reason" = preflight-passed ] && [ "$artifact_canonical" = "$installed_canonical" ]; then
            reason="artifact-is-installed-binary"
        elif [ "$reason" = preflight-passed ] && [ ! -x "$artifact_canonical" ]; then
            reason="artifact-not-executable"
        elif [ "$reason" = preflight-passed ] && [ "$artifact_mtime" -lt "$iteration_start_epoch" ] 2>/dev/null; then
            reason="artifact-stale"
        elif [ "$reason" = preflight-passed ] && ! artifact_sha=$(hash_file_sha256 "$artifact_canonical"); then
            reason="artifact-hash-failed"
        elif [ "$reason" = preflight-passed ]; then
            artifact_copy="$install_root/artifact-mini-agent"
            if ! cp "$artifact_canonical" "$artifact_copy" || ! chmod 500 "$artifact_copy" \
                    || [ "$(hash_file_sha256 "$artifact_copy" 2>/dev/null || true)" != "$artifact_sha" ]; then
                reason="artifact-copy-failed"
            fi
        fi
    fi

    if [ "$reason" = preflight-passed ]; then
        trace_nonce=$(generate_evidence_token) || reason="trace-nonce-failed"
        [ "$profile" = tmux-tui ] && socket_label="rb-${trace_nonce:0:16}"
    fi
    if [ "$reason" = preflight-passed ]; then
        scenario_file=$(mktemp) || reason="scenario-file-failed"
    fi
    if [ "$reason" = preflight-passed ]; then
        scenario_workspace=$(mktemp -d) || reason="scenario-workspace-failed"
    fi
    if [ "$reason" = preflight-passed ] \
            && ! (set -o pipefail
                git archive --format=tar HEAD | tar -xf - -C "$scenario_workspace"); then
        reason="scenario-workspace-copy-failed"
    fi
    if [ "$reason" = preflight-passed ]; then
        case "$profile" in
            headless|connectivity)
                scenario_commands="$installed_canonical${commands#mini-agent}"
                ;;
            packaged-artifact)
                scenario_commands="$artifact_copy${commands#"$artifact"}"
                ;;
            tmux-tui)
                scenario_commands=$(rewrite_tmux_scenario "$commands" "$original_session" \
                    "$session_name" "$trusted_tmux_path" "$socket_label" "$installed_canonical") \
                    || reason="tmux-scenario-rewrite-failed"
                ;;
        esac
        {
            printf "PS4='+RB%s+ '\n" "$trace_nonce"
            printf 'readonly PS4 PATH\nhash -r\nset -o pipefail\nset -x\n'
            printf '%s\n' "$scenario_commands"
        } > "$scenario_file"
        if ! bash -n "$scenario_file" >> "$transcript" 2>&1; then
            replay_status=1
            reason="scenario-syntax-invalid"
        elif (cd "$scenario_workspace" || exit 1
            ulimit -f 4096 2>/dev/null || true
            run_with_hard_timeout "${LOOP_EVIDENCE_TIMEOUT_SECS:-300}" \
                run_in_clean_replay_environment "$runtime_path" \
                    bash --noprofile --norc "$scenario_file") >> "$transcript" 2>&1; then
            replay_status=0
        else
            replay_status=$?
            reason="scenario-exit-${replay_status}"
        fi
        repo_after=$(repository_state_digest) || {
            replay_status=1
            reason="repository-post-replay-snapshot-failed"
        }
        if [ -n "$repo_after" ] && [ "$repo_before" != "$repo_after" ]; then
            replay_status=1
            reason="scenario-mutated-repository"
        fi

        if [ "$replay_status" = 0 ]; then
            case "$profile" in
                headless|connectivity)
                    grep -F "+RB${trace_nonce}+ ${installed_canonical}" "$transcript" >/dev/null \
                        || { replay_status=1; reason="installed-binary-not-traced"; }
                    ;;
                tmux-tui)
                    for verb in new-session capture-pane kill-session; do
                        grep -F "+RB${trace_nonce}+ ${trusted_tmux_path} -L ${socket_label}" "$transcript" \
                            | grep -F "${verb}" >/dev/null \
                            || { replay_status=1; reason="tmux-${verb}-not-traced"; break; }
                    done
                    grep -F "exec ${installed_canonical}" "$transcript" >/dev/null \
                        || { replay_status=1; reason="tmux-installed-binary-not-sent"; }
                    grep -F -- "-t ${session_name}" "$transcript" >/dev/null \
                        || { replay_status=1; reason="tmux-session-not-correlated"; }
                    ;;
                packaged-artifact)
                    grep -F "+RB${trace_nonce}+ ${artifact_copy}" "$transcript" >/dev/null \
                        || { replay_status=1; reason="artifact-not-traced"; }
                    ;;
            esac
        fi
    fi

    if [ -n "$socket_label" ] && [ -x "$trusted_tmux_path" ]; then
        "$trusted_tmux_path" -L "$socket_label" kill-server 2>/dev/null || true
        if [ "$(hash_file_sha256 "$trusted_tmux_path" 2>/dev/null || true)" != "$trusted_tmux_sha" ]; then
            replay_status=1
            reason="tmux-executable-changed"
        fi
    fi
    transcript_sha=$(hash_file_sha256 "$transcript" 2>/dev/null || true)
    if [ "$replay_status" = 0 ]; then reason="passed"; fi
    if ! record_replay_attestation "$issue_id" "$token" "$profile" \
            "$([ "$replay_status" = 0 ] && echo PASS || echo FAIL)" "$reason" \
            "$installed_sha" "$artifact_sha" "$commands_sha" "$transcript_sha"; then
        replay_status=1
    fi
    rm -f "$transcript" "$scenario_file"
    rm -rf "$install_root" "$install_workspace" "$scenario_workspace"
    [ "$replay_status" = 0 ]
}

decide_build_outcome() {
    local closed="$1" verify_status="$2" evidence="$3" committed="$4" code_changed="$5"
    if [ "$closed" = true ]; then
        if [ "$verify_status" = "0" ] && [ "$evidence" = pass ] \
                && [ "$committed" = true ] && [ "$code_changed" = true ]; then
            echo accept
        elif [ "$verify_status" = "0" ] && [ "$evidence" = pass ]; then
            # Build passes and evidence is valid but no code changed or not
            # committed — agent closed it prematurely; reopen quietly.
            echo reopen
        else
            # Build failed OR evidence is invalid: reopen so the agent retries.
            # evidence=invalid when verify passes means the proof format was
            # wrong but the binary works — still reopen (not reject) so the
            # agent can supply better evidence next iteration, but do NOT spawn
            # a new child P0 (that is handled by run_verification).
            echo reopen
        fi
    elif [ "$verify_status" = "0" ] && [ "$evidence" = pass ] && [ "$committed" = true ] && [ "$code_changed" = true ]; then
        echo auto-close
    elif [ "$verify_status" = "0" ] && [ "$evidence" = pass ]; then
        # Build passes, evidence valid, but nothing committed or no code changed.
        # Keep the bead alive without an explicit reopen comment.
        echo partial
    else
        echo partial
    fi
}

path_is_relevant_for_profile() {
    local path="$1" profile="$2" surfaces="${3:-rust}"
    case "$path" in
        src/*|Cargo.toml|Cargo.lock|build.rs) return 0 ;;
    esac
    case ",$surfaces," in
        *,script,*) case "$path" in scripts/*) return 0 ;; esac ;;
    esac
    case ",$surfaces," in
        *,data,*) case "$path" in data/*) return 0 ;; esac ;;
    esac
    case ",$surfaces," in
        *,asset,*) case "$path" in assets/*) return 0 ;; esac ;;
    esac
    case ",$surfaces," in
        *,cargo-config,*) case "$path" in .cargo/*) return 0 ;; esac ;;
    esac
    if [ "$profile" = packaged-artifact ] && [[ ",$surfaces," == *,packaging,* ]]; then
        case "$path" in
            packaging/*|nix/*|tap/*|.github/*|scripts/*|install.sh|justfile|default.nix|release.nix|shell.nix) return 0 ;;
        esac
    fi
    return 1
}

current_iteration_has_relevant_changes() {
    local start_commit="$1" end_commit="$2" profile="${3:-headless}" surfaces="${4:-rust}" path
    [ "$profile" = packaged-artifact ] && [ "$surfaces" = rust ] && surfaces="rust,packaging"
    [ -n "$start_commit" ] && [ -n "$end_commit" ] || return 1
    while IFS= read -r path; do
        path_is_relevant_for_profile "$path" "$profile" "$surfaces" && return 0
    done < <(git diff --name-only "$start_commit" "$end_commit" 2>/dev/null)
    return 1
}

# Stage automatic loop commits without sweeping tracked Beads exports into
# product history. Return success only when a non-Beads change is staged.
stage_non_beads_changes() {
    git add -A || return 1
    git reset -q -- .beads 2>/dev/null || return 1
    ! git diff --cached --quiet
}

reopen_build_bead() {
    local issue_id="$1" verify_status="$2" evidence_status="$3" state
    # Beads 1.0.2 treats --claim by an existing assignee as a successful no-op,
    # even when the issue is open. Clear assignment so the next claim can start it.
    if bd update "$issue_id" --status=open --assignee "" 2>/dev/null; then
        state=$(bead_enforcement_status "$issue_id")
    else
        state=unavailable
    fi
    if [ "$state" = open ]; then
        bd comments add "$issue_id" \
            "[LOOP] Reopened after iteration $CURRENT_ITERATION: verification=$([ "$verify_status" = 0 ] && echo pass || echo fail), evidence=$evidence_status." \
            2>/dev/null || true
        return 0
    fi
    # CLOSED and UNAVAILABLE are both enforcement failures: only explicit OPEN is safe.
    echo -e "${RED}ERROR: failed to explicitly confirm $issue_id open (state=$state); stopping this iteration${NC}" >&2
    bd comments add "$issue_id" "[LOOP] Failed to confirm bead open after acceptance gates failed (state=$state)." 2>/dev/null || true
    return 1
}

auto_close_build_bead() {
    local issue_id="$1" commit="$2" state
    if bd close "$issue_id" --reason "Auto-closed by loop: both gates passed, commit $commit" 2>/dev/null \
            && state=$(bead_enforcement_status "$issue_id") && [ "$state" = closed ]; then
        return 0
    fi
    echo -e "${RED}ERROR: failed to close and confirm $issue_id; leaving iteration partial${NC}" >&2
    bd comments add "$issue_id" "[LOOP] Auto-close failed after acceptance gates passed; bead remains partial." 2>/dev/null || true
    return 1
}

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Core: Run One Agent Iteration                                   ║
# ╚══════════════════════════════════════════════════════════════════╝

# run_iteration <prompt_file> <label> <iteration_num> <max_iterations>
# Each call is a fresh agent invocation (fresh context window).
run_iteration() {
    local prompt_file="$1" label="$2" iter="$3" max="$4"
    CURRENT_ITERATION=$iter

    echo -e "\n${BLUE}══════════════════════════════════════════════════════════════════${NC}"
    echo -e "${BLUE}  $label  —  Iteration $iter / $max  —  $(timestamp)${NC}"
    echo -e "${BLUE}══════════════════════════════════════════════════════════════════${NC}"

    print_open_beads

    # --- Build mode: pick a bead ---
    PICKED_ID=""
    PICKED_TITLE=""
    if [ "$MODE" = "build" ]; then
        local attempt
        for attempt in 1 2 3; do
            if pick_ready_bead; then
                break
            fi
            [ "$attempt" -lt 3 ] && sleep 1
        done

        if [ -z "$PICKED_ID" ]; then
            local total_open
            total_open=$( (bd list --limit 0 --status open 2>/dev/null || true) | count_lines)
            if [ "$total_open" = "0" ]; then
                echo -e "${GREEN}All tasks complete!${NC}"; return 1
            fi
            local blocked_count
            blocked_count=$( (bd blocked 2>/dev/null || true) | count_lines)
            if [ "$blocked_count" != "0" ]; then
                echo -e "${YELLOW}All remaining tasks are blocked.${NC}"
                bd blocked 2>/dev/null; return 1
            fi
            local raw_ready_count
            raw_ready_count=$( (bd ready 2>/dev/null || true) | count_lines)
            if [ "$raw_ready_count" != "0" ]; then
                echo -e "${YELLOW}All ready beads are filtered out (no-auto-loop / manual-gate / organizational epics).${NC}"
                echo -e "${DIM}  Filtered labels: ${BEAD_LABEL_BLOCKLIST[*]}${NC}"
                bd ready 2>/dev/null
                return 1
            fi
            echo -e "${RED}bd ready returned empty across 3 retries despite $total_open open and 0 blocked.${NC}"
            echo -e "${RED}Likely a dolt sync issue — try: bd dolt pull && bd ready${NC}"
            return 1
        fi

        if [ -n "$PICKED_ID" ]; then
            # Selection starts the acceptance-critical window. Set this before
            # any state query, claim, output, or other interruptible work so
            # SIGINT always reopens the selected bead unless a terminal state
            # has subsequently been confirmed.
            BUILD_ACCEPTANCE_ENFORCED=false
            local initial_state
            initial_state=$(bead_enforcement_status "$PICKED_ID")
            case "$initial_state" in
                open)
                    if ! bd update "$PICKED_ID" --claim 2>/dev/null; then
                        echo -e "${RED}Cannot claim $PICKED_ID; attempting checked reopen${NC}"
                        if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                            BUILD_ACCEPTANCE_ENFORCED=true
                        fi
                        return 1
                    fi
                    if [ "$(bead_enforcement_status "$PICKED_ID")" != in_progress ]; then
                        echo -e "${RED}Cannot confirm claim for $PICKED_ID; attempting checked reopen${NC}"
                        if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                            BUILD_ACCEPTANCE_ENFORCED=true
                        fi
                        return 1
                    fi
                    ;;
                in_progress)
                    # pick_ready_bead intentionally resumes active work first.
                    # It is already claimed, so continue without rewriting it.
                    ;;
                closed|unavailable)
                    echo -e "${RED}Cannot safely start $PICKED_ID from state=$initial_state; attempting checked reopen${NC}"
                    if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                        BUILD_ACCEPTANCE_ENFORCED=true
                    fi
                    return 1
                    ;;
            esac
            print_picking_up "$PICKED_ID" "$PICKED_TITLE"

            if [ "${CONSEC_FAILURES:-0}" -ge "$STUCK_LOOP_THRESHOLD" ] \
               && [ "$PICKED_ID" = "${LAST_FAILED_PICKED_ID:-}" ]; then
                echo -e "${RED}Stuck-loop: $CONSEC_FAILURES consecutive no-result failures on $PICKED_ID${NC}"
                bd create \
                    --title "Stuck-loop on $PICKED_ID — $CONSEC_FAILURES consecutive timeouts" \
                    --description "loop.sh build mode hit the agent-timeout / no-result-event path on \`$PICKED_ID\` $CONSEC_FAILURES iterations in a row at $(timestamp). Each iteration ran the agent to wall-clock SIGTERM (AGENT_TIMEOUT_SECS=$AGENT_TIMEOUT_SECS) without emitting the terminal stream-json frame. Investigate the bead manually before re-running the loop on it — likely the work needs to be split, the agent is looping on a single failing test, or the timeout needs another bump." \
                    --type=bug \
                    --priority=0 2>/dev/null || true
                if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                    BUILD_ACCEPTANCE_ENFORCED=true
                else
                    return 1
                fi
                return 1
            fi
        fi
    fi

    # --- Run agent (fresh context window) ---
    local evidence_token="" verification_profile="" verification_surfaces="" profile_instruction=""
    local iteration_start_commit iteration_start_epoch trusted_cargo_path="" trusted_cargo_sha=""
    local trusted_rustc_path="" trusted_rustc_sha="" trusted_tmux_path="" trusted_tmux_sha="" rustup_path=""
    iteration_start_commit=$(git rev-parse HEAD 2>/dev/null || echo "")
    iteration_start_epoch=$(date +%s)
    if [ "$MODE" = "build" ] && [ -n "$PICKED_ID" ]; then
        rustup_path=$(type -P rustup 2>/dev/null || true)
        if [[ "$rustup_path" = /* ]] && [ -x "$rustup_path" ]; then
            trusted_cargo_path=$("$rustup_path" which cargo 2>/dev/null || true)
            trusted_rustc_path=$("$rustup_path" which rustc 2>/dev/null || true)
        else
            trusted_cargo_path=$(type -P cargo 2>/dev/null || true)
            trusted_rustc_path=$(type -P rustc 2>/dev/null || true)
        fi
        if [[ "$trusted_cargo_path" != /* ]] || [ ! -x "$trusted_cargo_path" ] \
                || ! trusted_cargo_sha=$(hash_file_sha256 "$trusted_cargo_path") \
                || [[ "$trusted_rustc_path" != /* ]] || [ ! -x "$trusted_rustc_path" ] \
                || ! trusted_rustc_sha=$(hash_file_sha256 "$trusted_rustc_path"); then
            echo -e "${RED}Cannot pin Cargo and rustc before the agent run — leaving $PICKED_ID open${NC}"
            if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                BUILD_ACCEPTANCE_ENFORCED=true
                return 0
            fi
            return 1
        fi
        if ! verification_profile=$(bead_verification_profile "$PICKED_ID"); then
            echo -e "${RED}Cannot derive a verification profile for $PICKED_ID — leaving it open${NC}"
            if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                BUILD_ACCEPTANCE_ENFORCED=true
                return 0
            fi
            return 1
        fi
        if ! verification_surfaces=$(bead_implementation_surfaces "$PICKED_ID" "$verification_profile"); then
            echo -e "${RED}Cannot derive implementation surfaces for $PICKED_ID — leaving it open${NC}"
            if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                BUILD_ACCEPTANCE_ENFORCED=true
                return 0
            fi
            return 1
        fi
        if [ "$verification_profile" = tmux-tui ]; then
            trusted_tmux_path=$(type -P tmux 2>/dev/null || true)
            if [[ "$trusted_tmux_path" != /* ]] || [ ! -x "$trusted_tmux_path" ] \
                    || ! trusted_tmux_sha=$(hash_file_sha256 "$trusted_tmux_path"); then
                echo -e "${RED}Cannot pin tmux before the agent run — leaving $PICKED_ID open${NC}"
                if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                    BUILD_ACCEPTANCE_ENFORCED=true
                    return 0
                fi
                return 1
            fi
        fi
        if ! evidence_token=$(generate_evidence_token); then
            echo -e "${RED}Cannot generate real-binary evidence token — leaving $PICKED_ID open${NC}"
            # Reuse the checked enforcement path; never launch without a token.
            if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                bd comments add "$PICKED_ID" \
                    "[LOOP] Iteration $CURRENT_ITERATION did not run: evidence token generation failed." 2>/dev/null || true
                BUILD_ACCEPTANCE_ENFORCED=true
                return 0
            fi
            return 1
        fi
        case "$verification_profile" in
            connectivity)
                profile_instruction="Use \`Interface: headless\` and \`Artifact: none\`. This bead is explicitly connectivity-specific, so a one-word hello scenario is permitted only if it exercises the changed connectivity path."
                ;;
            tmux-tui)
                profile_instruction="Use \`Interface: tmux-tui\` and \`Artifact: none\`. Commands must be one \`&&\` chain in this strict order: \`tmux new-session -d -s <name>\` (optional \`-x <n> -y <n>\`), \`tmux send-keys -t <same-name> 'mini-agent ...' Enter\`, optional bounded \`sleep <1-99>\` and same-session \`send-keys ... Enter\` interactions, \`tmux capture-pane -t <same-name> -p | grep -Fq -- '<feature-specific literal>'\`, then \`tmux kill-session -t <same-name>\`. The loop pins tmux before the agent, uses a post-agent secret private socket with no user config, replaces the session name, and replaces the initial command with \`exec <fresh-installed-path>\`."
                ;;
            packaged-artifact)
                profile_instruction="Use \`Interface: packaged-artifact\`. Set \`Artifact:\` to a non-symlink executable produced this iteration under this repository (for example \`./target/loop-artifacts/<name>/mini-agent\`). Commands must be exactly that literal path plus feature-driving arguments, piped directly to \`grep -Fq -- '<feature-specific literal>'\`. The loop copies verified bytes to a private path and executes that copy."
                ;;
            *)
                profile_instruction="Use \`Interface: headless\` and \`Artifact: none\`. Commands must be exactly \`mini-agent\` plus feature-driving arguments, piped directly to \`grep -Fq -- '<feature-specific literal>'\`. Generic hello or one-word output is invalid."
                ;;
        esac
    fi
    local prompt_content
    prompt_content=$(cat "$prompt_file")

    # Inject the pre-selected bead ID so the agent doesn't waste calls discovering work
    if [ -n "$PICKED_ID" ]; then
        prompt_content="${prompt_content}

## Pre-selected task
Work on bead \`${PICKED_ID}\`. Run \`bd show ${PICKED_ID}\` to read the details, then implement. Do not pick a different task."

        prompt_content="${prompt_content}

## Required current-iteration installed-app evidence
Before closing this bead, test-drive its actual feature using evidence token \`${evidence_token}\`:
1. Derive a concrete end-to-end scenario from the bead outcome and acceptance criteria.
2. Run \`cargo install --path . --debug\`.
3. Run \`command -v mini-agent\`, then invoke that installed \`mini-agent\` from PATH—not \`cargo run\`, a test, or \`target/debug/mini-agent\`.
4. Force the implemented feature through its public CLI path. For TUI behavior use tmux; for packaging/release behavior execute the exact produced artifact.
5. Compare one specific expected result with the observed result. Generic hello or one-word output is valid only when this is a connectivity-specific bead.
6. Follow this bead-derived evidence profile: ${profile_instruction}
7. Make \`Commands:\` a replayable one-line scenario only; do not include installation or setup/mutation commands. Outside the required tmux chain, it must be one producer piped directly to exactly \`grep -Fq -- '<feature-specific literal>'\`; grep cannot have file operands. Do not use variables, substitutions, redirections, globbing, escapes, subshells, semicolons, \`||\`, nested shells, or PATH changes. The loop independently installs into a private root, substitutes the verified executable path, replays under a private trace nonce and clean shell environment, and requires the assertion to exit zero without mutating the repository.
8. Add exactly one bounded, secret-free Beads comment in this shape (replace every placeholder, but preserve labels and token):
[REAL-BINARY EVIDENCE]
Token: ${evidence_token}
Scenario: <non-empty>
Interface: <headless | tmux-tui | packaged-artifact, exactly as required above>
Artifact: <none or literal current-iteration packaged executable path>
Commands: <one replayable line that drives the feature and mechanically asserts its output>
Expected: <non-empty>
Observed: <non-empty>
Result: PASS

If the scenario fails, cannot run, has no production path, or lacks credentials/platform support, record \`Result: FAIL\` or \`Result: BLOCKED\` in that same shape and leave the bead open."
    fi

    if [ "$MODE" = build ] && [ -n "$PICKED_ID" ]; then
        local launch_state
        launch_state=$(bead_enforcement_status "$PICKED_ID")
        if [ "$launch_state" != in_progress ]; then
            echo -e "${RED}Cannot launch for $PICKED_ID from state=$launch_state; attempting checked reopen${NC}"
            if reopen_build_bead "$PICKED_ID" 1 unavailable; then
                BUILD_ACCEPTANCE_ENFORCED=true
            fi
            return 1
        fi
    fi

    echo -e "${BLUE}Running agent...${NC}"
    local agent_ok=true
    local agent_prefix=()
    if [ -n "$TIMEOUT_BIN" ] && [ "$AGENT_TIMEOUT_SECS" -gt 0 ] 2>/dev/null; then
        agent_prefix=("$TIMEOUT_BIN" "--kill-after=30" "$AGENT_TIMEOUT_SECS")
    fi

    if [ "$AGENT_EXECUTOR" = "codex" ]; then
        local codex_out
        codex_out=$(mktemp)
        if ! run_with_codex_exec "$prompt_content" "$codex_out" "${AGENT_MODEL:-}"; then
            agent_ok=false
        fi
        rm -f "$codex_out"
    elif [ "$AGENT_CMD" = "claude" ]; then
        if ! run_with_claude_agent "$prompt_content" "${agent_prefix[@]}"; then
            agent_ok=false
        fi
    else
        echo "$prompt_content" | $AGENT_CMD || agent_ok=false
    fi

    if [ "$agent_ok" = false ]; then
        echo -e "${RED}Agent invocation failed (process error or no successful result event)${NC}"

        if [ -n "$(git status --porcelain)" ]; then
            local partial_changes
            partial_changes=$(git status --porcelain | grep -v '\.beads/' | head -1)
            if [ -n "$partial_changes" ]; then
                echo -e "${YELLOW}Committing partial progress so the next iteration starts clean...${NC}"
                if stage_non_beads_changes; then
                    git commit -m "loop($label): iteration $iter (timed out — partial)" --no-verify 2>/dev/null || true
                fi
            fi
        fi

        local selected_reopen_ok=true
        if [ "$MODE" = build ] && [ -n "$PICKED_ID" ]; then
            reopen_build_bead "$PICKED_ID" 1 unavailable || selected_reopen_ok=false
        fi

        LAST_FAILED_PICKED_ID="${PICKED_ID:-}"
        CONSEC_FAILURES=$((CONSEC_FAILURES + 1))

        bd dolt push 2>/dev/null || true
        sleep 2
        [ "$selected_reopen_ok" = true ] || return 1
        BUILD_ACCEPTANCE_ENFORCED=true
        return 0
    fi

    CONSEC_FAILURES=0
    LAST_FAILED_PICKED_ID=""

    # --- Post-iteration: commit, verify, then handle bead status ---
    local commit_after iteration_committed=false

    if [ -n "$(git status --porcelain)" ]; then
        local real_changes
        real_changes=$(git status --porcelain | grep -v '\.beads/' | head -1)
        if [ -n "$real_changes" ]; then
            echo -e "${GREEN}Committing changes...${NC}"
            if stage_non_beads_changes; then
                git commit -m "loop($label): iteration $iter" --no-verify 2>/dev/null || true
            fi
        else
            echo -e "${DIM}Only beads DB changed — skipping git commit${NC}"
        fi
    fi

    commit_after=$(git rev-parse HEAD 2>/dev/null || echo "")
    [ -n "$iteration_start_commit" ] && [ -n "$commit_after" ] && [ "$iteration_start_commit" != "$commit_after" ] \
        && iteration_committed=true

    run_verification "$iteration_start_commit" "$commit_after" "$verification_profile" "$verification_surfaces" \
        && local verify_ok=0 || local verify_ok=$?

    # Verification may apply rustfmt/clippy fixes. Never accept those changes
    # uncommitted, and ensure the final commit still passes the whole range.
    if [ "$verify_ok" = "0" ] && [ -n "$(git status --porcelain | grep -v '\.beads/' || true)" ]; then
        echo -e "${GREEN}Committing verification fixes...${NC}"
        if stage_non_beads_changes; then
            git commit -m "loop($label): iteration $iter verification fixes" --no-verify 2>/dev/null || true
        fi
        commit_after=$(git rev-parse HEAD 2>/dev/null || echo "")
        [ -n "$iteration_start_commit" ] && [ "$iteration_start_commit" != "$commit_after" ] \
            && iteration_committed=true
        run_verification "$iteration_start_commit" "$commit_after" "$verification_profile" "$verification_surfaces" \
            && verify_ok=0 || verify_ok=$?
        if [ -n "$(git status --porcelain | grep -v '\.beads/' || true)" ]; then
            echo -e "${RED}FAIL: verification mutated tracked non-Beads files twice${NC}" >&2
            verify_ok=1
        fi
    fi

    if [ "$MODE" = "build" ] && [ -n "$PICKED_ID" ]; then
        local bead_state bead_closed=false code_changed=false evidence_status outcome
        if [ "$iteration_committed" = true ] \
                && current_iteration_has_relevant_changes "$iteration_start_commit" "$commit_after" \
                    "$verification_profile" "$verification_surfaces"; then
            code_changed=true
        fi
        evidence_status=$(real_binary_evidence_status "$PICKED_ID" "$evidence_token" "$verification_profile")
        if [ "$verify_ok" = 0 ] && [ "$iteration_committed" = true ] \
                && [ "$code_changed" = true ] && [ "$evidence_status" = pass ]; then
            echo -e "${CYAN}Replaying nonce-bound feature scenario through the freshly installed app...${NC}"
            if replay_real_binary_evidence "$PICKED_ID" "$evidence_token" \
                    "$verification_profile" "$iteration_start_epoch" \
                    "$trusted_cargo_path" "$trusted_cargo_sha" \
                    "$trusted_rustc_path" "$trusted_rustc_sha" \
                    "$trusted_tmux_path" "$trusted_tmux_sha"; then
                evidence_status=pass
            else
                evidence_status=replay-fail
            fi
        fi

        # Replay runs agent-authored commands, so state is authoritative only
        # after replay and its loop-owned attestation have completed.
        bead_state=$(bead_enforcement_status "$PICKED_ID")
        if [ "$bead_state" = unavailable ]; then
            echo -e "${RED}Cannot enforce acceptance: bead state unavailable${NC}" >&2
            reopen_build_bead "$PICKED_ID" 1 unavailable || return 1
            bead_state=open
        fi
        [ "$bead_state" = closed ] && bead_closed=true
        outcome=$(decide_build_outcome "$bead_closed" "$verify_ok" "$evidence_status" "$iteration_committed" "$code_changed")
        echo -e "${BOLD}Real-binary evidence:${NC} ${evidence_status}"

        if [ "$outcome" = reopen ]; then
            echo -e "${YELLOW}Reopening $PICKED_ID — acceptance gates did not both pass${NC}"
            if ! reopen_build_bead "$PICKED_ID" "$verify_ok" "$evidence_status"; then
                print_completed "$PICKED_ID" "$PICKED_TITLE" "partial"
                return 1
            fi
            bead_closed=false
        fi

        if [ "$outcome" = partial ] && [ "$bead_state" = in_progress ]; then
            echo -e "${YELLOW}Reopening $PICKED_ID — resumed work did not pass acceptance gates${NC}"
            if ! reopen_build_bead "$PICKED_ID" "$verify_ok" "$evidence_status"; then
                print_completed "$PICKED_ID" "$PICKED_TITLE" "partial"
                return 1
            fi
            bead_state=open
        fi

        if [ "$outcome" = auto-close ]; then
            echo -e "${YELLOW}Auto-closing $PICKED_ID (verification and real-binary evidence passed)${NC}"
            if auto_close_build_bead "$PICKED_ID" "$(git log -1 --pretty=%h)"; then
                bead_closed=true
            else
                # A failed close is not a confirmed partial/open state.
                reopen_build_bead "$PICKED_ID" 1 unavailable || return 1
                outcome=partial
            fi
        fi

        if [ "$outcome" = accept ] || [ "$outcome" = auto-close ]; then
            BUILD_ACCEPTANCE_ENFORCED=true
            print_completed "$PICKED_ID" "$PICKED_TITLE" "closed"
        else
            # Reopen and partial paths have both explicitly confirmed OPEN.
            BUILD_ACCEPTANCE_ENFORCED=true
            print_completed "$PICKED_ID" "$PICKED_TITLE" "partial"
        fi
    fi

    # --- Codex second-opinion pass (optional) ---
    if [ "$CODEX_VERIFY" = true ] && [ "$MODE" = "build" ] && [ -n "$PICKED_ID" ]; then
        echo -e "${CYAN}Running Codex second-opinion review...${NC}"
        local codex_prompt="Review the changes made for bead ${PICKED_ID} in the mini-agent project. Run 'git diff HEAD~1' to see what changed, then 'cargo test --workspace' to verify. Report any issues found."
        "${CODEX_COMPANION_CMD[@]}" task "$codex_prompt" 2>/dev/null || echo -e "${DIM}Codex review skipped (unavailable)${NC}"
    fi

    if [ "$CODEX_VERIFY" = true ] && [ "$MODE" = "review" ]; then
        echo -e "${CYAN}Running Codex cross-check on review findings...${NC}"
        local codex_prompt="Cross-check the latest review findings filed as beads. Run 'bd list -n 5' to see recent beads. For each P0/P1 finding, verify it's a real issue by reading the referenced code. Report false positives."
        "${CODEX_COMPANION_CMD[@]}" task "$codex_prompt" 2>/dev/null || echo -e "${DIM}Codex cross-check skipped (unavailable)${NC}"
    fi

    # Batch dolt pushes to reduce GC churn; exit handler ensures final flush.
    : "${DOLT_PUSH_EVERY:=3}"
    if [ "$((iter % DOLT_PUSH_EVERY))" -eq 0 ]; then
        bd dolt push 2>/dev/null || true
    fi

    return 0
}

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Review: Run One Domain (1+ iterations, each with fresh context) ║
# ╚══════════════════════════════════════════════════════════════════╝

# run_review_domain <domain> <max_iterations> <domain_index> <total_domains>
run_review_domain() {
    local domain="$1" max_iters="$2" idx="$3" total="$4"
    local prompt_file="$SCRIPT_DIR/PROMPT_review_${domain}.md"

    if [ ! -f "$prompt_file" ]; then
        echo -e "${RED}Skipping $domain — prompt file not found: $prompt_file${NC}"
        return 0
    fi

    local tier="1"
    case "$domain" in
        arch|deps|compound) tier="2" ;;
        debate|synthesis) tier="3" ;;
    esac

    echo ""
    echo -e "${MAGENTA}╔══════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${MAGENTA}║  Review Domain ${idx}/${total}: ${BOLD}${domain}${NC}${MAGENTA}  (Tier ${tier})                       ║${NC}"
    echo -e "${MAGENTA}╚══════════════════════════════════════════════════════════════════╝${NC}"

    local ids_before
    ids_before=$( (bd list --limit 0 --status open 2>/dev/null || true) | extract_bead_ids)

    local iter=0
    while [ "$iter" -lt "$max_iters" ]; do
        iter=$((iter + 1))
        CURRENT_DOMAIN="$domain"
        run_iteration "$prompt_file" "review:$domain" "$iter" "$max_iters" || break
    done

    local ids_after new_ids created
    ids_after=$( (bd list --limit 0 --status open 2>/dev/null || true) | extract_bead_ids)
    new_ids=$(comm -13 <(printf '%s\n' "$ids_before") <(printf '%s\n' "$ids_after"))
    created=$(printf '%s\n' "$new_ids" | grep -c 'mini-agent-' || true)
    created=${created:-0}
    TOTAL_REVIEW_FINDINGS=$((TOTAL_REVIEW_FINDINGS + created))

    echo -e "${MAGENTA}  Domain ${BOLD}$domain${NC}${MAGENTA} complete — ${created} new beads filed${NC}"
}

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Decompose: Recursive Bead Decomposition (Codex QC is opt-in)    ║
# ╚══════════════════════════════════════════════════════════════════╝

count_dN_beads() {
    local depth="$1"
    local n
    n=$( (bd list --limit 0 --status open 2>/dev/null || true) \
        | grep -cE "\[D${depth}\]" 2>/dev/null || echo 0)
    printf '%s\n' "$n" | head -1
}

count_ready_beads() {
    local n
    n=$( (bd list --limit 0 --status open 2>/dev/null || true) \
        | grep -cE ':READY:' 2>/dev/null || echo 0)
    printf '%s\n' "$n" | head -1
}

count_qc_beads() {
    local n
    n=$( (bd list --limit 0 --status open 2>/dev/null || true) \
        | grep -cE '\[QC\]' 2>/dev/null || echo 0)
    printf '%s\n' "$n" | head -1
}

decompose_census() {
    printf 'D0=%s D1=%s D2=%s D3=%s D4=%s D5=%s READY=%s QC=%s OPEN=%s' \
        "$(count_dN_beads 0)" "$(count_dN_beads 1)" "$(count_dN_beads 2)" \
        "$(count_dN_beads 3)" "$(count_dN_beads 4)" "$(count_dN_beads 5)" \
        "$(count_ready_beads)" "$(count_qc_beads)" \
        "$( (bd list --limit 0 --status open 2>/dev/null || true) | count_lines)"
}

run_decompose_qc() {
    local round="$1" new_ids="$2"

    if [ "$CODEX_VERIFY" != true ]; then
        echo -e "${DIM}  (Codex QC opt-out — pass --codex-verify to enable; defaulting to CONTINUE)${NC}" >&2
        echo "CONTINUE"
        return
    fi

    local qc_prompt_file="$SCRIPT_DIR/PROMPT_decompose_qc.md"
    if [ ! -f "$qc_prompt_file" ]; then
        echo -e "${DIM}  (QC prompt file missing — skipping QC, defaulting to CONTINUE)${NC}" >&2
        echo "CONTINUE"
        return
    fi

    local census
    census=$(decompose_census)

    local qc_prompt
    qc_prompt=$(cat "$qc_prompt_file")
    qc_prompt+="

## Round context (injected by loop.sh)
- Round number: ${round}
- Bead IDs newly created this round: ${new_ids:-(none)}
- Current bead census: ${census}
"

    echo -e "${CYAN}  Running Codex QC review for round ${round}...${NC}" >&2
    local qc_output verdict
    if qc_output=$("${CODEX_COMPANION_CMD[@]}" task "$qc_prompt" 2>&1); then
        verdict=$(printf '%s\n' "$qc_output" \
            | grep -oE '^VERDICT:[[:space:]]+(STOP|CONTINUE_AFTER_FIXES|CONTINUE)' \
            | head -1 | awk '{print $2}')
        if [ -z "$verdict" ]; then
            echo -e "${YELLOW}  Codex returned no parseable VERDICT — defaulting to CONTINUE${NC}" >&2
            verdict="CONTINUE"
        fi
        echo -e "${CYAN}  Codex verdict: ${BOLD}${verdict}${NC}" >&2
    else
        echo -e "${DIM}  Codex companion unavailable — defaulting to CONTINUE${NC}" >&2
        verdict="CONTINUE"
    fi
    echo "$verdict"
}

run_decompose_loop() {
    local max_rounds="$1"
    local prompt_file="$SCRIPT_DIR/PROMPT_decompose.md"
    if [ ! -f "$prompt_file" ]; then
        echo -e "${RED}Error: $prompt_file not found${NC}"
        exit 1
    fi

    local round=0
    local stop_votes=0
    local prev_total=0
    local low_growth_streak=0

    while [ "$round" -lt "$max_rounds" ]; do
        round=$((round + 1))
        CURRENT_ITERATION=$round

        echo ""
        echo -e "${MAGENTA}╔══════════════════════════════════════════════════════════════════╗${NC}"
        echo -e "${MAGENTA}║  DECOMPOSE Round ${round}/${max_rounds}  —  $(timestamp)             ${NC}${MAGENTA}║${NC}"
        echo -e "${MAGENTA}╚══════════════════════════════════════════════════════════════════╝${NC}"

        print_open_beads

        local ids_before
        ids_before=$( (bd list --limit 0 --status open 2>/dev/null || true) | extract_bead_ids)
        local ready_before qc_before total_before census_before
        ready_before=$(count_ready_beads)
        qc_before=$(count_qc_beads)
        total_before=$(printf '%s\n' "$ids_before" | grep -c 'mini-agent-' || true)
        total_before=${total_before:-0}
        census_before=$(decompose_census)

        local prompt_content
        prompt_content=$(cat "$prompt_file")
        prompt_content+="

## Round context (injected by loop.sh)
- Round number: ${round}/${max_rounds}
- Bead census: ${census_before}
- Open QC beads to address first: ${qc_before}
- Total open beads: ${total_before}

If QC beads are open, address them first per PROMPT_decompose.md's QC-first rule.
Stop this round once you've completed one full pass at the targeted depth — do not
race to deeper layers; the next round's fresh context will pick that up.
"

        echo -e "${BLUE}Running decomposition agent (round ${round})...${NC}"
        local agent_ok=true
        if [ "$AGENT_EXECUTOR" = "codex" ]; then
            local codex_out
            codex_out=$(mktemp)
            if ! run_with_codex_exec "$prompt_content" "$codex_out" "${AGENT_MODEL:-}"; then
                agent_ok=false
            fi
            rm -f "$codex_out"
        elif [ "$AGENT_CMD" = "claude" ]; then
            if ! run_with_claude_agent "$prompt_content"; then
                agent_ok=false
            fi
        else
            echo "$prompt_content" | $AGENT_CMD || agent_ok=false
        fi

        if [ "$agent_ok" = false ]; then
            echo -e "${RED}Decomposition agent failed in round ${round} — bailing out${NC}"
            break
        fi

        if [ -n "$(git status --porcelain 2>/dev/null | grep -v '\.beads/' | head -1)" ]; then
            if stage_non_beads_changes; then
                git commit -m "loop(decompose): round $round" --no-verify 2>/dev/null || true
            fi
        fi

        local ids_after new_ids new_ids_inline
        ids_after=$( (bd list --limit 0 --status open 2>/dev/null || true) | extract_bead_ids)
        new_ids=$(comm -13 <(printf '%s\n' "$ids_before") <(printf '%s\n' "$ids_after"))
        new_ids_inline=$(printf '%s ' $new_ids | sed 's/[[:space:]]*$//')

        local ready_after qc_after total_after created
        ready_after=$(count_ready_beads)
        qc_after=$(count_qc_beads)
        total_after=$(printf '%s\n' "$ids_after" | grep -c 'mini-agent-' || true)
        total_after=${total_after:-0}
        created=$(printf '%s\n' "$new_ids" | grep -c 'mini-agent-' || true)
        created=${created:-0}

        echo ""
        echo -e "${MAGENTA}  Round ${round} delta: +${created} new beads · READY ${ready_before}→${ready_after} · QC ${qc_before}→${qc_after} · OPEN ${total_before}→${total_after}${NC}"

        # Exit condition 1: no-op round
        if [ "$created" -eq 0 ] \
            && [ "$ready_after" = "$ready_before" ] \
            && [ "$qc_after" = "$qc_before" ]; then
            echo -e "${YELLOW}  No-op round detected (no new beads, no READY flips, no QC change). Exiting.${NC}"
            break
        fi

        local verdict
        verdict=$(run_decompose_qc "$round" "$new_ids_inline")

        case "$verdict" in
            STOP)
                stop_votes=$((stop_votes + 1))
                echo -e "${MAGENTA}  Codex STOP vote ${stop_votes}/2${NC}"
                if [ "$stop_votes" -ge 2 ]; then
                    echo -e "${GREEN}  Two consecutive STOP verdicts — decomposition saturated. Exiting.${NC}"
                    break
                fi
                ;;
            CONTINUE_AFTER_FIXES)
                stop_votes=0
                echo -e "${YELLOW}  QC findings filed — next round will address them first${NC}"
                ;;
            CONTINUE|*)
                stop_votes=0
                ;;
        esac

        # Exit condition 3: diminishing returns
        if [ "$prev_total" -gt 0 ]; then
            local growth_pct=$(( (created * 100) / prev_total ))
            if [ "$growth_pct" -lt "$DECOMPOSE_LOW_GROWTH_PCT" ]; then
                low_growth_streak=$((low_growth_streak + 1))
                echo -e "${DIM}  Low-growth round (${growth_pct}% < ${DECOMPOSE_LOW_GROWTH_PCT}%, streak ${low_growth_streak}/2)${NC}"
            else
                low_growth_streak=0
            fi
        fi
        prev_total=$total_after

        if [ "$low_growth_streak" -ge 2 ]; then
            echo -e "${GREEN}  Diminishing returns (<${DECOMPOSE_LOW_GROWTH_PCT}% growth × 2 rounds) — exiting.${NC}"
            break
        fi

        bd dolt push 2>/dev/null || true
        sleep 2
    done

    echo ""
    echo -e "${MAGENTA}══════════════════════════════════════════════════════════════════${NC}"
    echo -e "${MAGENTA}  Decomposition loop done after ${round} round(s) — $(timestamp)${NC}"
    echo -e "${MAGENTA}══════════════════════════════════════════════════════════════════${NC}"
    print_open_beads

    local final_d0 final_d1 final_d2plus final_ready final_qc
    final_d0=$(count_dN_beads 0)
    final_d1=$(count_dN_beads 1)
    final_d2plus=$(( $(count_dN_beads 2) + $(count_dN_beads 3) + $(count_dN_beads 4) + $(count_dN_beads 5) ))
    final_ready=$(count_ready_beads)
    final_qc=$(count_qc_beads)
    echo -e "${BOLD}  Final tally: D0=${final_d0} D1=${final_d1} D2+=${final_d2plus} READY=${final_ready} QC=${final_qc}${NC}"
}

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Interrupt Handler                                               ║
# ╚══════════════════════════════════════════════════════════════════╝

handle_interrupt() {
    # Cleanup must be single-entry: a second terminal signal during Beads or
    # Git operations must not nest another handler invocation.
    trap '' SIGINT SIGTERM
    echo ""
    echo -e "${YELLOW}── Interrupted at $(timestamp) ──${NC}"

    if [ "$USE_BEADS" = true ]; then
        if [ -n "${PICKED_ID:-}" ]; then
            bd comments add "$PICKED_ID" \
                "[INTERRUPTED] iteration $CURRENT_ITERATION domain=$CURRENT_DOMAIN ($(timestamp))" \
                2>/dev/null || true
            if [ "$MODE" = build ] && [ "${BUILD_ACCEPTANCE_ENFORCED:-true}" != true ]; then
                if ! reopen_build_bead "$PICKED_ID" 1 unavailable; then
                    echo -e "${RED}${BOLD}INTERRUPT SAFETY FAILURE: could not confirm $PICKED_ID open${NC}" >&2
                else
                    BUILD_ACCEPTANCE_ENFORCED=true
                fi
            fi
        fi
        bd dolt push 2>/dev/null || true
    fi

    local partial_changes
    partial_changes=$(git status --porcelain 2>/dev/null | grep -v '\.beads/' | head -1)
    if [ -n "$partial_changes" ]; then
        echo -e "${YELLOW}Committing partial changes...${NC}"
        if stage_non_beads_changes; then
            git commit -m "loop: interrupted at iteration $CURRENT_ITERATION" --no-verify 2>/dev/null || true
        fi
    fi

    echo -e "${YELLOW}Run ./scripts/loop.sh again to resume.${NC}"
    exit 130
}

handle_termination() {
    trap '' SIGINT SIGTERM
    echo -e "\n${YELLOW}── Terminated at $(timestamp); EXIT safety will reopen any unenforced build bead ──${NC}" >&2
    exit 143
}

trap handle_interrupt SIGINT
trap handle_termination SIGTERM

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Startup                                                         ║
# ╚══════════════════════════════════════════════════════════════════╝

echo -e "${BLUE}╔══════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║           mini-agent Loop v1.0                                  ║${NC}"
echo -e "${BLUE}║           $(timestamp)                                    ║${NC}"
echo -e "${BLUE}╚══════════════════════════════════════════════════════════════════╝${NC}"

if [ "$MODE" = "review" ]; then
    echo -e "  Mode:       ${GREEN}review — ${BOLD}$REVIEW_DOMAIN${NC}"
else
    echo -e "  Mode:       ${GREEN}$MODE${NC}"
fi
echo -e "  Max iters:  ${YELLOW}$MAX_ITERATIONS${NC}"
if [ "$AGENT_EXECUTOR" = "codex" ]; then
    echo -e "  Executor:   ${BLUE}codex exec${NC} (model: ${AGENT_MODEL})"
elif [ -n "${AGENT_MODEL:-}" ]; then
    echo -e "  Executor:   ${BLUE}$AGENT_CMD${NC} (model: ${AGENT_MODEL})"
else
    echo -e "  Executor:   ${BLUE}$AGENT_CMD${NC}"
fi
echo -e "  Project:    ${CYAN}mini-agent (minimalistic coding agent with built-in JS engine)${NC}"

# Require beads
if command -v bd &> /dev/null; then
    USE_BEADS=true
    [ ! -d ".beads" ] && bd init
else
    echo -e "\n${RED}Error: 'bd' (beads) is required.${NC}"
    exit 1
fi

# Init codex home only when codex executor is selected
[ "$AGENT_EXECUTOR" = "codex" ] && setup_codex_home

# Pre-flight: refuse to start with a dirty working tree. Otherwise the first
# iteration's commit sweeps up unrelated pre-existing changes as "loop work".
dirty=$(git status --porcelain 2>/dev/null | grep -v '\.beads/' || true)
if [ -n "$dirty" ]; then
    echo ""
    echo -e "${RED}Error: working tree is dirty. Refusing to start.${NC}"
    echo -e "${DIM}The loop commits after each iteration; starting dirty would attribute"
    echo -e "pre-existing changes to the agent.${NC}"
    echo ""
    echo -e "${YELLOW}Dirty files (excluding .beads/):${NC}"
    echo "$dirty" | sed 's/^/  /'
    echo ""
    echo -e "${DIM}Commit, stash, or discard these before running the loop.${NC}"
    exit 1
fi

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Main Dispatch                                                   ║
# ╚══════════════════════════════════════════════════════════════════╝

if [ "$MODE" = "review" ]; then
    if [ -z "$REVIEW_DOMAIN" ]; then
        echo -e "${RED}Error: review mode requires a domain.${NC}"
        echo -e "  Domains: bugs, security, perf, orphans, missing, quality, arch, deps, compound, debate, synthesis, all"
        exit 1
    fi

    if [ "$REVIEW_DOMAIN" = "all" ]; then
        review_total=${#REVIEW_ALL_DOMAINS[@]}
        echo ""
        echo -e "${MAGENTA}Running full review: ${review_total} domains in 3 tiers${NC}"
        echo -e "${DIM}  Tier 1 (analysis):     ${REVIEW_TIER1[*]}${NC}"
        echo -e "${DIM}  Tier 2 (cross-cutting): ${REVIEW_TIER2[*]}${NC}"
        echo -e "${DIM}  Tier 3 (QC):           ${REVIEW_TIER3[*]}${NC}"

        domain_idx=0
        for tier_name in "Tier 1: Analysis" "Tier 2: Cross-Cutting" "Tier 3: QC"; do
            echo ""
            echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
            echo -e "${MAGENTA}  ${tier_name}${NC}"
            echo -e "${MAGENTA}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

            tier_domains=()
            case "$tier_name" in
                "Tier 1"*) tier_domains=("${REVIEW_TIER1[@]}") ;;
                "Tier 2"*) tier_domains=("${REVIEW_TIER2[@]}") ;;
                "Tier 3"*) tier_domains=("${REVIEW_TIER3[@]}") ;;
            esac

            for domain in "${tier_domains[@]}"; do
                domain_idx=$((domain_idx + 1))
                run_review_domain "$domain" 1 "$domain_idx" "$review_total"
            done

            if bd dolt push 2>/dev/null; then
                echo -e "${MAGENTA}  ${tier_name} complete — beads synced${NC}"
            else
                echo -e "${YELLOW}  ${tier_name} complete — beads sync FAILED (later tiers may be stale)${NC}"
            fi
        done
    else
        run_review_domain "$REVIEW_DOMAIN" "$MAX_ITERATIONS" 1 1
    fi

elif [ "$MODE" = "decompose" ]; then
    run_decompose_loop "$MAX_ITERATIONS"

else
    # --- Build / Plan mode ---
    PROMPT_FILE="$SCRIPT_DIR/PROMPT_${MODE}.md"
    if [ ! -f "$PROMPT_FILE" ]; then
        echo -e "${RED}Error: $PROMPT_FILE not found${NC}"
        exit 1
    fi

    iteration=0
    while [ "$iteration" -lt "$MAX_ITERATIONS" ]; do
        iteration=$((iteration + 1))
        run_iteration "$PROMPT_FILE" "$MODE" "$iteration" "$MAX_ITERATIONS" || break
    done
fi

# ╔══════════════════════════════════════════════════════════════════╗
# ║  Summary                                                         ║
# ╚══════════════════════════════════════════════════════════════════╝

echo ""
echo -e "${BLUE}══════════════════════════════════════════════════════════════════${NC}"
echo -e "${BLUE}  Loop Complete  —  $(timestamp)${NC}"
echo -e "${BLUE}══════════════════════════════════════════════════════════════════${NC}"

if [ "$MODE" = "review" ]; then
    echo -e "  Mode:       review ($REVIEW_DOMAIN)"
    echo -e "  Findings:   ${YELLOW}$TOTAL_REVIEW_FINDINGS new beads${NC}"
    print_open_beads
elif [ "$MODE" = "decompose" ]; then
    echo -e "  Mode:       decompose"
else
    echo -e "  Mode:       $MODE"
    print_open_beads
fi
