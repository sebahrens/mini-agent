//! `mini-agent schedule add|list|remove|run`: OS-scheduled headless runs.
//!
//! A schedule entry (workspace, prompt, permission mode, budget cap, calendar
//! or interval) is stored as JSON under `<data_dir>/schedules/` and installed
//! as a per-user timer with the platform scheduler: a launchd agent on macOS,
//! a systemd `--user` timer on Linux, a Task Scheduler task on Windows. The
//! timer invokes `mini-agent schedule run <id>`, which runs one headless turn
//! (`mini-agent -p --session sched-<id> ...`) in the workspace with this same
//! executable and records the outcome in `<id>.last.json` beside a log of the
//! run's output. There is no daemon of our own, so a missed fire is caught up
//! only where the scheduler does it (systemd `Persistent=true`).
//!
//! Runs inherit headless semantics: anything that would ask for approval is
//! denied unless a rule allows it. The run is an ordinary `-p` invocation, so
//! `terminal_notify` and the status-socket signals behave as configured.
//! The Windows and Linux installers are written from the documented command
//! grammars and could not be run on the development host.
pub(crate) mod platform;

use std::path::{Path, PathBuf};
use std::time::Duration;

use platform::Platform;
use serde::{Deserialize, Serialize};

const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MIN_EVERY_MINUTES: u32 = 5;
const MAX_EVERY_MINUTES: u32 = 1439;
const DEFAULT_TIMEOUT_SECS: u64 = 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum When {
    Daily { hour: u8, minute: u8 },
    Weekdays { hour: u8, minute: u8 },
    Every { minutes: u32 },
}

impl When {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Daily { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
            Self::Weekdays { hour, minute } => format!("weekdays at {hour:02}:{minute:02}"),
            Self::Every { minutes } => format!("every {minutes} min"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) workspace: String,
    pub(crate) prompt: String,
    pub(crate) when: When,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) timeout_secs: u64,
    pub(crate) created: String,
}

impl Entry {
    fn session_name(&self) -> String {
        format!("sched-{}", self.id)
    }
}

/// `HH:MM`, 24-hour.
fn parse_time(text: &str) -> Result<(u8, u8), String> {
    let (h, m) = text.split_once(':').ok_or("time must be HH:MM")?;
    let (hour, minute): (u8, u8) = (
        h.parse().map_err(|_| "time must be HH:MM")?,
        m.parse().map_err(|_| "time must be HH:MM")?,
    );
    if hour > 23 || minute > 59 {
        return Err("time must be a valid 24-hour HH:MM".into());
    }
    Ok((hour, minute))
}

/// `30m`, `2h` or a bare number of minutes.
fn parse_every(text: &str) -> Result<u32, String> {
    let (digits, factor) = match text.strip_suffix('h') {
        Some(d) => (d, 60),
        None => (text.strip_suffix('m').unwrap_or(text), 1),
    };
    let minutes = digits
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .ok_or("--every needs a duration such as 30m or 2h")?;
    if !(MIN_EVERY_MINUTES..=MAX_EVERY_MINUTES).contains(&minutes) {
        return Err(format!(
            "--every must be between {MIN_EVERY_MINUTES} minutes and 23h59m"
        ));
    }
    Ok(minutes)
}

/// What a `schedule ...` command line asks for.
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Add(Box<NewEntry>),
    List,
    Remove(String),
    Run(String),
}

#[derive(Debug, PartialEq)]
pub(crate) struct NewEntry {
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) prompt: String,
    pub(crate) when: When,
    pub(crate) permission_mode: Option<String>,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) timeout_secs: u64,
}

pub(crate) const USAGE: &str = "\
Usage: mini-agent schedule <command>

Commands:
  add (--daily HH:MM | --weekdays HH:MM | --every 30m) --prompt TEXT
      [--workspace DIR] [--permission-mode MODE] [--max-budget-usd N] [--timeout 1h]
                    Install a per-user timer that runs the prompt headless
  list              Show installed schedules and their last result
  remove <id>       Uninstall a schedule
  run <id>          Run one scheduled turn now (what the timer invokes)";

#[derive(Debug, PartialEq)]
pub(crate) enum Invocation {
    Run(Command),
    Help,
    Invalid(String),
}

