use super::monochrome_from;
use std::ffi::OsStr;

#[test]
fn no_color_env_counts_only_when_non_empty() {
    assert!(monochrome_from(true, None));
    assert!(monochrome_from(false, Some(OsStr::new("1"))));
    assert!(!monochrome_from(false, Some(OsStr::new(""))));
    assert!(!monochrome_from(false, None));
}
