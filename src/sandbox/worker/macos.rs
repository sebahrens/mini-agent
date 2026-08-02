use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Child, ExitStatus};

use super::{
    WorkerBackend, WorkerContainmentAssurance, WorkerContainmentStatus, WorkerLaunchError,
    WorkerProcess,
};

const BACKEND: WorkerBackend = WorkerBackend::Seatbelt;
const ASSURANCE: WorkerContainmentAssurance = WorkerContainmentAssurance::DeprecatedBestEffort;
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const SW_VERS: &str = "/usr/bin/sw_vers";
const VALIDATED_MACOS_MAJORS: &[u32] = &[26];
const EXEC_TRANSITION_BLOCKER: &str = "sandbox-exec requires an initial exec allowance; a stable exact worker-image allowance remains reusable after launch, and macOS rejects an attempt to tighten an already applied Seatbelt profile, so that image cannot subsequently be denied";

pub(super) fn standard_streams_are_protocol_pipes() -> bool {
    fn is_pipe(fd: RawFd) -> bool {
        std::fs::metadata(format!("/dev/fd/{fd}"))
            .map(|metadata| metadata.file_type().is_fifo())
            .unwrap_or(false)
    }

    is_pipe(std::io::stdin().as_raw_fd())
        && is_pipe(std::io::stdout().as_raw_fd())
        && is_pipe(std::io::stderr().as_raw_fd())
}

pub(super) fn containment_status() -> WorkerContainmentStatus {
    let reason = unavailable_reason();
    WorkerContainmentStatus::Unavailable {
        backend: BACKEND,
        assurance: ASSURANCE,
        reason,
    }
}

pub(super) fn launch() -> Result<WorkerProcess, WorkerLaunchError> {
    Err(WorkerLaunchError::Unavailable {
        backend: BACKEND,
        reason: unavailable_reason(),
    })
}

#[cfg(test)]
pub(super) fn launch_executable_for_benchmark(
    _executable: &std::path::Path,
) -> Result<WorkerProcess, WorkerLaunchError> {
    launch()
}

fn unavailable_reason() -> String {
    if !trusted_system_executable(Path::new(SANDBOX_EXEC)) {
        return missing_sandbox_exec_reason();
    }

    unavailable_reason_from_version_probe(macos_major_version())
}

fn missing_sandbox_exec_reason() -> String {
    format!(
        "the undocumented/deprecated best-effort MAC policy is unavailable because {SANDBOX_EXEC} is missing or untrusted"
    )
}

fn unavailable_reason_from_version_probe(macos_major: Result<u32, String>) -> String {
    match macos_major {
        Ok(major) => unavailable_reason_for_major(major),
        Err(reason) => format!(
            "the undocumented/deprecated best-effort MAC policy is disabled because the macOS major version could not be validated: {reason}"
        ),
    }
}

fn unavailable_reason_for_major(major: u32) -> String {
    if !VALIDATED_MACOS_MAJORS.contains(&major) {
        return format!(
            "the undocumented/deprecated best-effort MAC policy is disabled on unvalidated macOS major version {major}"
        );
    }
    format!(
        "the undocumented/deprecated best-effort MAC policy is disabled: {EXEC_TRANSITION_BLOCKER}"
    )
}

fn trusted_system_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_file()
        && metadata.uid() == 0
        && metadata.mode() & 0o022 == 0
        && metadata.mode() & 0o111 != 0
}

fn macos_major_version() -> Result<u32, String> {
    if !trusted_system_executable(Path::new(SW_VERS)) {
        return Err(format!("{SW_VERS} is missing or untrusted"));
    }
    let output = std::process::Command::new(SW_VERS)
        .env_clear()
        .arg("-productVersion")
        .output()
        .map_err(|_| format!("{SW_VERS} could not be executed"))?;
    if !output.status.success() {
        return Err(format!("{SW_VERS} rejected the version query"));
    }
    parse_macos_major(&output.stdout)
}

fn parse_macos_major(version: &[u8]) -> Result<u32, String> {
    let version = std::str::from_utf8(version)
        .map_err(|_| "the version was not UTF-8".to_string())?
        .trim();
    let major = version
        .split('.')
        .next()
        .filter(|component| !component.is_empty())
        .ok_or_else(|| "the version was empty".to_string())?
        .parse::<u32>()
        .map_err(|_| "the major version was not numeric".to_string())?;
    if major == 0 {
        return Err("the major version was zero".to_string());
    }
    Ok(major)
}

#[derive(Debug)]
pub(super) struct WorkerChild {
    child: Child,
}

impl WorkerChild {
    #[cfg(test)]
    pub(super) fn from_unconfined_test_child(child: Child) -> Self {
        Self { child }
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn terminate_tree(&mut self) -> io::Result<()> {
        super::terminate_worker_process_group(self.child.id())
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_major_parser_is_strict_and_allowlist_is_explicit() {
        assert_eq!(parse_macos_major(b"15.7.5\n").unwrap(), 15);
        assert_eq!(parse_macos_major(b"26.5.2\n").unwrap(), 26);
        for invalid in [b"".as_slice(), b"0.1", b"future", b".15"] {
            assert!(parse_macos_major(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(VALIDATED_MACOS_MAJORS, &[26]);
    }

    #[test]
    fn unavailable_reasons_distinguish_missing_unknown_and_supported_probes() {
        let missing = missing_sandbox_exec_reason();
        assert!(missing.contains("sandbox-exec"));
        assert!(missing.contains("missing or untrusted"));
        assert!(!missing.contains("stable exact worker-image"));

        let failed_version =
            unavailable_reason_from_version_probe(Err("version probe failed".into()));
        assert!(failed_version.contains("major version could not be validated"));
        assert!(failed_version.contains("version probe failed"));

        let unknown = unavailable_reason_from_version_probe(Ok(15));
        assert!(unknown.contains("unvalidated macOS major version 15"));
        assert!(!unknown.contains("stable exact worker-image"));

        let validated = unavailable_reason_from_version_probe(Ok(26));
        assert!(validated.contains("stable exact worker-image"));
        assert!(validated.contains("remains reusable"));
    }
}