fn parse_timeout(text: &str) -> Result<u64, String> {
    let (digits, factor) = if let Some(d) = text.strip_suffix('h') {
        (d, 3600)
    } else if let Some(d) = text.strip_suffix('m') {
        (d, 60)
    } else {
        (text.strip_suffix('s').unwrap_or(text), 1)
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .filter(|secs| (1..=24 * 3600).contains(secs))
        .ok_or_else(|| "--timeout must be between 1s and 24h".to_owned())
}

/// Claims `schedule add|list|remove|run|--help` only, so `mini-agent schedule
/// the release` stays an ordinary prompt.
pub(crate) fn parse_invocation(args: &[std::ffi::OsString]) -> Option<Invocation> {
    let words: Vec<Option<&str>> = args.iter().map(|a| a.to_str()).collect();
    if words.get(1) != Some(&Some("schedule")) {
        return None;
    }
    let sub = words.get(2).copied().flatten()?;
    if !matches!(sub, "add" | "list" | "remove" | "run" | "-h" | "--help") {
        return None;
    }
    let rest: Vec<&str> = match words[3..].iter().copied().collect::<Option<Vec<_>>>() {
        Some(rest) => rest,
        None => return Some(Invocation::Invalid("arguments must be valid UTF-8".into())),
    };
    Some(match sub {
        "-h" | "--help" => Invocation::Help,
        "list" if rest.is_empty() => Invocation::Run(Command::List),
        "list" => Invocation::Invalid("list takes no arguments".into()),
        "remove" | "run" => match rest.as_slice() {
            [id] if valid_id(id) => Invocation::Run(if sub == "remove" {
                Command::Remove((*id).to_owned())
            } else {
                Command::Run((*id).to_owned())
            }),
            _ => Invocation::Invalid(format!("{sub} needs one schedule id")),
        },
        _ => match parse_add(&rest) {
            Ok(add) => Invocation::Run(Command::Add(Box::new(add))),
            Err(message) => Invocation::Invalid(message),
        },
    })
}

fn valid_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn parse_add(args: &[&str]) -> Result<NewEntry, String> {
    let mut when = None;
    let mut prompt = None;
    let mut workspace = None;
    let mut permission_mode = None;
    let mut max_budget_usd = None;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .copied()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let set_when = |when: &mut Option<When>, new: When| {
            if when.replace(new).is_some() {
                Err("choose one of --daily, --weekdays, --every".to_owned())
            } else {
                Ok(())
            }
        };
        match *flag {
            "--daily" => {
                let (hour, minute) = parse_time(value()?)?;
                set_when(&mut when, When::Daily { hour, minute })?;
            }
            "--weekdays" => {
                let (hour, minute) = parse_time(value()?)?;
                set_when(&mut when, When::Weekdays { hour, minute })?;
            }
            "--every" => set_when(
                &mut when,
                When::Every {
                    minutes: parse_every(value()?)?,
                },
            )?,
            "--prompt" => prompt = Some(value()?.to_owned()),
            "--workspace" => workspace = Some(PathBuf::from(value()?)),
            "--permission-mode" => {
                let mode = value()?;
                if mode.is_empty()
                    || mode.len() > 32
                    || !mode.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                {
                    return Err("--permission-mode must be a mode name such as guarded".into());
                }
                permission_mode = Some(mode.to_owned());
            }
            "--max-budget-usd" => {
                let usd: f64 = value()?
                    .parse()
                    .map_err(|_| "--max-budget-usd needs a number")?;
                if !usd.is_finite() || usd <= 0.0 {
                    return Err("--max-budget-usd must be a positive number".into());
                }
                max_budget_usd = Some(usd);
            }
            "--timeout" => timeout_secs = parse_timeout(value()?)?,
            other => return Err(format!("unknown option '{other}' for schedule add")),
        }
    }
    let prompt = prompt.ok_or("--prompt is required")?;
    if prompt.trim().is_empty() {
        return Err("--prompt must not be empty".into());
    }
    if prompt.len() > MAX_PROMPT_BYTES || prompt.contains('\0') {
        return Err(format!("--prompt must be under {MAX_PROMPT_BYTES} bytes"));
    }
    Ok(NewEntry {
        workspace,
        prompt,
        when: when.ok_or("choose one of --daily, --weekdays, --every")?,
        permission_mode,
        max_budget_usd,
        timeout_secs,
    })
}

