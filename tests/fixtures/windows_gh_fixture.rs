//! Test-only native GH image. No GitHub access or general shell launch exists.
//! Fixed Windows phase roles are inert ownership experiments, never production.
#[cfg(windows)]
mod windows_phase_probe;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const URL: &str = "https://example.invalid/pull/32";

fn valid_argv(args: &[String]) -> bool {
    args.len() == 6
        && args[0] == "pr"
        && args[1] == "create"
        && args[2] == "--title"
        && args[4] == "--body"
}

fn role_matches(root: &Path, image: &Path, marker: &[u8]) -> bool {
    marker == b"native32" && image == root.join("fixture bin 雪").join("gh.exe")
}

fn owned_role() -> bool {
    let clock = Instant::now();
    let Ok(root) = std::env::current_dir().and_then(std::fs::canonicalize) else {
        return false;
    };
    let root_ms = clock.elapsed().as_millis();
    let Ok(image) = std::env::current_exe().and_then(std::fs::canonicalize) else {
        return false;
    };
    let image_ms = clock.elapsed().as_millis();
    let marker = root.join("gh-role");
    let Ok(metadata) = std::fs::symlink_metadata(&marker) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() != 8 {
        return false;
    }
    let metadata_ms = clock.elapsed().as_millis();
    let Ok(file) = std::fs::File::open(marker) else {
        return false;
    };
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == 8)
    {
        return false;
    }
    let opened_ms = clock.elapsed().as_millis();
    let mut marker = Vec::with_capacity(9);
    let matched =
        file.take(9).read_to_end(&mut marker).is_ok() && role_matches(&root, &image, &marker);
    // Fixed cumulative timings only, and only after the existing owned role has
    // been proven. No path, argv, environment value or marker content is logged.
    if matched {
        eprintln!(
            "NATIVE32_HELPER role root_ms={root_ms} image_ms={image_ms} metadata_ms={metadata_ms} opened_ms={opened_ms} validated_ms={}",
            clock.elapsed().as_millis()
        );
    }
    matched
}

fn self_test() {
    let root = Path::new("owned-root");
    let image = root.join("fixture bin 雪").join("gh.exe");
    assert!(role_matches(root, &image, b"native32"));
    assert!(!role_matches(root, &image, b""));
    assert!(!role_matches(root, &image, b"different"));
    assert!(!role_matches(root, &root.join("other.exe"), b"native32"));
    let args = [
        "pr",
        "create",
        "--title",
        "雪 \"quoted\"\\",
        "--body",
        "first\nlast $(NEVER_EXECUTE)",
    ]
    .map(String::from);
    assert!(valid_argv(&args));
    let encoded = serde_json::to_vec(&args).unwrap();
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&encoded).unwrap(),
        args
    );
    assert!(!valid_argv(&args[..5]));
    let mut invalid = args;
    invalid[0] = "different".into();
    assert!(!valid_argv(&invalid));
}

fn run() -> Result<i32, String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--self-test"] {
        self_test();
        return Ok(0);
    }
    if args.is_empty() {
        return Ok(0);
    }
    // Cargo runs harness=false targets during ordinary suites. Without the
    // explicit owned workspace role and the copied image identity, do no work.
    // The normal Shell environment intentionally strips arbitrary variables;
    // fixture authority must not depend on widening that production allowlist.
    if !owned_role() {
        return Ok(0);
    }
    #[cfg(windows)]
    if args.first().is_some_and(|arg| arg.starts_with("--phase-")) {
        let scalar = |value: &str| {
            if value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("phase scalar refused".to_string());
            }
            value
                .parse::<u64>()
                .map_err(|_| "phase scalar refused".to_string())
        };
        let result = match args.as_slice() {
            [role, case, run, cleanup] if role == "--phase-probe" => {
                windows_phase_probe::probe(case, scalar(run)?, scalar(cleanup)?)
            }
            [role, mode] if role == "--phase-helper" => windows_phase_probe::helper(mode),
            [role, handle] if role == "--phase-target" => windows_phase_probe::target(
                usize::try_from(scalar(handle)?).map_err(|_| "phase scalar refused")?,
            ),
            _ => Err("phase role refused"),
        };
        if let Err(error) = result {
            // Closed fixture-only categories; no argv, path, handle or payload.
            let name = match args[0].as_str() {
                "--phase-probe" => "phase-probe-error",
                "--phase-helper" => "phase-helper-error",
                "--phase-target" => "phase-target-error",
                _ => "phase-role-error",
            };
            let _ = std::fs::write(name, error.as_bytes());
        }
        return result.map_err(String::from);
    }
    if args == ["--posthook-redact"] {
        std::fs::write("gh-posthook", "HOOK_RAN").map_err(|_| "fixture hook marker failed")?;
        println!("{{\"redactions\":[\"{URL}\",\"exit code: 3\"]}}");
        return Ok(0);
    }
    if !valid_argv(&args) {
        return Err("fixture expected six literal PR arguments".into());
    }
    let record = serde_json::json!({"argv":args,"pid":std::process::id(),"role_env_present":std::env::var_os("MINI_AGENT_TEST_GH_ROLE").is_some()});
    std::fs::write("gh-argv.json", serde_json::to_vec(&record).unwrap())
        .map_err(|_| "fixture argv marker failed")?;
    let mode = std::fs::read_to_string("gh-mode").map_err(|_| "fixture mode missing")?;
    match mode.trim() {
        "success" => println!("{URL}"),
        "nonzero" => {
            println!("{URL}");
            return Ok(3);
        }
        "ambiguous" => println!("{URL}\nhttps://example.invalid/pull/33"),
        "incomplete" => {
            println!("{URL}");
            std::io::stdout()
                .write_all(&vec![b'x'; 65_536])
                .map_err(|_| "fixture stdout failed")?;
        }
        "wait" => {
            std::fs::write("gh-ready.part", serde_json::to_vec(&record).unwrap())
                .and_then(|()| std::fs::rename("gh-ready.part", "gh-ready.json"))
                .map_err(|_| "fixture readiness failed")?;
            let deadline = Instant::now() + Duration::from_secs(20);
            while !Path::new("gh-release").exists() {
                if Instant::now() >= deadline {
                    return Err("fixture release deadline expired".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write("gh-late", "LATE_EFFECT").map_err(|_| "fixture late marker failed")?;
            println!("{URL}");
        }
        _ => return Err("unknown fixture mode".into()),
    }
    Ok(0)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(9);
        }
    }
}
