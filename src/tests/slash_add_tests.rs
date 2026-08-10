use crate::ui::slash::add::resolve_path;
use std::path::PathBuf;

#[test]
fn test_resolve_path_absolute() {
    let absolute = std::env::temp_dir().join("foo.txt");
    assert!(absolute.is_absolute());
    let result = resolve_path(&absolute.to_string_lossy());
    assert_eq!(result, absolute);
}

#[test]
fn test_resolve_path_relative_root() {
    let root = std::env::temp_dir()
        .ancestors()
        .last()
        .expect("an absolute temporary directory has a root")
        .to_path_buf();
    assert!(root.is_absolute());
    let result = resolve_path(&root.to_string_lossy());
    assert_eq!(result, root);
}

#[test]
fn test_resolve_path_relative_is_under_cwd() {
    let result = resolve_path("bar.txt");
    let expected = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("bar.txt");
    assert_eq!(result, expected);
}

#[test]
fn test_resolve_path_empty_joins_cwd() {
    let result = resolve_path("");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    assert_eq!(result, cwd);
}
