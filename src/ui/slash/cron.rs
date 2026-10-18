//! `/cron`: arm, list, stop and run session-local recurring prompts.
//!
//! This handler only edits the [`CronStore`]; it never starts a turn. Entries
//! fire when the TUI is idle, so every verb is safe while a run is active.
//!
//! Grammar, decided by the first token after `/cron`:
//! - none, or `list`: list the schedules;
//! - `stop` / `run`: the verbs (any further text is a usage error, so a
//!   prompt can never start with a verb word);
//! - an interval token (`^\d+(s|m|h)$`): an interval entry with the rest as
//!   its payload;
//! - anything else: a self-paced (dynamic) entry whose payload is the whole
//!   remainder. Its first fire runs at the next idle point and each fire's
//!   turn chooses the next one through the `schedule_wakeup` tool.
use super::{write_error, write_ok, write_result};
use crate::ui::cron::{self, ArmError, CronStore};
use crate::ui::renderer::Renderer;
use tokio::time::Instant;

const USAGE: &str = "usage: /cron <interval> <prompt>  |  /cron <prompt>  |  /cron list  |  /cron stop <id>|all  |  /cron run <id>";

/// Slash payloads an interval entry may schedule, matched against the whole
/// trimmed payload split by `split_command`. Each one only reads state and
/// prints: it never starts a model run, edits the session, or touches a
/// running turn (`/goal status` reads the goal store, `/tasks` and `/jobs`
/// list background work, `/cost` reads the spend ledger). A fire runs it
/// through the normal slash dispatcher at idle instead of starting a turn.
pub(crate) const CRON_SLASH_ALLOWLIST: &[&[&str]] =
    &[&["/goal", "status"], &["/tasks"], &["/jobs"], &["/cost"]];

/// Whether `payload` is exactly one of [`CRON_SLASH_ALLOWLIST`].
pub(crate) fn is_allowlisted_slash_payload(payload: &str) -> bool {
    let parts = super::split_command(payload);
    CRON_SLASH_ALLOWLIST
        .iter()
        .any(|allowed| parts.as_slice() == *allowed)
}

/// Why an interval payload cannot be scheduled, or `None` for a plain prompt
/// or an allowlisted read-only slash command.
pub(crate) fn payload_refusal(payload: &str) -> Option<&'static str> {
    match payload.trim_start().chars().next() {
        Some('/') if is_allowlisted_slash_payload(payload) => None,
        Some('/') => Some(
            "cron payloads are plain prompts or one of /goal status, /tasks, /jobs, /cost; other slash commands cannot be scheduled",
        ),
        Some('!') => Some("cron payloads are plain prompts; shell (!) lines cannot be scheduled"),
        _ => None,
    }
}

/// Why a dynamic payload cannot be scheduled. A self-paced entry needs a
/// model turn to call `schedule_wakeup`, so no slash payload qualifies.
fn dynamic_payload_refusal(payload: &str) -> Option<&'static str> {
    match payload.trim_start().chars().next() {
        Some('/') => {
            Some("self-paced cron payloads are plain prompts; slash commands cannot be scheduled")
        }
        Some('!') => Some("cron payloads are plain prompts; shell (!) lines cannot be scheduled"),
        _ => None,
    }
}