/// Runs the external commands the installer needs, and the scheduled turn.
pub(crate) trait Runner {
    /// Run a fixed scheduler command (`launchctl`, `systemctl`, `schtasks`).
    fn tool(&mut self, argv: &[String]) -> Result<(), String>;
}

/// Where schedule state lives.
pub(crate) struct Store {
    pub(crate) dir: PathBuf,
}

impl Store {
    fn entry_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
    pub(crate) fn last_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.last.json"))
    }
    pub(crate) fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.log"))
    }

    fn save(&self, entry: &Entry) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let json = serde_json::to_vec_pretty(entry).map_err(|e| e.to_string())?;
        crate::fs::atomic_write_sync(&self.entry_path(&entry.id), &json)
            .map_err(|e| format!("cannot save the schedule: {e}"))
    }

    pub(crate) fn load(&self, id: &str) -> Result<Entry, String> {
        let text = std::fs::read_to_string(self.entry_path(id))
            .map_err(|_| format!("no schedule with id {id}"))?;
        serde_json::from_str(&text).map_err(|e| format!("schedule {id} is damaged: {e}"))
    }

    pub(crate) fn list(&self) -> Vec<Entry> {
        let mut entries: Vec<Entry> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|item| {
                let name = item.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".json")?;
                valid_id(id).then(|| self.load(id).ok()).flatten()
            })
            .collect();
        entries.sort_by(|a, b| a.created.cmp(&b.created));
        entries
    }

    fn delete(&self, id: &str) {
        for path in [self.entry_path(id), self.last_path(id), self.log_path(id)] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Environment of one `add`/`remove`: executable, home, uid, platform.
pub(crate) struct Host {
    pub(crate) platform: Platform,
    pub(crate) exe: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) uid: u32,
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// Create the entry and install its timer. Rolls back everything on failure.
pub(crate) fn add(
    store: &Store,
    host: &Host,
    runner: &mut dyn Runner,
    new: NewEntry,
    cwd: &Path,
) -> Result<Entry, String> {
    let workspace = match &new.workspace {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => cwd.join(path),
        None => cwd.to_path_buf(),
    };
    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("workspace {} is not usable: {e}", workspace.display()))?;
    if !workspace.is_dir() {
        return Err(format!(
            "workspace {} is not a directory",
            workspace.display()
        ));
    }
    let entry = Entry {
        id: new_id(),
        workspace: workspace.to_string_lossy().into_owned(),
        prompt: new.prompt,
        when: new.when,
        permission_mode: new.permission_mode,
        max_budget_usd: new.max_budget_usd,
        timeout_secs: new.timeout_secs,
        created: crate::session::rfc3339_utc(jiff::Timestamp::now()),
    };
    store.save(&entry)?;
    let files = platform::files(
        host.platform,
        &entry,
        &host.exe,
        &host.home,
        &store.log_path(&entry.id),
    );
    let mut written = Vec::new();
    let result = (|| -> Result<(), String> {
        for file in &files {
            if let Some(parent) = file.path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
            }
            std::fs::write(&file.path, &file.content)
                .map_err(|e| format!("cannot write {}: {e}", file.path.display()))?;
            written.push(file.path.clone());
        }
        let plist = files.first().map(|f| f.path.as_path());
        for argv in platform::install_commands(host.platform, &entry, &host.exe, plist, host.uid) {
            runner.tool(&argv)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in written {
            let _ = std::fs::remove_file(path);
        }
        store.delete(&entry.id);
        return Err(error);
    }
    Ok(entry)
}

/// Uninstall the timer and delete the entry. A scheduler that no longer knows
/// the job is not an error; its files and the entry are removed regardless.
pub(crate) fn remove(
    store: &Store,
    host: &Host,
    runner: &mut dyn Runner,
    id: &str,
) -> Result<Vec<String>, String> {
    let entry = store.load(id)?;
    let mut notes = Vec::new();
    for argv in platform::remove_commands(host.platform, id, host.uid) {
        if let Err(error) = runner.tool(&argv) {
            notes.push(format!("scheduler: {error}"));
        }
    }
    let log = store.log_path(id);
    for file in platform::files(host.platform, &entry, &host.exe, &host.home, &log) {
        let _ = std::fs::remove_file(&file.path);
    }
    for argv in platform::after_remove_commands(host.platform) {
        if let Err(error) = runner.tool(&argv) {
            notes.push(format!("scheduler: {error}"));
        }
    }
    store.delete(id);
    Ok(notes)
}

/// Runs an argv with a working directory, log file and timeout; returns
/// `(exit_code, timed_out)`.
pub(crate) type Launch<'a> =
    dyn FnMut(&[String], &Path, &Path, Duration) -> Result<(Option<i32>, bool), String> + 'a;

/// The record `schedule run` leaves behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub(crate) started: String,
    pub(crate) finished: String,
    /// `None` when the run was killed (timeout or signal) or never started.
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) error: Option<String>,
}

