use std::path::Path;

use crate::paths::{
    PortablePathError, collision_key, contained_join, digest_filename, ensure_contained,
    ensure_no_link_traversal, opaque_name, validate_portable_component,
    validate_portable_relative_path,
};

#[test]
fn portable_filename_policy_accepts_only_cross_platform_components() {
    for valid in [
        ".zerostack",
        "normal-name_1.json",
        "résumé",
        "COM0",
        "LPT10",
    ] {
        assert_eq!(validate_portable_component(valid), Ok(()), "{valid:?}");
    }

    let invalid = [
        "",
        ".",
        "..",
        "a/b",
        r"a\b",
        "/absolute",
        r"\absolute",
        r"C:\absolute",
        r"\\server\share",
        "name:stream",
        "trailing.",
        "trailing ",
        "nul\0byte",
        "control\u{001f}",
        "less<than",
        "greater>than",
        "double\"quote",
        "pipe|name",
        "question?",
        "star*",
    ];
    for component in invalid {
        assert!(
            validate_portable_component(component).is_err(),
            "{component:?} must be rejected"
        );
    }

    for device in ["CON", "prn.txt", "Aux.JSON", "nul"] {
        assert!(
            matches!(
                validate_portable_component(device),
                Err(PortablePathError::ReservedWindowsDevice { .. })
                    | Err(PortablePathError::TrailingDotOrSpace)
            ),
            "{device:?} must be rejected"
        );
    }
    for prefix in ["COM", "LPT"] {
        for number in 1..=9 {
            let device = format!("{prefix}{number}.json");
            assert!(matches!(
                validate_portable_component(&device),
                Err(PortablePathError::ReservedWindowsDevice { .. })
            ));
        }
    }
}

#[test]
fn portable_filename_policy_has_deterministic_length_errors() {
    let long_ascii = "a".repeat(256);
    assert!(matches!(
        validate_portable_component(&long_ascii),
        Err(PortablePathError::ComponentTooLong {
            utf8_bytes: 256,
            utf16_units: 256
        })
    ));

    let long_utf8 = "😀".repeat(64);
    assert!(matches!(
        validate_portable_component(&long_utf8),
        Err(PortablePathError::ComponentTooLong {
            utf8_bytes: 256,
            utf16_units: 128
        })
    ));

    let long_path = format!("a/{}", "b/".repeat(2048));
    assert!(matches!(
        validate_portable_relative_path(Path::new(&long_path)),
        Err(PortablePathError::PathTooLong { .. })
    ));
}

#[test]
fn portable_filename_policy_detects_normalized_and_case_folded_collisions() {
    assert_eq!(collision_key("Résumé").unwrap(), collision_key("RE\u{301}SUME\u{301}").unwrap());
    assert_eq!(collision_key("Straße").unwrap(), collision_key("STRASSE").unwrap());
    assert_eq!(collision_key("Provider").unwrap(), collision_key("provider").unwrap());
    assert_ne!(collision_key("provider-a").unwrap(), collision_key("provider-b").unwrap());
}

#[test]
fn portable_filename_call_sites_use_full_versioned_digests() {
    let first = opaque_name("project", &[b"/workspace/alpha"]);
    let again = opaque_name("project", &[b"/workspace/alpha"]);
    let distinct = opaque_name("project", &[b"/workspace/beta"]);
    assert_eq!(first, again);
    assert_ne!(first, distinct);
    assert_eq!(first.len(), 64);
    assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(!first.contains("alpha"));

    assert_ne!(
        opaque_name("identity", &[b"ab", b"c"]),
        opaque_name("identity", &[b"a", b"bc"])
    );
    let file = digest_filename("archive", &[b"display/name"], "zip").unwrap();
    assert_eq!(file.len(), 68);
    assert!(file.ends_with(".zip"));
    assert!(!file.contains("display"));

    #[cfg(feature = "memory")]
    {
        let project = crate::extras::memory::project_slug(Path::new("/workspace/Project Name"));
        assert_eq!(project.len(), 64);
        assert!(!project.contains("Project"));
    }
}

#[test]
fn path_containment_is_component_aware_and_rejects_traversal() {
    let root = Path::new("/tmp/portable-root");
    assert!(ensure_contained(root, Path::new("/tmp/portable-root/child")).is_ok());
    assert!(matches!(
        ensure_contained(root, Path::new("/tmp/portable-root-sibling")),
        Err(PortablePathError::OutsideRoot)
    ));
    assert!(matches!(
        contained_join(root, Path::new("child/../escape")),
        Err(PortablePathError::ParentTraversal)
    ));
    assert_eq!(
        contained_join(root, Path::new(r"child\escape")).unwrap(),
        root.join("child").join("escape")
    );
}

#[cfg(unix)]
#[test]
fn path_containment_rejects_unix_symlink_escape() {
    use std::os::unix::fs::symlink;

    let base = std::env::temp_dir().join(format!("zs-portable-{}", uuid::Uuid::new_v4()));
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let link = root.join("escape");
    symlink(&outside, &link).unwrap();

    assert!(matches!(
        ensure_no_link_traversal(&root, &link.join("file")),
        Err(PortablePathError::LinkTraversal { .. })
    ));

    std::fs::remove_file(link).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}

#[cfg(windows)]
#[test]
fn path_containment_rejects_windows_junction_escape() {
    let base = std::env::temp_dir().join(format!("zs-portable-{}", uuid::Uuid::new_v4()));
    let root = base.join("root");
    let outside = base.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let junction = root.join("escape");
    let status = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            junction.to_str().unwrap(),
            outside.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "test fixture must create a real junction");

    assert!(matches!(
        ensure_no_link_traversal(&root, &junction.join("file")),
        Err(PortablePathError::LinkTraversal { .. })
    ));

    std::fs::remove_dir(junction).unwrap();
    std::fs::remove_dir_all(base).unwrap();
}