/// Handle one `/cron` line already split by `split_command` into at most
/// three fields. `budget_capped` says whether a USD ceiling is configured.
pub(crate) fn handle(
    parts: &[&str],
    store: &mut CronStore,
    renderer: &mut Renderer,
    max_fires: u32,
    budget_capped: bool,
    now: Instant,
) {
    match parts {
        ["/cron"] | ["/cron", "list"] => list(store, renderer, max_fires, now),
        ["/cron", "stop", "all"] => {
            let count = store.stop_all();
            write_ok(renderer, format!("cron: stopped {count} schedule(s)"));
        }
        ["/cron", "stop", id] if !id.contains(char::is_whitespace) => {
            if store.stop(id) {
                write_ok(renderer, format!("cron {id}: stopped"));
            } else {
                write_error(renderer, format!("cron: no schedule {id} (see /cron list)"));
            }
        }
        ["/cron", "run", id] if !id.contains(char::is_whitespace) => {
            if store.mark_due(id) {
                write_ok(
                    renderer,
                    format!("cron {id}: marked due; it fires at the next idle point"),
                );
            } else {
                write_error(renderer, format!("cron: no schedule {id} (see /cron list)"));
            }
        }
        ["/cron", interval, prompt] if cron::is_interval_token(interval) => arm(
            interval,
            prompt,
            store,
            renderer,
            max_fires,
            budget_capped,
            now,
        ),
        ["/cron", interval] if cron::is_interval_token(interval) => {
            write_error(renderer, "usage: /cron <interval> <prompt>");
        }
        ["/cron", "list" | "stop" | "run", ..] => write_error(renderer, USAGE),
        ["/cron", first, rest @ ..] => {
            let prompt = std::iter::once(*first)
                .chain(rest.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            arm_dynamic(&prompt, store, renderer, max_fires, budget_capped, now);
        }
        _ => write_error(renderer, USAGE),
    }
}

fn arm_dynamic(
    prompt: &str,
    store: &mut CronStore,
    renderer: &mut Renderer,
    max_fires: u32,
    budget_capped: bool,
    now: Instant,
) {
    let prompt = prompt.trim();
    if let Some(reason) = dynamic_payload_refusal(prompt) {
        write_error(renderer, format!("cron: {reason}"));
        return;
    }
    // `/cron every 5m x` most likely meant an interval entry; refuse rather
    // than arm a self-paced one the user did not ask for.
    if prompt
        .split_whitespace()
        .nth(1)
        .is_some_and(cron::is_interval_token)
    {
        write_error(
            renderer,
            "cron: for a fixed interval put it first (/cron 5m <prompt>); a self-paced prompt must not start with an interval",
        );
        return;
    }
    let id = match store.arm_dynamic(prompt.to_owned(), now) {
        Ok(entry) => entry.id.clone(),
        Err(ArmError::Full) => {
            write_error(
                renderer,
                format!(
                    "cron: at most {} schedules per session; /cron stop one first",
                    cron::CRON_MAX_ENTRIES
                ),
            );
            return;
        }
    };
    write_ok(
        renderer,
        format!(
            "cron {id}: self-paced, first fire at the next idle point; the model picks each next fire ({}..{}), up to {max_fires} fires",
            cron::format_duration(std::time::Duration::from_secs(
                crate::agent::tools::schedule_wakeup::WAKEUP_MIN_SECS
            )),
            cron::format_duration(std::time::Duration::from_secs(
                crate::agent::tools::schedule_wakeup::WAKEUP_MAX_SECS
            )),
        ),
    );
    if !budget_capped && store.take_budget_warning() {
        write_result(
            renderer,
            "cron: no max_budget_usd is set, so scheduled turns are unbounded in spend",
        );
    }
}

fn list(store: &CronStore, renderer: &mut Renderer, max_fires: u32, now: Instant) {
    if store.is_empty() {
        write_ok(renderer, "no cron schedules in this session");
        write_result(renderer, USAGE);
        return;
    }
    for entry in store.entries() {
        let next = if entry.due {
            "due at next idle".to_owned()
        } else if entry.in_flight {
            "running".to_owned()
        } else if store.is_suspended() {
            "suspended".to_owned()
        } else {
            cron::format_until(entry.next_at, now)
        };
        let payload: String = crate::ui::events::sanitize_output(&entry.payload)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        let every = if entry.dynamic {
            "self-paced".to_owned()
        } else {
            format!("every {}", cron::format_duration(entry.interval))
        };
        write_ok(
            renderer,
            format!(
                "{}  {every}  {next}  {}/{max_fires}  {payload}",
                entry.id, entry.fires,
            ),
        );
    }
}

fn arm(
    interval: &str,
    prompt: &str,
    store: &mut CronStore,
    renderer: &mut Renderer,
    max_fires: u32,
    budget_capped: bool,
    now: Instant,
) {
    let interval = match cron::parse_interval(interval) {
        Ok(interval) => interval,
        Err(error) => {
            write_error(renderer, format!("cron: {error}"));
            return;
        }
    };
    let prompt = prompt.trim();
    if let Some(reason) = payload_refusal(prompt) {
        write_error(renderer, format!("cron: {reason}"));
        return;
    }
    let entry = match store.arm(interval.duration, prompt.to_owned(), now) {
        Ok(entry) => entry,
        Err(ArmError::Full) => {
            write_error(
                renderer,
                format!(
                    "cron: at most {} schedules per session; /cron stop one first",
                    cron::CRON_MAX_ENTRIES
                ),
            );
            return;
        }
    };
    let id = entry.id.clone();
    let every = cron::format_duration(entry.interval);
    if interval.rounded_up {
        write_result(
            renderer,
            format!(
                "cron: interval raised to the {} minimum",
                cron::format_duration(cron::CRON_MIN_INTERVAL)
            ),
        );
    }
    write_ok(
        renderer,
        format!(
            "cron {id}: every {every}, first fire {}; up to {max_fires} fires, only while idle",
            cron::format_until(now + interval.duration, now)
        ),
    );
    if !budget_capped && store.take_budget_warning() {
        write_result(
            renderer,
            "cron: no max_budget_usd is set, so scheduled turns are unbounded in spend",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::slash::split_command;

    fn run(text: &str, store: &mut CronStore, budget_capped: bool, now: Instant) -> Vec<String> {
        let mut renderer = Renderer::new().unwrap();
        handle(
            &split_command(text),
            store,
            &mut renderer,
            24,
            budget_capped,
            now,
        );
        let feed = renderer.feed();
        (0..feed.block_count())
            .map(|index| feed.block_text(index).unwrap().to_string())
            .collect()
    }

    #[test]
    fn cron_arms_and_lists_with_relative_times() {
        let now = Instant::now();
        let mut store = CronStore::default();
        let lines = run("/cron 5m check the deploy", &mut store, true, now);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("cron c1: every 5m, first fire in 5m")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("unbounded")), "{lines:?}");

        for listing in ["/cron", "/cron list"] {
            let lines = run(
                listing,
                &mut store,
                true,
                now + std::time::Duration::from_secs(60),
            );
            assert_eq!(lines, ["c1  every 5m  in 4m  0/24  check the deploy"]);
        }
    }

    #[test]
    fn cron_list_when_empty_shows_usage() {
        let mut store = CronStore::default();
        let lines = run("/cron list", &mut store, true, Instant::now());
        assert_eq!(lines[0], "no cron schedules in this session");
        assert!(lines[1].starts_with("usage: /cron"));
    }

    #[test]
    fn cron_stop_one_and_all() {
        let now = Instant::now();
        let mut store = CronStore::default();
        run("/cron 1m a", &mut store, true, now);
        run("/cron 2m b", &mut store, true, now);
        run("/cron 3m c", &mut store, true, now);
        assert_eq!(
            run("/cron stop c2", &mut store, true, now),
            ["cron c2: stopped"]
        );
        assert_eq!(
            run("/cron stop c2", &mut store, true, now),
            ["cron: no schedule c2 (see /cron list)"]
        );
        assert_eq!(
            run("/cron stop all", &mut store, true, now),
            ["cron: stopped 2 schedule(s)"]
        );
        assert!(store.is_empty());
    }

    #[test]
    fn cron_run_marks_the_entry_due_for_the_next_idle_point() {
        let now = Instant::now();
        let mut store = CronStore::default();
        run("/cron 10m a", &mut store, true, now);
        assert_eq!(
            run("/cron run c1", &mut store, true, now),
            ["cron c1: marked due; it fires at the next idle point"]
        );
        assert!(store.get("c1").unwrap().due);
        assert_eq!(
            run("/cron list", &mut store, true, now),
            ["c1  every 10m  due at next idle  0/24  a"]
        );
        assert_eq!(
            run("/cron run c7", &mut store, true, now),
            ["cron: no schedule c7 (see /cron list)"]
        );
    }

    #[test]
    fn cron_refuses_slash_and_shell_payloads() {
        let now = Instant::now();
        let mut store = CronStore::default();
        for (payload, word) in [("/x", "slash"), ("  /cron 1m y", "slash"), ("!x", "shell")] {
            let lines = run(&format!("/cron 5m {payload}"), &mut store, true, now);
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(
                lines[0].starts_with("cron: cron payloads are plain prompts")
                    && lines[0].contains(word),
                "{lines:?}"
            );
        }
        assert!(store.is_empty());
    }

    #[test]
    fn cron_rounds_short_intervals_up_and_refuses_bad_ones() {
        let now = Instant::now();
        let mut store = CronStore::default();
        let lines = run("/cron 10s ping", &mut store, true, now);
        assert_eq!(lines[0], "cron: interval raised to the 1m minimum");
        assert_eq!(store.get("c1").unwrap().interval, cron::CRON_MIN_INTERVAL);
        assert_eq!(
            run("/cron 25h x", &mut store, true, now),
            ["cron: interval must be at most 24h"]
        );
        assert_eq!(
            run("/cron 0m x", &mut store, true, now),
            ["cron: interval must be greater than zero"]
        );
        assert_eq!(
            run("/cron 5m", &mut store, true, now),
            ["usage: /cron <interval> <prompt>"]
        );
        assert!(
            run("/cron every 5m x", &mut store, true, now)[0]
                .starts_with("cron: for a fixed interval put it first")
        );
        assert!(run("/cron stop c1 now", &mut store, true, now)[0].starts_with("usage: /cron"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn cron_without_an_interval_arms_a_self_paced_entry() {
        let now = Instant::now();
        let mut store = CronStore::default();
        let lines = run("/cron watch the   CI run", &mut store, true, now);
        assert!(
            lines[0].starts_with("cron c1: self-paced, first fire at the next idle point")
                && lines[0].contains("(1m..1h)"),
            "{lines:?}"
        );
        let entry = store.get("c1").unwrap();
        assert!(entry.dynamic && entry.due);
        assert_eq!(entry.payload, "watch the   CI run");
        assert_eq!(
            run("/cron", &mut store, true, now),
            ["c1  self-paced  due at next idle  0/24  watch the CI run"]
        );
        // A single word is a prompt too.
        run("/cron summarize", &mut store, true, now);
        assert!(store.get("c2").unwrap().dynamic);
        // `/cron` alone still lists; the verbs keep their meaning.
        assert_eq!(run("/cron list", &mut store, true, now).len(), 2);
        for verb_line in [
            "/cron list all",
            "/cron stop",
            "/cron run",
            "/cron run the tests",
        ] {
            assert!(
                run(verb_line, &mut store, true, now)[0].starts_with("usage: /cron"),
                "{verb_line}"
            );
        }
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn self_paced_entries_refuse_slash_shell_and_interval_second_words() {
        let now = Instant::now();
        let mut store = CronStore::default();
        for (line, expected) in [
            (
                "/cron /cost",
                "cron: self-paced cron payloads are plain prompts",
            ),
            (
                "/cron /goal status",
                "cron: self-paced cron payloads are plain prompts",
            ),
            (
                "/cron !ls",
                "cron: cron payloads are plain prompts; shell (!)",
            ),
            (
                "/cron every 10m poll",
                "cron: for a fixed interval put it first",
            ),
        ] {
            let lines = run(line, &mut store, true, now);
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].starts_with(expected), "{line}: {lines:?}");
        }
        assert!(store.is_empty());
    }

    #[test]
    fn interval_entries_accept_only_allowlisted_slash_payloads() {
        let now = Instant::now();
        let mut store = CronStore::default();
        for payload in ["/goal status", "/tasks", "/jobs", "/cost", "  /cost  "] {
            let lines = run(&format!("/cron 5m {payload}"), &mut store, true, now);
            assert!(lines[0].contains(": every 5m"), "{payload}: {lines:?}");
        }
        assert_eq!(store.len(), 5);
        for payload in [
            "/steer go left",
            "/btw why",
            "/queue x",
            "/loop 5m x",
            "/cron 1m x",
            "/goal",
            "/goal status now",
            "/goal clear",
            "/tasks cancel 1",
            "/jobs stop j1",
            "/cost extra",
            "/compact",
            "/model x",
        ] {
            let lines = run(&format!("/cron 5m {payload}"), &mut store, true, now);
            assert_eq!(lines.len(), 1, "{payload}: {lines:?}");
            assert_eq!(
                lines[0],
                "cron: cron payloads are plain prompts or one of /goal status, /tasks, /jobs, /cost; other slash commands cannot be scheduled",
                "{payload}"
            );
        }
        assert_eq!(store.len(), 5);
        assert!(is_allowlisted_slash_payload("/goal   status"));
        assert!(!is_allowlisted_slash_payload("!/cost"));
    }

    #[test]
    fn cron_warns_once_when_spend_is_unbounded() {
        let now = Instant::now();
        let mut store = CronStore::default();
        let first = run("/cron 5m a", &mut store, false, now);
        assert!(first.iter().any(|l| l.contains("unbounded")), "{first:?}");
        let second = run("/cron 5m b", &mut store, false, now);
        assert!(
            !second.iter().any(|l| l.contains("unbounded")),
            "{second:?}"
        );
    }

    #[test]
    fn cron_refuses_the_seventeenth_schedule() {
        let now = Instant::now();
        let mut store = CronStore::default();
        for _ in 0..cron::CRON_MAX_ENTRIES {
            run("/cron 5m a", &mut store, true, now);
        }
        assert_eq!(
            run("/cron 5m a", &mut store, true, now),
            ["cron: at most 16 schedules per session; /cron stop one first"]
        );
    }
}