/// The headless argv for one run: `mini-agent -p --session sched-<id> ... -- <prompt>`.
///
/// The first run creates the named session (`--name`); later runs, once one
/// has succeeded, resume it (`--session`), so a schedule keeps its context.
pub(crate) fn turn_argv(exe: &Path, entry: &Entry, resume: bool) -> Vec<String> {
    let mut argv = vec![
        exe.to_string_lossy().into_owned(),
        "-p".into(),
        if resume { "--session" } else { "--name" }.into(),
        entry.session_name(),
    ];
    if let Some(mode) = &entry.permission_mode {
        argv.push("--permission-mode".into());
        argv.push(mode.clone());
    }
    if let Some(usd) = entry.max_budget_usd {
        argv.push("--max-budget-usd".into());
        argv.push(usd.to_string());
    }
    argv.push("--".into());
    argv.push(entry.prompt.clone());
    argv
}

/// Run one scheduled turn in the workspace, with output going to the entry's
/// log, and record the result. `launch` runs the argv with the given working
/// directory, log file and timeout and returns `(exit_code, timed_out)`.
pub(crate) fn run_entry(
    store: &Store,
    exe: &Path,
    id: &str,
    launch: &mut Launch<'_>,
) -> Result<RunRecord, String> {
    let entry = store.load(id)?;
    let started = crate::session::rfc3339_utc(jiff::Timestamp::now());
    let resume = std::fs::read_to_string(store.last_path(id))
        .ok()
        .and_then(|text| serde_json::from_str::<RunRecord>(&text).ok())
        .is_some_and(|record| record.exit_code == Some(0));
    let outcome = launch(
        &turn_argv(exe, &entry, resume),
        Path::new(&entry.workspace),
        &store.log_path(id),
        Duration::from_secs(entry.timeout_secs),
    );
    let (exit_code, timed_out, error) = match outcome {
        Ok((code, timed_out)) => (code, timed_out, None),
        Err(error) => (None, false, Some(error)),
    };
    let record = RunRecord {
        started,
        finished: crate::session::rfc3339_utc(jiff::Timestamp::now()),
        exit_code,
        timed_out,
        error,
    };
    std::fs::create_dir_all(&store.dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
    crate::fs::atomic_write_sync(&store.last_path(id), &json)
        .map_err(|e| format!("cannot record the result: {e}"))?;
    Ok(record)
}

/// The one place this module starts a process. With `log = None` stdio is
/// discarded (scheduler tools); with a log, stdout and stderr append to it and
/// the run is bounded by `timeout`, killing the process group when it expires.
pub(crate) fn launch_process(
    argv: &[String],
    cwd: Option<&Path>,
    log: Option<&Path>,
    timeout: Duration,
) -> Result<(Option<i32>, bool), String> {
    launch_process_with_env(argv, cwd, log, timeout, &[])
}

/// [`launch_process`] with extra environment variables for the child.
pub(crate) fn launch_process_with_env(
    argv: &[String],
    cwd: Option<&Path>,
    log: Option<&Path>,
    timeout: Duration,
    env: &[(&str, &str)],
) -> Result<(Option<i32>, bool), String> {
    use crate::process_creation::StdCommandCreationExt as _;
    use std::process::{Command, Stdio};
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    command.envs(env.iter().copied());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    match log {
        Some(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
                .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
            let second = file.try_clone().map_err(|e| e.to_string())?;
            command.stdout(file).stderr(second);
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn_guarded()
        .map_err(|e| format!("could not start {program}: {:?}", e.kind()))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status.code(), false)),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                #[cfg(unix)]
                if let Ok(pid) = i32::try_from(child.id())
                    && pid > 1
                {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                let _ = child.kill();
                let _ = child.wait();
                return Ok((None, true));
            }
            Err(error) => return Err(format!("could not wait for {program}: {:?}", error.kind())),
        }
    }
}

