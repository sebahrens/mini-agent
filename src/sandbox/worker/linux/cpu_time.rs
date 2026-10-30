//! Host-side CPU accounting for the sandboxed JavaScript worker.
//!
//! Everything here runs in the trusted parent, inspecting a child's `/proc` entries from
//! outside its containment boundary. None of it executes inside the seccomp-restricted worker,
//! and none of it is part of the containment boundary itself: a worker that cannot be read
//! here is simply never reused (fail closed), never trusted with elevated access.

use std::fs;
use std::io;

/// Bubblewrap's own `--unshare-pid` setup forks an inner process that becomes PID 1 of the new
/// namespace before it execs the worker, so the PID this crate directly launched is never the
/// worker's own PID and its CPU time is always near zero. Each containment level in that chain
/// has exactly one child; the worker itself can never have a child of its own (`denied_syscalls`
/// in the parent module blocks every `fork`/`vfork`/`clone`/`clone3` variant from its seccomp
/// filter), so the first PID in the chain with no children is the worker. This bounds how many
/// nested levels are ever followed, so a corrupted or unexpectedly branching tree fails closed
/// (a read error) rather than looping or mis-attributing CPU time to the wrong process.
const MAX_CONTAINMENT_DESCENT: usize = 8;

/// Finds the actual worker process below `outer_pid`, the PID this crate directly forked for
/// bubblewrap. See [`MAX_CONTAINMENT_DESCENT`] for why `outer_pid` itself is not the right PID
/// and how the worker is identified once found.
///
/// Relies on `/proc/<pid>/task/<pid>/children`, which requires `CONFIG_CHECKPOINT_RESTORE` and
/// is not perfectly race-free. Both are acceptable here: the worker is idle (not itself forking,
/// which it cannot do anyway) when this is read, and any failure already falls back to "do not
/// reuse" rather than to a wrong answer.
pub(super) fn innermost_descendant_pid(outer_pid: u32) -> io::Result<u32> {
    let mut current = outer_pid;
    for _ in 0..MAX_CONTAINMENT_DESCENT {
        let children = fs::read_to_string(format!("/proc/{current}/task/{current}/children"))?;
        let mut children = children.split_whitespace();
        let Some(only_child) = children.next() else {
            return Ok(current);
        };
        if children.next().is_some() {
            return Err(io::Error::other(
                "worker containment process unexpectedly has more than one child",
            ));
        }
        current = only_child
            .parse()
            .map_err(|_| io::Error::other("worker containment child PID was not a valid number"))?;
    }
    Err(io::Error::other(
        "worker containment process tree exceeded the expected nesting depth",
    ))
}

/// Parses the cumulative `utime`+`stime` CPU time (fields 14 and 15 of `/proc/<pid>/stat`) out
/// of that file's raw contents, in whole seconds rounded up so a fractional second of real usage
/// is never under-counted against a safety margin.
///
/// The second field (`comm`, the executable's basename) is parenthesized and may itself contain
/// spaces or even parentheses, so the split point is the *last* `)` on the line rather than a
/// fixed field index.
pub(super) fn parse_cumulative_cpu_seconds(
    stat_contents: &str,
    ticks_per_second: u64,
) -> Option<u64> {
    if ticks_per_second == 0 {
        return None;
    }
    let close_paren = stat_contents.rfind(')')?;
    let remainder = stat_contents.get(close_paren + 1..)?;
    let mut fields = remainder.split_whitespace();
    // After the closing paren of `comm`: state(0) ppid(1) pgrp(2) session(3) tty_nr(4)
    // tpgid(5) flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12).
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    let ticks = utime.checked_add(stime)?;
    Some(ticks.div_ceil(ticks_per_second))
}

#[allow(unsafe_code)]
fn ticks_per_second() -> io::Result<u64> {
    // SAFETY: `sysconf` is a synchronous, thread-safe query of a static per-process constant; no
    // pointer or borrowed memory crosses the call, and `_SC_CLK_TCK` never changes for the
    // lifetime of a process.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 {
        Ok(ticks as u64)
    } else {
        Err(io::Error::other(
            "sysconf(_SC_CLK_TCK) did not report a positive clock tick rate",
        ))
    }
}

/// Reads the worker's cumulative CPU time in whole seconds, fail-closed on any I/O or parse
/// error. `outer_pid` is the PID this crate directly launched (bubblewrap); see
/// [`innermost_descendant_pid`] for why that is not the PID whose CPU time is read.
pub(super) fn cumulative_cpu_seconds(outer_pid: u32) -> io::Result<u64> {
    let worker_pid = innermost_descendant_pid(outer_pid)?;
    let stat = fs::read_to_string(format!("/proc/{worker_pid}/stat"))?;
    let ticks_per_second = ticks_per_second()?;
    parse_cumulative_cpu_seconds(&stat, ticks_per_second)
        .ok_or_else(|| io::Error::other("worker /proc/<pid>/stat had an unexpected format"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utime_and_stime_from_a_well_formed_stat_line() {
        // pid comm state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
        // utime stime cutime cstime ...
        let line = "4242 (mini-agent) R 4241 4242 4242 0 -1 4194624 100 0 0 0 1200 300 0 0 20 0 \
            1 0 999 0 0 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0 0 0 0 0 0 0";
        // utime=1200, stime=300 ticks; at 100 ticks/sec that is exactly 15.0s.
        assert_eq!(parse_cumulative_cpu_seconds(line, 100), Some(15));
    }

    #[test]
    fn rounds_a_fractional_second_up_rather_than_truncating() {
        let line = "1 (x) R 0 0 0 0 -1 0 0 0 0 0 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 \
            0 0 0 0 0 0 0 0";
        // utime=1, stime=1 => 2 ticks at 100 ticks/sec = 0.02s, which must round up to 1 second
        // rather than truncate to 0: the retirement margin must never under-count real usage.
        assert_eq!(parse_cumulative_cpu_seconds(line, 100), Some(1));
    }

    #[test]
    fn a_comm_field_containing_a_closing_paren_does_not_misalign_the_remaining_fields() {
        let line = "7 (weird)name)) R 1 1 1 0 -1 0 0 0 0 0 500 500 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 \
            0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_cumulative_cpu_seconds(line, 100), Some(10));
    }

    #[test]
    fn a_truncated_line_fails_closed_instead_of_panicking() {
        assert_eq!(
            parse_cumulative_cpu_seconds("4242 (mini-agent) R 1 1", 100),
            None
        );
    }

    #[test]
    fn a_non_numeric_cpu_field_fails_closed_instead_of_panicking() {
        let line = "4242 (mini-agent) R 1 1 1 0 -1 0 0 0 0 0 nope 10 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 \
            0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_cumulative_cpu_seconds(line, 100), None);
    }

    #[test]
    fn zero_ticks_per_second_fails_closed_instead_of_dividing_by_zero() {
        let line = "4242 (mini-agent) R 1 1 1 0 -1 0 0 0 0 0 10 10 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 \
            0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(parse_cumulative_cpu_seconds(line, 0), None);
    }

    #[test]
    fn a_pid_with_no_proc_entry_fails_closed() {
        // PID 0 never has a live /proc entry to read.
        assert!(cumulative_cpu_seconds(0).is_err());
    }
}
