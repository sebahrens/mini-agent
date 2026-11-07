//! Platform-independent pseudo-terminal limits shared by the Unix (`pty`) and
//! Windows ConPTY (`pty_windows`) implementations.

use std::io;

/// Largest input one `job_status` `input` action may write.
pub(crate) const MAX_PTY_INPUT_BYTES: usize = 16 * 1024;
/// How long one `job_status` `input` write waits for the program to read.
pub(crate) const PTY_INPUT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Terminal size used when `LINES`/`COLUMNS` are unset or unusable.
pub(crate) const DEFAULT_PTY_SIZE: (u16, u16) = (40, 120);
const PTY_ROWS_RANGE: std::ops::RangeInclusive<u16> = 5..=500;
const PTY_COLS_RANGE: std::ops::RangeInclusive<u16> = 20..=1000;

/// Rows and columns for a new terminal from the operator's `LINES` and
/// `COLUMNS`. A value that is not a plain decimal number inside the sane range
/// falls back to the default for that axis, so the environment can resize but
/// never break the terminal.
pub(crate) fn pty_size(lines: Option<&str>, columns: Option<&str>) -> (u16, u16) {
    fn parse(value: Option<&str>, range: &std::ops::RangeInclusive<u16>, default: u16) -> u16 {
        value
            .and_then(|text| text.trim().parse::<u16>().ok())
            .filter(|number| range.contains(number))
            .unwrap_or(default)
    }
    (
        parse(lines, &PTY_ROWS_RANGE, DEFAULT_PTY_SIZE.0),
        parse(columns, &PTY_COLS_RANGE, DEFAULT_PTY_SIZE.1),
    )
}

/// Why a bounded terminal write stopped early.
#[derive(Debug)]
pub(crate) enum PtyWriteFailure {
    /// The program did not drain its input before the deadline.
    TimedOut,
    Io(io::Error),
}

/// A bounded terminal write that did not complete, with the bytes accepted.
#[derive(Debug)]
pub(crate) struct PtyWriteError {
    pub(crate) written: usize,
    pub(crate) failure: PtyWriteFailure,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_size_honours_sane_lines_and_columns_and_ignores_the_rest() {
        assert_eq!(pty_size(None, None), DEFAULT_PTY_SIZE);
        assert_eq!(pty_size(Some("24"), Some("80")), (24, 80));
        assert_eq!(pty_size(Some(" 50 "), None), (50, 120));
        assert_eq!(pty_size(None, Some("200")), (40, 200));
        for bad in [
            "",
            "abc",
            "-3",
            "4",
            "501",
            "0",
            "99999999999",
            "1e3",
            "80 cols",
        ] {
            assert_eq!(pty_size(Some(bad), None).0, 40, "lines={bad:?}");
        }
        for bad in ["", "x", "-1", "19", "1001", "65536"] {
            assert_eq!(pty_size(None, Some(bad)).1, 120, "columns={bad:?}");
        }
    }
}