/// The numeric id of the current user, read as the owner of the home
/// directory (launchd's `gui/<uid>` domain); 0 where ids do not exist.
fn home_owner_uid() -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        dirs::home_dir()
            .and_then(|home| std::fs::metadata(home).ok())
            .map_or(0, |meta| meta.uid())
    }
    #[cfg(not(unix))]
    {
        0
    }
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn tool(&mut self, argv: &[String]) -> Result<(), String> {
        match launch_process(argv, None, None, Duration::from_secs(30))? {
            (Some(0), _) => Ok(()),
            (_, true) => Err(format!("{} timed out", argv[0])),
            (code, _) => Err(format!(
                "{} {} failed ({})",
                argv[0],
                argv.get(1).map(String::as_str).unwrap_or(""),
                code.map_or("signal".to_owned(), |c| format!("exit {c}"))
            )),
        }
    }
}

fn last_summary(store: &Store, id: &str) -> String {
    match std::fs::read_to_string(store.last_path(id))
        .ok()
        .and_then(|t| serde_json::from_str::<RunRecord>(&t).ok())
    {
        Some(record) => format!(
            "last run {} ({})",
            record.finished,
            if record.timed_out {
                "timed out".to_owned()
            } else if let Some(error) = record.error {
                format!("error: {error}")
            } else {
                format!(
                    "exit {}",
                    record.exit_code.map_or("?".to_owned(), |c| c.to_string())
                )
            }
        ),
        None => "never run".to_owned(),
    }
}

