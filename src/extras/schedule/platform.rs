//! Per-platform scheduler artifacts for `mini-agent schedule`: the launchd
//! agent, the systemd user units and the Task Scheduler argv, generated as
//! plain data so they can be checked on any host. Nothing here runs a process.
use std::path::{Path, PathBuf};

use super::{Entry, When};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    Launchd,
    Systemd,
    TaskScheduler,
}

impl Platform {
    pub(crate) fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Launchd)
        } else if cfg!(target_os = "linux") {
            Some(Self::Systemd)
        } else if cfg!(windows) {
            Some(Self::TaskScheduler)
        } else {
            None
        }
    }
}

/// A file the scheduler needs, with its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArtifactFile {
    pub(crate) path: PathBuf,
    pub(crate) content: String,
}

pub(crate) fn launchd_label(id: &str) -> String {
    format!("com.mini-agent.schedule.{id}")
}

pub(crate) fn systemd_unit(id: &str) -> String {
    format!("mini-agent-schedule-{id}")
}

pub(crate) fn task_name(id: &str) -> String {
    format!("mini-agent-schedule-{id}")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn calendar_dict(hour: u8, minute: u8, weekday: Option<u8>) -> String {
    let mut out = String::from("    <dict>\n");
    if let Some(day) = weekday {
        out.push_str(&format!("      <key>Weekday</key><integer>{day}</integer>\n"));
    }
    out.push_str(&format!(
        "      <key>Hour</key><integer>{hour}</integer>\n      <key>Minute</key><integer>{minute}</integer>\n    </dict>\n"
    ));
    out
}

pub(crate) fn launchd_plist(entry: &Entry, exe: &Path, log: &Path) -> String {
    let schedule = match entry.when {
        When::Daily { hour, minute } => format!(
            "  <key>StartCalendarInterval</key>\n  <dict>\n    <key>Hour</key><integer>{hour}</integer>\n    <key>Minute</key><integer>{minute}</integer>\n  </dict>\n"
        ),
        When::Weekdays { hour, minute } => {
            let mut out = String::from("  <key>StartCalendarInterval</key>\n  <array>\n");
            for day in 1..=5 {
                out.push_str(&calendar_dict(hour, minute, Some(day)));
            }
            out.push_str("  </array>\n");
            out
        }
        When::Every { minutes } => format!(
            "  <key>StartInterval</key>\n  <integer>{}</integer>\n",
            u64::from(minutes) * 60
        ),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key>\n  <string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n    <string>{exe}</string>\n    <string>schedule</string>\n    <string>run</string>\n    <string>{id}</string>\n  </array>\n\
  <key>WorkingDirectory</key>\n  <string>{cwd}</string>\n\
{schedule}\
  <key>StandardOutPath</key>\n  <string>{log}</string>\n\
  <key>StandardErrorPath</key>\n  <string>{log}</string>\n\
</dict>\n</plist>\n",
        label = xml_escape(&launchd_label(&entry.id)),
        exe = xml_escape(&exe.to_string_lossy()),
        id = xml_escape(&entry.id),
        cwd = xml_escape(&entry.workspace),
        log = xml_escape(&log.to_string_lossy()),
    )
}

/// One argument in a systemd `ExecStart=` line: double-quoted, with `\`, `"`,
/// `%` and `$` escaped so nothing is expanded.
fn systemd_quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `(service, timer)` unit file contents.
pub(crate) fn systemd_units(entry: &Entry, exe: &Path) -> (String, String) {
    let service = format!(
        "[Unit]\nDescription=mini-agent scheduled run {id}\n\n[Service]\nType=oneshot\nWorkingDirectory={cwd}\nExecStart={exe} schedule run {id}\n",
        id = entry.id,
        cwd = systemd_quote(&entry.workspace),
        exe = systemd_quote(&exe.to_string_lossy()),
    );
    let trigger = match entry.when {
        When::Daily { hour, minute } => {
            format!("OnCalendar=*-*-* {hour:02}:{minute:02}:00\nPersistent=true\n")
        }
        When::Weekdays { hour, minute } => {
            format!("OnCalendar=Mon..Fri *-*-* {hour:02}:{minute:02}:00\nPersistent=true\n")
        }
        When::Every { minutes } => format!("OnBootSec=2min\nOnUnitActiveSec={minutes}min\n"),
    };
    let timer = format!(
        "[Unit]\nDescription=mini-agent scheduled run {id}\n\n[Timer]\n{trigger}Unit={unit}.service\n\n[Install]\nWantedBy=timers.target\n",
        id = entry.id,
        unit = systemd_unit(&entry.id),
    );
    (service, timer)
}

/// Files to write for `platform` (none for Task Scheduler).
pub(crate) fn files(
    platform: Platform,
    entry: &Entry,
    exe: &Path,
    home: &Path,
    log: &Path,
) -> Vec<ArtifactFile> {
    match platform {
        Platform::Launchd => vec![ArtifactFile {
            path: home
                .join("Library/LaunchAgents")
                .join(format!("{}.plist", launchd_label(&entry.id))),
            content: launchd_plist(entry, exe, log),
        }],
        Platform::Systemd => {
            let (service, timer) = systemd_units(entry, exe);
            let dir = home.join(".config/systemd/user");
            vec![
                ArtifactFile {
                    path: dir.join(format!("{}.service", systemd_unit(&entry.id))),
                    content: service,
                },
                ArtifactFile {
                    path: dir.join(format!("{}.timer", systemd_unit(&entry.id))),
                    content: timer,
                },
            ]
        }
        Platform::TaskScheduler => Vec::new(),
    }
}

/// Commands that activate the artifacts. `uid` is the numeric user id (launchd).
pub(crate) fn install_commands(
    platform: Platform,
    entry: &Entry,
    exe: &Path,
    plist: Option<&Path>,
    uid: u32,
) -> Vec<Vec<String>> {
    match platform {
        Platform::Launchd => vec![vec![
            "launchctl".into(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            plist.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        ]],
        Platform::Systemd => vec![
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            vec![
                "systemctl".into(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                format!("{}.timer", systemd_unit(&entry.id)),
            ],
        ],
        Platform::TaskScheduler => vec![schtasks_create(entry, exe)],
    }
}

pub(crate) fn remove_commands(platform: Platform, id: &str, uid: u32) -> Vec<Vec<String>> {
    match platform {
        Platform::Launchd => vec![vec![
            "launchctl".into(),
            "bootout".into(),
            format!("gui/{uid}/{}", launchd_label(id)),
        ]],
        Platform::Systemd => vec![vec![
            "systemctl".into(),
            "--user".into(),
            "disable".into(),
            "--now".into(),
            format!("{}.timer", systemd_unit(id)),
        ]],
        Platform::TaskScheduler => vec![vec![
            "schtasks".into(),
            "/Delete".into(),
            "/TN".into(),
            task_name(id),
            "/F".into(),
        ]],
    }
}

/// Commands to run after the unit files are deleted (systemd reload).
pub(crate) fn after_remove_commands(platform: Platform) -> Vec<Vec<String>> {
    match platform {
        Platform::Systemd => vec![vec!["systemctl".into(), "--user".into(), "daemon-reload".into()]],
        _ => Vec::new(),
    }
}

/// `schtasks /Create ...` for the entry. The task action is the quoted
/// executable plus `schedule run <id>`.
pub(crate) fn schtasks_create(entry: &Entry, exe: &Path) -> Vec<String> {
    let action = format!("\"{}\" schedule run {}", exe.to_string_lossy(), entry.id);
    let mut argv: Vec<String> = ["schtasks", "/Create", "/TN"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    argv.push(task_name(&entry.id));
    argv.push("/TR".into());
    argv.push(action);
    match entry.when {
        When::Daily { hour, minute } => {
            argv.extend(["/SC".into(), "DAILY".into(), "/ST".into(), format!("{hour:02}:{minute:02}")]);
        }
        When::Weekdays { hour, minute } => argv.extend([
            "/SC".into(),
            "WEEKLY".into(),
            "/D".into(),
            "MON,TUE,WED,THU,FRI".into(),
            "/ST".into(),
            format!("{hour:02}:{minute:02}"),
        ]),
        When::Every { minutes } => {
            argv.extend(["/SC".into(), "MINUTE".into(), "/MO".into(), minutes.to_string()]);
        }
    }
    argv.push("/F".into());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(when: When) -> Entry {
        Entry {
            id: "ab12cd34".into(),
            workspace: "/work/my repo".into(),
            prompt: "run the nightly review".into(),
            when,
            permission_mode: None,
            max_budget_usd: None,
            timeout_secs: 3600,
            created: "2026-11-21T00:00:00Z".into(),
        }
    }

    #[test]
    fn launchd_plist_has_label_arguments_and_calendar() {
        let exe = Path::new("/opt/bin/mini-agent");
        let weekdays = launchd_plist(&entry(When::Weekdays { hour: 8, minute: 5 }), exe, Path::new("/l.log"));
        assert!(weekdays.contains("<string>com.mini-agent.schedule.ab12cd34</string>"));
        assert!(weekdays.contains("<string>schedule</string>") && weekdays.contains("<string>run</string>"));
        assert_eq!(weekdays.matches("<key>Weekday</key>").count(), 5);
        assert!(weekdays.contains("<key>Hour</key><integer>8</integer>"));
        assert!(weekdays.contains("/work/my repo"));
        let every = launchd_plist(&entry(When::Every { minutes: 30 }), exe, Path::new("/l.log"));
        assert!(every.contains("<key>StartInterval</key>\n  <integer>1800</integer>"));
        let daily = launchd_plist(&entry(When::Daily { hour: 23, minute: 0 }), exe, Path::new("/l.log"));
        assert!(daily.contains("<key>StartCalendarInterval</key>\n  <dict>"), "{daily}");
        assert!(daily.matches("<dict>").count() == 2 && daily.matches("</dict>").count() == 2, "{daily}");
        // Markup characters in paths cannot break the document.
        let mut hostile = entry(When::Daily { hour: 1, minute: 2 });
        hostile.workspace = "/a</string><x>&".into();
        assert!(!launchd_plist(&hostile, exe, Path::new("/l")).contains("<x>"));
    }

    #[test]
    fn systemd_units_quote_and_schedule() {
        let exe = Path::new("/opt/my bin/mini-agent");
        let (service, timer) = systemd_units(&entry(When::Weekdays { hour: 8, minute: 0 }), exe);
        assert!(service.contains("ExecStart=\"/opt/my bin/mini-agent\" schedule run ab12cd34"));
        assert!(service.contains("WorkingDirectory=\"/work/my repo\""));
        assert!(timer.contains("OnCalendar=Mon..Fri *-*-* 08:00:00"));
        assert!(timer.contains("Persistent=true"));
        assert!(timer.contains("Unit=mini-agent-schedule-ab12cd34.service"));
        let (_, every) = systemd_units(&entry(When::Every { minutes: 90 }), exe);
        assert!(every.contains("OnUnitActiveSec=90min"));
        assert_eq!(systemd_quote("a%b$c\"d"), "\"a%%b$$c\\\"d\"");
    }

    #[test]
    fn schtasks_argv_covers_each_schedule_kind() {
        let exe = Path::new("C:\\Program Files\\mini-agent\\mini-agent.exe");
        let weekly = schtasks_create(&entry(When::Weekdays { hour: 8, minute: 30 }), exe);
        assert_eq!(&weekly[..4], ["schtasks", "/Create", "/TN", "mini-agent-schedule-ab12cd34"]);
        assert_eq!(
            weekly[5],
            "\"C:\\Program Files\\mini-agent\\mini-agent.exe\" schedule run ab12cd34"
        );
        assert!(weekly.windows(2).any(|w| w == ["/D", "MON,TUE,WED,THU,FRI"]));
        assert!(weekly.windows(2).any(|w| w == ["/ST", "08:30"]));
        assert_eq!(weekly.last().map(String::as_str), Some("/F"));
        let every = schtasks_create(&entry(When::Every { minutes: 15 }), exe);
        assert!(every.windows(2).any(|w| w == ["/SC", "MINUTE"]));
        assert!(every.windows(2).any(|w| w == ["/MO", "15"]));
        let daily = schtasks_create(&entry(When::Daily { hour: 7, minute: 0 }), exe);
        assert!(daily.windows(2).any(|w| w == ["/SC", "DAILY"]));
    }

    #[test]
    fn install_and_remove_commands_are_fixed_grammars() {
        let e = entry(When::Daily { hour: 1, minute: 1 });
        let exe = Path::new("/x/mini-agent");
        let plist = Path::new("/home/u/Library/LaunchAgents/com.mini-agent.schedule.ab12cd34.plist");
        assert_eq!(
            install_commands(Platform::Launchd, &e, exe, Some(plist), 501),
            vec![vec!["launchctl", "bootstrap", "gui/501", plist.to_str().unwrap()]]
        );
        assert_eq!(
            remove_commands(Platform::Launchd, "ab12cd34", 501),
            vec![vec!["launchctl", "bootout", "gui/501/com.mini-agent.schedule.ab12cd34"]]
        );
        let systemd = install_commands(Platform::Systemd, &e, exe, None, 0);
        assert_eq!(systemd[1].last().map(String::as_str), Some("mini-agent-schedule-ab12cd34.timer"));
        assert_eq!(
            remove_commands(Platform::TaskScheduler, "ab12cd34", 0)[0],
            ["schtasks", "/Delete", "/TN", "mini-agent-schedule-ab12cd34", "/F"]
        );
        let files = files(Platform::Systemd, &e, exe, Path::new("/home/u"), Path::new("/l"));
        assert_eq!(files.len(), 2);
        assert!(files[1].path.ends_with("mini-agent-schedule-ab12cd34.timer"));
    }
}
