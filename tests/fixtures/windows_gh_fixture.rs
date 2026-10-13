//! Test-only native GH image. No GitHub access, shell or child launch exists here.
use std::io::Write;
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

fn self_test() {
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
    // explicit owned fixture role, do no work and never touch application state.
    if std::env::var_os("MINI_AGENT_TEST_GH_ROLE").as_deref()
        != Some(std::ffi::OsStr::new("native32"))
    {
        return Ok(0);
    }
    if args == ["--posthook-redact"] {
        std::fs::write("gh-posthook", "HOOK_RAN").map_err(|_| "fixture hook marker failed")?;
        println!("{{\"redactions\":[\"{URL}\",\"exit code: 3\"]}}");
        return Ok(0);
    }
    if !valid_argv(&args) {
        return Err("fixture expected six literal PR arguments".into());
    }
    let record = serde_json::json!({"argv":args,"pid":std::process::id()});
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
