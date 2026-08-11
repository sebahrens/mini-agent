use crate::ui::slash::add::resolve_path;

#[test]
fn test_resolve_path_absolute() {
    let absolute = std::env::temp_dir().join("foo.txt");
    assert!(absolute.is_absolute());
    let result = resolve_path(std::path::Path::new("ignored"), &absolute.to_string_lossy());
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
    let result = resolve_path(std::path::Path::new("ignored"), &root.to_string_lossy());
    assert_eq!(result, root);
}

#[test]
fn test_resolve_path_relative_is_under_cwd() {
    let workspace = std::env::temp_dir().join("active-workspace");
    let result = resolve_path(&workspace, "bar.txt");
    let expected = workspace.join("bar.txt");
    assert_eq!(result, expected);
}

#[test]
fn test_resolve_path_empty_joins_cwd() {
    let workspace = std::env::temp_dir().join("active-workspace");
    let result = resolve_path(&workspace, "");
    assert_eq!(result, workspace);
}
