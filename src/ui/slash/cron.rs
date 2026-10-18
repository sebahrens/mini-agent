//! `/cron`: arm, list, stop and run session-local recurring prompts.
//!
//! This handler only edits the [`CronStore`]; it never starts a turn. Entries
//! fire when the TUI is idle, so every verb is safe while a run is active.
use super::{write_error, write_ok, write_result};
use crate::ui::cron::{self, ArmError, CronStore};
use crate::ui::renderer::Renderer;
use tokio::time::Instant;

const USAGE: &str =
    "usage: /cron <interval> <prompt>  |  /cron list  |  /cron stop <id>|all  |  /cron run <id>";

/// Why a payload cannot be scheduled, or `None` for a plain prompt.
pub(crate) fn payload_refusal(payload: &str) -> Option<&'static str> {
    match payload.trim_start().chars().next() {
        Some('/') => Some("cron payloads are plain prompts; slash commands cannot be scheduled"),
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
        _ => write_error(renderer, USAGE),
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
        write_ok(
            renderer,
            format!(
                "{}  every {}  {next}  {}/{max_fires}  {payload}",
                entry.id,
                cron::format_duration(entry.interval),
                entry.fires,
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
        assert!(run("/cron every 5m x", &mut store, true, now)[0].starts_with("usage: /cron"));
        assert!(run("/cron stop c1 now", &mut store, true, now)[0].starts_with("usage: /cron"));
        assert_eq!(store.len(), 1);
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