/// Entry point: returns the process exit code.
pub(crate) fn run(command: Command) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths = match crate::paths::AppPaths::from_process(Some(cwd.clone())) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("error: cannot resolve mini-agent directories: {error}");
            return 1;
        }
    };
    let store = Store {
        dir: paths.data_dir.join("schedules"),
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("error: cannot find this executable: {error}");
            return 1;
        }
    };
    if let Command::Run(id) = &command {
        return match run_entry(&store, &exe, id, &mut |argv, cwd, log, timeout| {
            launch_process(argv, Some(cwd), Some(log), timeout)
        }) {
            Ok(record) if record.exit_code == Some(0) => 0,
            Ok(record) => {
                eprintln!(
                    "scheduled run {id} {}",
                    if record.timed_out {
                        "timed out".to_owned()
                    } else {
                        format!("exited {:?}", record.exit_code)
                    }
                );
                1
            }
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        };
    }
    let Some(platform) = Platform::current() else {
        eprintln!("error: scheduling is not supported on this platform");
        return 1;
    };
    let host = Host {
        platform,
        exe,
        home: dirs::home_dir().unwrap_or_default(),
        uid: home_owner_uid(),
    };
    match command {
        Command::Add(new) => match add(&store, &host, &mut SystemRunner, *new, &cwd) {
            Ok(entry) => {
                println!(
                    "Scheduled {} ({}) in {}",
                    entry.id,
                    entry.when.describe(),
                    entry.workspace
                );
                println!("Results: {}", store.last_path(&entry.id).display());
                0
            }
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        Command::List => {
            let entries = store.list();
            if entries.is_empty() {
                println!("No schedules.");
            }
            for entry in entries {
                println!(
                    "{}  {}  {}  {}\n    {}",
                    entry.id,
                    entry.when.describe(),
                    entry.workspace,
                    last_summary(&store, &entry.id),
                    entry.prompt.lines().next().unwrap_or("")
                );
            }
            0
        }
        Command::Remove(id) => match remove(&store, &host, &mut SystemRunner, &id) {
            Ok(notes) => {
                println!("Removed {id}.");
                for note in notes {
                    eprintln!("note: {note}");
                }
                0
            }
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        Command::Run(_) => unreachable!("handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<std::ffi::OsString> {
        std::iter::once("mini-agent")
            .chain(line.split_whitespace())
            .map(std::ffi::OsString::from)
            .collect()
    }

    #[test]
    fn invocation_claims_only_schedule_subcommands() {
        assert!(parse_invocation(&args("schedule the release")).is_none());
        assert!(parse_invocation(&args("schedule")).is_none());
        assert!(parse_invocation(&args("fix schedule add")).is_none());
        assert_eq!(
            parse_invocation(&args("schedule list")),
            Some(Invocation::Run(Command::List))
        );
        assert_eq!(
            parse_invocation(&args("schedule --help")),
            Some(Invocation::Help)
        );
        assert_eq!(
            parse_invocation(&args("schedule remove ab12cd34")),
            Some(Invocation::Run(Command::Remove("ab12cd34".into())))
        );
        assert!(matches!(
            parse_invocation(&args("schedule remove ../x")),
            Some(Invocation::Invalid(_))
        ));
    }

    #[test]
    fn add_arguments_validate() {
        let ok = parse_add(&[
            "--weekdays",
            "08:00",
            "--prompt",
            "review",
            "--max-budget-usd",
            "2.5",
            "--timeout",
            "30m",
        ])
        .unwrap();
        assert_eq!(ok.when, When::Weekdays { hour: 8, minute: 0 });
        assert_eq!(ok.timeout_secs, 1800);
        assert_eq!(ok.max_budget_usd, Some(2.5));
        assert_eq!(
            parse_add(&["--every", "2h", "--prompt", "p"]).unwrap().when,
            When::Every { minutes: 120 }
        );
        for bad in [
            vec!["--daily", "25:00", "--prompt", "p"],
            vec!["--daily", "08:00", "--weekdays", "09:00", "--prompt", "p"],
            vec!["--every", "1m", "--prompt", "p"],
            vec!["--every", "30m"],
            vec!["--daily", "08:00", "--prompt", "  "],
            vec![
                "--daily",
                "08:00",
                "--prompt",
                "p",
                "--permission-mode",
                "Bad Mode",
            ],
            vec![
                "--daily",
                "08:00",
                "--prompt",
                "p",
                "--max-budget-usd",
                "-1",
            ],
            vec!["--daily", "08:00", "--prompt", "p", "--bogus"],
        ] {
            assert!(parse_add(&bad).is_err(), "{bad:?}");
        }
    }

    #[derive(Default)]
    struct Recording(Vec<Vec<String>>);
    impl Runner for Recording {
        fn tool(&mut self, argv: &[String]) -> Result<(), String> {
            self.0.push(argv.to_vec());
            Ok(())
        }
    }

    fn fixture() -> (PathBuf, Store, Host) {
        let root =
            std::env::temp_dir().join(format!("mini-agent-schedule-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("ws")).unwrap();
        let host = Host {
            platform: Platform::Launchd,
            exe: PathBuf::from("/opt/mini-agent"),
            home: root.join("home"),
            uid: 501,
        };
        (
            root.clone(),
            Store {
                dir: root.join("data/schedules"),
            },
            host,
        )
    }

    #[test]
    fn add_list_remove_round_trip_with_platform_artifacts() {
        let (root, store, host) = fixture();
        let mut runner = Recording::default();
        let new = parse_add(&[
            "--weekdays",
            "08:00",
            "--prompt",
            "nightly review",
            "--workspace",
            "ws",
        ])
        .unwrap();
        let entry = add(&store, &host, &mut runner, new, &root).unwrap();
        let plist = host.home.join(format!(
            "Library/LaunchAgents/com.mini-agent.schedule.{}.plist",
            entry.id
        ));
        assert!(plist.is_file());
        assert_eq!(runner.0[0][..3], ["launchctl", "bootstrap", "gui/501"]);
        assert_eq!(store.list(), vec![entry.clone()]);
        assert_eq!(
            entry.workspace,
            root.join("ws").canonicalize().unwrap().to_string_lossy()
        );

        let notes = remove(&store, &host, &mut runner, &entry.id).unwrap();
        assert!(notes.is_empty());
        assert!(!plist.exists());
        assert!(store.list().is_empty());
        assert_eq!(runner.0[1][..2], ["launchctl", "bootout"]);
        assert!(remove(&store, &host, &mut runner, &entry.id).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_install_rolls_everything_back() {
        struct Failing;
        impl Runner for Failing {
            fn tool(&mut self, _: &[String]) -> Result<(), String> {
                Err("launchctl bootstrap failed (exit 5)".into())
            }
        }
        let (root, store, host) = fixture();
        let new = parse_add(&["--daily", "07:30", "--prompt", "p", "--workspace", "ws"]).unwrap();
        assert!(add(&store, &host, &mut Failing, new, &root).is_err());
        assert!(store.list().is_empty());
        let agents = host.home.join("Library/LaunchAgents");
        assert!(std::fs::read_dir(&agents).map(|d| d.count()).unwrap_or(0) == 0);
        let missing =
            parse_add(&["--daily", "07:30", "--prompt", "p", "--workspace", "nope"]).unwrap();
        assert!(add(&store, &host, &mut Recording::default(), missing, &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn turn_argv_is_a_headless_named_session_with_the_prompt_last() {
        let (root, store, host) = fixture();
        let new = parse_add(&[
            "--daily",
            "07:30",
            "--prompt",
            "-rf the thing",
            "--workspace",
            "ws",
            "--permission-mode",
            "guarded",
            "--max-budget-usd",
            "3",
        ])
        .unwrap();
        let entry = add(&store, &host, &mut Recording::default(), new, &root).unwrap();
        let argv = turn_argv(Path::new("/x/mini-agent"), &entry, false);
        assert_eq!(
            argv[..4],
            [
                "/x/mini-agent",
                "-p",
                "--name",
                &format!("sched-{}", entry.id)
            ]
        );
        assert_eq!(turn_argv(Path::new("/x"), &entry, true)[2], "--session");
        assert!(
            argv.windows(2)
                .any(|w| w == ["--permission-mode", "guarded"])
        );
        assert!(argv.windows(2).any(|w| w == ["--max-budget-usd", "3"]));
        assert_eq!(argv[argv.len() - 2..], ["--", "-rf the thing"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_entry_records_the_result_of_one_turn() {
        let (root, store, host) = fixture();
        let new = parse_add(&["--daily", "07:30", "--prompt", "p", "--workspace", "ws"]).unwrap();
        let entry = add(&store, &host, &mut Recording::default(), new, &root).unwrap();
        let mut seen = None;
        let record = run_entry(
            &store,
            Path::new("/x/mini-agent"),
            &entry.id,
            &mut |argv, cwd, log, timeout| {
                seen = Some((argv.to_vec(), cwd.to_path_buf(), log.to_path_buf(), timeout));
                Ok((Some(0), false))
            },
        )
        .unwrap();
        assert_eq!(record.exit_code, Some(0));
        let (argv, cwd, _, timeout) = seen.unwrap();
        assert_eq!(cwd, PathBuf::from(&entry.workspace));
        assert_eq!(timeout, Duration::from_secs(3600));
        assert!(argv.contains(&"-p".to_owned()));
        let saved: RunRecord =
            serde_json::from_str(&std::fs::read_to_string(store.last_path(&entry.id)).unwrap())
                .unwrap();
        assert_eq!(saved, record);
        assert!(argv.contains(&"--name".to_owned()));
        // After a successful run the named session is resumed.
        let mut second = Vec::new();
        run_entry(&store, Path::new("/x"), &entry.id, &mut |argv, _, _, _| {
            second = argv.to_vec();
            Ok((Some(0), false))
        })
        .unwrap();
        assert!(second.contains(&"--session".to_owned()));
        assert!(last_summary(&store, &entry.id).contains("exit 0"));
        let failed = run_entry(&store, Path::new("/x"), &entry.id, &mut |_, _, _, _| {
            Err("could not start".into())
        })
        .unwrap();
        assert!(failed.error.is_some() && failed.exit_code.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn launch_process_logs_output_reports_exit_and_enforces_the_timeout() {
        let root = std::env::temp_dir().join(format!("mini-agent-launch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("run.log");
        let ok = launch_process(
            &[
                "sh".into(),
                "-c".into(),
                "echo out; echo err >&2; exit 3".into(),
            ],
            Some(&root),
            Some(&log),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(ok, (Some(3), false));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("out") && text.contains("err"));
        let started = std::time::Instant::now();
        let slow = launch_process(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            None,
            Some(&log),
            Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(slow, (None, true));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            launch_process(
                &["/definitely/not/here".into()],
                None,
                None,
                Duration::from_secs(1)
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
