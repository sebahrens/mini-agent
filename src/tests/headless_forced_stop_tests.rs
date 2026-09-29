//! mini-agent-p73n1: a headless run whose owned work never settles ends on a
//! second SIGINT, in a real process under the real signal handlers. The test
//! re-executes this test binary as the child so its signals and its forced
//! `exit` stay out of the parallel test runner.

const FORCED_STOP_CHILD_ENV: &str = "MINI_AGENT_TEST_FORCED_STOP_CHILD";

/// The child half of the test below: a real headless turn whose scoped
/// work never finishes, under the process's real signal handlers.
fn forced_stop_child() -> ! {
    use std::io::Write as _;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(crate::print::run_headless_turn(async move {
            std::mem::drop(crate::agent::runner::spawn_blocking_scoped(move || {
                let _ = started_tx.send(());
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }));
            std::future::pending::<crate::agent::runner::HeadlessTurn>().await
        }));
        started_rx.await.unwrap();
        println!("FORCED_STOP_CHILD_READY");
        let _ = std::io::stdout().flush();
        let turn = run.await.unwrap();
        println!(
            "FORCED_STOP_CHILD_RESULT forced={} failure={}",
            crate::print::headless_force_stopped(),
            turn.failure.map(|f| format!("{f:#}")).unwrap_or_default()
        );
    });
    if crate::print::headless_force_stopped() {
        // `runtime` still owns the stalled blocking thread; dropping it
        // would hang, which is exactly what the exit path must avoid.
        crate::print::exit_force_stopped();
    }
    std::process::exit(3);
}

// mini-agent-p73n1: two real SIGINTs end a headless run whose owned work
// never settles, with status 130, well inside the grace deadline.
#[test]
fn two_sigints_exit_a_headless_run_with_stalled_cleanup() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if std::env::var_os(FORCED_STOP_CHILD_ENV).is_some() {
        forced_stop_child();
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::headless_forced_stop_tests::two_sigints_exit_a_headless_run_with_stalled_cleanup",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FORCED_STOP_CHILD_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines_tx, lines_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let pid = Pid::from_raw(child.id() as i32);
    let ready_deadline = Instant::now() + Duration::from_secs(60);
    let ready = loop {
        match lines_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) if line.contains("FORCED_STOP_CHILD_READY") => break true,
            Ok(_) => {}
            Err(_) if Instant::now() >= ready_deadline => break false,
            Err(_) => {}
        }
    };
    let outcome = (|| {
        if !ready {
            return Err("the child never started its turn".to_owned());
        }
        kill(pid, Signal::SIGINT).map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(300));
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("the first SIGINT alone ended the run: {status}"));
        }
        kill(pid, Signal::SIGINT).map_err(|e| e.to_string())?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok((status, started.elapsed()));
            }
            if started.elapsed() > Duration::from_secs(8) {
                return Err("the second SIGINT did not end the run".to_owned());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let (status, elapsed) = outcome.unwrap();
    assert_eq!(
        status.code(),
        Some(crate::print::HEADLESS_FORCED_STOP_EXIT_CODE)
    );
    assert!(
        elapsed < crate::print::HEADLESS_FORCED_STOP_GRACE,
        "{elapsed:?}"
    );
    let result = lines_rx
        .iter()
        .find(|line| line.contains("FORCED_STOP_CHILD_RESULT"))
        .expect("the child reports its turn before exiting");
    assert!(result.contains("forced=true"), "{result}");
    assert!(result.contains("interrupted again"), "{result}");
}
