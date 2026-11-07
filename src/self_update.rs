//! `mini-agent update [--check] [--release X.Y.Z]`: replace this binary with a
//! release archive whose SHA-256 matches the release's `SHA256SUMS`.
//!
//! The verification rules are `install.sh`'s: exactly one manifest line for the
//! archive, in canonical `<64 lowercase hex>  <file>` form, matching the
//! downloaded bytes, before anything is extracted. Only the `mini-agent`
//! executable is taken out of the archive (by exact name, never by path), it is
//! staged beside the installed binary and renamed over it atomically, and a
//! binary installed by a package manager, a build tree or the VS Code extension
//! is left alone with the command that updates it. Build provenance is not
//! checked by checksum alone: when an authenticated GitHub CLI is available the
//! archive must also pass `gh attestation verify --repo <slug> --signer-workflow <release.yml>
//! --source-ref refs/tags/v<version>` (the release workflow for that tag) before anything is
//! installed (a failure aborts); without it the manual command is printed and the
//! install needs `--allow-unattested` or an interactive confirmation.
//! `MINI_AGENT_SKIP_ATTESTATION=1` skips the check as in `install.sh`, and an
//! older `--release` than the running version needs `--allow-downgrade` or a
//! confirmation. A successful update also refreshes the notice documents under `share/doc/mini-agent`.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

const BINARY_NAME: &str = "mini-agent";
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
/// Largest notice document taken from a release archive.
const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;
/// The notice documents `install.sh` installs next to the binary.
const NOTICE_DOCUMENTS: [&str; 4] = ["LICENSE", "NOTICE", "SOURCE.md", "THIRD_PARTY_LICENSES"];
const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UpdateArgs {
    pub check: bool,
    pub release: Option<String>,
    /// Install without build-provenance verification when `gh` cannot do it.
    pub allow_unattested: bool,
    /// Allow `--release` to pick a version older than the running one.
    pub allow_downgrade: bool,
}

/// Recognises `mini-agent update` with only `--check`, `--release X.Y.Z`,
/// `--allow-unattested` and `--allow-downgrade`
/// after it. Anything else (including `mini-agent update the docs`) is left to
/// the normal command line, where it stays an ordinary prompt.
pub(crate) fn parse_update_args(args: &[OsString]) -> Option<UpdateArgs> {
    let mut args = args.iter().skip(1);
    if args.next()?.to_str()? != "update" {
        return None;
    }
    let mut parsed = UpdateArgs {
        check: false,
        release: None,
        allow_unattested: false,
        allow_downgrade: false,
    };
    while let Some(arg) = args.next() {
        match arg.to_str()? {
            "--check" if !parsed.check => parsed.check = true,
            "--allow-unattested" if !parsed.allow_unattested => parsed.allow_unattested = true,
            "--allow-downgrade" if !parsed.allow_downgrade => parsed.allow_downgrade = true,
            "--release" if parsed.release.is_none() => {
                parsed.release = Some(args.next()?.to_str()?.to_owned());
            }
            _ => return None,
        }
    }
    Some(parsed)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InstallChannel {
    /// Installed by `install.sh` or copied by hand: this command may replace it.
    SelfManaged,
    /// Somebody else owns the file; carries the instruction to show.
    Managed(String),
}

/// Classifies the running executable's (canonical) path. Pure, so the table is
/// testable on any host.
pub(crate) fn classify_install(exe: &str, home: Option<&str>, version: &str) -> InstallChannel {
    let path = exe.replace('\\', "/");
    let managed = |message: &str| InstallChannel::Managed(message.to_owned());
    if cfg!(windows) || path.contains(".msi") || path.contains("/WindowsApps/") {
        return managed(
            "Windows installs are updated with the MSI from the latest GitHub release \
             (see docs/agent/WINDOWS_INSTALL.md); a running .exe cannot be replaced in place.",
        );
    }
    if path.contains("/.vscode") && path.contains("/extensions/")
        || path.contains("/.cursor/extensions/")
    {
        return managed(
            "This binary belongs to the VS Code extension. Update the extension to update it, \
             or point `mini-agent.executablePath` at a separately installed mini-agent.",
        );
    }
    if path.starts_with("/nix/store/") {
        return managed(
            "This binary lives in the Nix store; update it through Nix (for example `nix profile upgrade`).",
        );
    }
    if path.contains("/Cellar/") || path.contains("/homebrew/") || path.contains("/linuxbrew/") {
        return managed(
            "Installed with Homebrew. The in-repo formula is named `zerostack` and no \
             `mini-agent` tap is published, so `brew upgrade mini-agent` does not exist: \
             upgrade the formula you installed from (`brew list --formula` shows it), or \
             install the release archive from the GitHub releases page.",
        );
    }
    if path.contains("/node_modules/") {
        return managed(
            "Installed by npm: update it with `npm update -g` for the package that installed it.",
        );
    }
    if let Some(home) = home
        && path.starts_with(&format!("{}/.cargo/bin/", home.trim_end_matches('/')))
    {
        return InstallChannel::Managed(format!(
            "Installed with cargo: run `cargo install --locked --git {} --tag v{version} --force` \
             with the release tag you want.",
            crate::product::REPOSITORY_URL
        ));
    }
    if path.contains("/target/debug/") || path.contains("/target/release/") {
        return managed(
            "This is a local build output; rebuild it instead (`cargo build --release`).",
        );
    }
    for system in ["/usr/bin/", "/bin/", "/usr/sbin/", "/sbin/", "/opt/"] {
        if path.starts_with(system) {
            return managed(
                "Installed in a system location, so a package manager probably owns it: \
                 update it with that package manager (apt, dnf, pacman, ...).",
            );
        }
    }
    InstallChannel::SelfManaged
}

/// Release asset target for this host (`install.sh` names), or `None`.
fn release_archive_name() -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-musl",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    Some(format!("{BINARY_NAME}-{arch}-{os}.tar.gz"))
}

/// The expected digest from a `SHA256SUMS` manifest, with `install.sh`'s
/// fail-closed rules: exactly one entry for the file, in canonical form.
pub(crate) fn manifest_digest(manifest: &str, file: &str) -> Result<String, String> {
    let entries: Vec<&str> = manifest
        .lines()
        .filter(|line| line.split_whitespace().nth(1) == Some(file))
        .collect();
    let line = match entries.as_slice() {
        [] => return Err(format!("SHA256SUMS has no entry for {file}")),
        [line] => *line,
        _ => return Err(format!("SHA256SUMS has duplicate entries for {file}")),
    };
    let digest = line.split_whitespace().next().unwrap_or_default();
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("SHA256SUMS contains a malformed hash for {file}"));
    }
    if line != format!("{digest}  {file}") {
        return Err(format!("SHA256SUMS contains a malformed entry for {file}"));
    }
    Ok(digest.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::hex::encode_lower(Sha256::digest(bytes))
}

/// Verifies `archive` against `manifest` and returns the `mini-agent`
/// executable inside it. Nothing is extracted before the digest matches.
pub(crate) fn verified_binary(
    manifest: &str,
    file: &str,
    archive: &[u8],
) -> Result<Vec<u8>, String> {
    let expected = manifest_digest(manifest, file)?;
    let actual = sha256_hex(archive);
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {file}\n  expected: {expected}\n  actual:   {actual}"
        ));
    }
    extract_binary(archive, BINARY_NAME)
}

/// The one regular file called `name` (as `name` or `./name`) in a gzip tar,
/// read through bounded readers. Entries are matched by exact name, never
/// written to the paths they claim, so a hostile archive cannot traverse.
fn extract_binary(archive: &[u8], name: &str) -> Result<Vec<u8>, String> {
    extract_member(archive, name, MAX_BINARY_BYTES)?
        .ok_or_else(|| format!("archive does not contain the {name} executable"))
}

/// Like [`extract_binary`] for any one member: `None` when absent, with the
/// size of the member bounded by `max_bytes`.
fn extract_member(archive: &[u8], name: &str, max_bytes: u64) -> Result<Option<Vec<u8>>, String> {
    let decoder = flate2::read::GzDecoder::new(archive);
    // Bound the decompressed stream so a decompression bomb cannot grow memory.
    let mut tar = decoder.take(MAX_ARCHIVE_BYTES.saturating_mul(2));
    let mut found: Option<Vec<u8>> = None;
    let mut long_name: Option<String> = None;
    loop {
        let mut header = [0u8; 512];
        match tar.read_exact(&mut header) {
            Ok(()) => {}
            Err(_) => return Err("archive is truncated or not a gzip tar".into()),
        }
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let field = |range: std::ops::Range<usize>| {
            let raw = &header[range];
            let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let size = u64::from_str_radix(field(124..136).trim(), 8)
            .map_err(|_| "archive has a malformed entry size".to_string())?;
        let kind = header[156];
        let mut entry_name = field(0..100);
        let prefix = field(345..500);
        if !prefix.is_empty() {
            entry_name = format!("{prefix}/{entry_name}");
        }
        if let Some(long) = long_name.take() {
            entry_name = long;
        }
        let padded = size.div_ceil(512) * 512;
        let wanted = matches!(kind, b'0' | 0) && entry_name.trim_start_matches("./") == name;
        if wanted {
            if found.is_some() {
                return Err(format!("archive contains {name} more than once"));
            }
            if size == 0 || size > max_bytes {
                return Err(format!("{name} in the archive has an unreasonable size"));
            }
            let mut data = Vec::new();
            (&mut tar)
                .take(size)
                .read_to_end(&mut data)
                .map_err(|_| "archive is truncated".to_string())?;
            if data.len() as u64 != size {
                return Err("archive is truncated".into());
            }
            skip(&mut tar, padded - size)?;
            found = Some(data);
        } else if kind == b'L' && size <= 4096 {
            let mut data = Vec::new();
            (&mut tar)
                .take(size)
                .read_to_end(&mut data)
                .map_err(|_| "archive is truncated".to_string())?;
            skip(&mut tar, padded - size)?;
            let end = data
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(data.len());
            long_name = Some(String::from_utf8_lossy(&data[..end]).into_owned());
        } else {
            skip(&mut tar, padded)?;
        }
    }
    Ok(found)
}

/// The notice documents present in an archive whose digest was already
/// verified. Older releases lack some of them; those are simply absent.
fn archive_documents(archive: &[u8]) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let mut documents = Vec::new();
    for name in NOTICE_DOCUMENTS {
        if let Some(data) = extract_member(archive, name, MAX_DOCUMENT_BYTES)? {
            documents.push((name, data));
        }
    }
    Ok(documents)
}

/// Where `install.sh` puts the notice documents for an executable at `exe`.
fn notice_directory(exe: &Path) -> Option<PathBuf> {
    let bin_dir = exe.parent()?;
    let base = if bin_dir.file_name().is_some_and(|name| name == "bin") {
        bin_dir.parent()?.to_path_buf()
    } else {
        bin_dir.to_path_buf()
    };
    Some(base.join("share").join("doc").join(BINARY_NAME))
}

/// How refreshing the installed notice documents went.
#[derive(Debug, PartialEq, Eq)]
enum NoticeRefresh {
    Refreshed(usize),
    /// The notice directory does not exist: nothing was installed there.
    Missing(PathBuf),
    Failed(PathBuf, String),
}

/// Replaces the documents in `directory` (which must already exist) one by one
/// through a staged file and a rename, so a failure never leaves a half-written
/// notice. Documents the directory does not hold yet are added.
fn refresh_notice_documents(directory: &Path, documents: &[(&str, Vec<u8>)]) -> NoticeRefresh {
    if !directory.is_dir() {
        return NoticeRefresh::Missing(directory.to_path_buf());
    }
    let mut written = 0;
    for (name, data) in documents {
        let staged = directory.join(format!(".{name}.update.{}", std::process::id()));
        let result = std::fs::write(&staged, data)
            .and_then(|()| std::fs::rename(&staged, directory.join(name)));
        if let Err(error) = result {
            let _ = std::fs::remove_file(&staged);
            return NoticeRefresh::Failed(directory.to_path_buf(), error.to_string());
        }
        written += 1;
    }
    NoticeRefresh::Refreshed(written)
}

fn report_notice_refresh(outcome: &NoticeRefresh) {
    match outcome {
        NoticeRefresh::Refreshed(count) if *count > 0 => {
            println!("Refreshed {count} license and source notice document(s).");
        }
        NoticeRefresh::Refreshed(_) => {}
        NoticeRefresh::Missing(directory) => eprintln!(
            "Warning: {} does not exist, so the license and source notices were not refreshed; \
             rerun install.sh to install the notices for this release.",
            directory.display()
        ),
        NoticeRefresh::Failed(directory, error) => eprintln!(
            "Warning: could not refresh the license and source notices in {}: {error}; \
             rerun install.sh to update them.",
            directory.display()
        ),
    }
}

fn skip(reader: &mut impl Read, bytes: u64) -> Result<(), String> {
    let copied = std::io::copy(&mut reader.take(bytes), &mut std::io::sink())
        .map_err(|_| "archive is truncated".to_string())?;
    if copied == bytes {
        Ok(())
    } else {
        Err("archive is truncated".into())
    }
}

/// Stage `bytes` beside `target` and rename it into place. A rename replaces
/// the directory entry atomically and leaves a running process on the old inode
/// (writing over the file would hit ETXTBSY or corrupt a signed executable).
#[cfg(unix)]
fn install_atomically(target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let directory = target
        .parent()
        .ok_or_else(|| std::io::Error::other("the executable has no parent directory"))?;
    let staged = directory.join(format!(".{BINARY_NAME}.update.{}", std::process::id()));
    let _ = std::fs::remove_file(&staged);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&staged)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staged, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

#[cfg(not(unix))]
fn install_atomically(_target: &Path, _bytes: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::other("in-place update is Unix only"))
}

pub(crate) fn https_only_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
            attempt.error("release downloads must stay on https and follow at most 5 redirects")
        } else {
            attempt.follow()
        }
    })
}

async fn download(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(https_only_redirects())
        .user_agent(concat!("mini-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| error.to_string())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("could not download {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "could not download {url}: HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(format!("{url} is larger than the {limit} byte limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("download of {url} failed: {error}"))?
    {
        if bytes.len() as u64 + chunk.len() as u64 > limit {
            return Err(format!("{url} is larger than the {limit} byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn latest_version() -> Result<String, String> {
    let endpoint = crate::update::github_latest_url(env!("CARGO_PKG_REPOSITORY"))
        .ok_or("the repository URL is not a GitHub URL")?;
    crate::update::fetch_latest_explicit(&endpoint, API_TIMEOUT)
        .await
        .map_err(|error| {
            format!(
                "could not read the latest release from GitHub: {error}. Behind a proxy, set \
                 HTTPS_PROXY (the explicit update honours it; the passive update check stays \
                 direct), or name a version with `mini-agent update --release X.Y.Z`."
            )
        })
}

fn running_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Entry point. Returns the process exit code and prints to stdout/stderr.
pub(crate) async fn run(args: UpdateArgs) -> i32 {
    match run_inner(args).await {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("Error: {message}");
            1
        }
    }
}

async fn run_inner(args: UpdateArgs) -> Result<(), String> {
    let current = running_version();
    let requested = match &args.release {
        Some(release) => crate::update::normalized_version(release)
            .ok_or_else(|| format!("invalid release version: {release}"))?,
        None => latest_version().await?,
    };
    let newer = crate::update::stable_version(&requested) > crate::update::stable_version(current);
    if args.check {
        if newer {
            println!(
                "mini-agent {current} -> {requested} is available. Run `mini-agent update` to install it."
            );
        } else if args.release.is_some() && requested != current {
            println!("mini-agent {requested} is older than the installed {current}.");
        } else {
            println!("mini-agent {current} is up to date.");
        }
        return Ok(());
    }
    if requested == current {
        println!("mini-agent {current} is already installed.");
        return Ok(());
    }
    let downgrade = args.release.is_some()
        && crate::update::stable_version(&requested) < crate::update::stable_version(current);
    if downgrade
        && !args.allow_downgrade
        && !confirm(&format!(
            "mini-agent {requested} is older than the installed {current}. Downgrade?"
        ))
    {
        return Err(format!(
            "{requested} is older than the installed {current}; rerun with --allow-downgrade \
             to install it anyway."
        ));
    }
    if !newer && args.release.is_none() {
        println!(
            "mini-agent {current} is newer than the latest release ({requested}); nothing to do."
        );
        return Ok(());
    }
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("cannot locate the running executable: {error}"))?;
    let home = std::env::var("HOME").ok();
    if let InstallChannel::Managed(instruction) =
        classify_install(&exe.to_string_lossy(), home.as_deref(), &requested)
    {
        return Err(instruction);
    }
    let archive_name = release_archive_name()
        .ok_or("no release archive exists for this platform; see docs/agent/WINDOWS_INSTALL.md")?;
    let base = format!(
        "https://github.com/{}/releases/download/v{requested}",
        crate::product::REPOSITORY_SLUG
    );
    println!("Downloading mini-agent v{requested} ({archive_name})...");
    let manifest = download(&format!("{base}/SHA256SUMS"), MAX_MANIFEST_BYTES).await?;
    let archive = download(&format!("{base}/{archive_name}"), MAX_ARCHIVE_BYTES).await?;
    let manifest = String::from_utf8(manifest).map_err(|_| "SHA256SUMS is not UTF-8")?;
    let binary = verified_binary(&manifest, &archive_name, &archive)?;
    println!("Checksum verified against SHA256SUMS.");
    let attestation = verify_provenance(
        &archive_name,
        &archive,
        &format!("v{requested}"),
        args.allow_unattested,
    )
    .await?;
    install_verified(&exe, &binary, &requested).await?;
    println!(
        "Updated mini-agent {current} -> {requested} at {}",
        exe.display()
    );
    if attestation != Attestation::Verified {
        eprintln!("Note: this install was not provenance-verified.");
    }
    // The archive digest was verified above; its notices now replace the old ones.
    let outcome = match (archive_documents(&archive), notice_directory(&exe)) {
        (Ok(documents), Some(directory)) => refresh_notice_documents(&directory, &documents),
        (Err(error), _) => NoticeRefresh::Failed(PathBuf::from("the release archive"), error),
        (_, None) => NoticeRefresh::Failed(exe.clone(), "no install directory".into()),
    };
    report_notice_refresh(&outcome);
    Ok(())
}

/// What `gh attestation verify` could say about the downloaded archive.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attestation {
    Verified,
    /// gh ran and rejected the archive: never installed.
    Failed(String),
    /// Could not run the check at all (and why).
    Unavailable(String),
    /// `MINI_AGENT_SKIP_ATTESTATION=1`.
    Skipped,
}

/// Limits for the helper and probe subprocesses `update` runs. The deadline is
/// configurable with `MINI_AGENT_UPDATE_HELPER_TIMEOUT_SECS` (default 60).
#[derive(Debug, Clone, Copy)]
pub(crate) struct HelperLimits {
    pub deadline: Duration,
    pub max_output: usize,
}

const DEFAULT_HELPER_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_HELPER_OUTPUT_BYTES: usize = 1024 * 1024;

impl HelperLimits {
    pub(crate) fn from_env() -> Self {
        let deadline = std::env::var("MINI_AGENT_UPDATE_HELPER_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(DEFAULT_HELPER_TIMEOUT, Duration::from_secs);
        Self {
            deadline,
            max_output: MAX_HELPER_OUTPUT_BYTES,
        }
    }
}

/// Runs `command` with a wall-clock deadline and a per-stream output cap. The
/// child leads its own process group (Unix) so a timeout or overflow tears
/// down every descendant; the failure is an error, never a partial success.
pub(crate) fn run_bounded(
    command: &mut std::process::Command,
    limits: HelperLimits,
) -> std::io::Result<std::process::Output> {
    use crate::process_creation::StdCommandCreationExt as _;
    use std::io::{Error, ErrorKind};
    use std::sync::mpsc;
    use std::time::Instant;

    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn_guarded()?;
    let pid = child.id();
    let (sender, receiver) = mpsc::channel::<(bool, Vec<u8>, bool)>();
    for (is_stdout, stream) in [
        (
            true,
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
        (
            false,
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
    ] {
        let Some(stream) = stream else { continue };
        let sender = sender.clone();
        let cap = limits.max_output;
        std::thread::spawn(move || {
            let mut data = Vec::new();
            let mut limited = stream.take(cap as u64 + 1);
            let _ = limited.read_to_end(&mut data);
            let overflow = data.len() > cap;
            data.truncate(cap);
            let _ = sender.send((is_stdout, data, overflow));
        });
    }
    drop(sender);
    let teardown = |child: &mut std::process::Child| {
        #[cfg(unix)]
        crate::sandbox::kill_process_group(pid);
        #[cfg(windows)]
        crate::sandbox::windows::terminate_owned_child(pid);
        let _ = child.kill();
        let _ = child.wait();
    };
    let started = Instant::now();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut finished_streams = 0;
    let mut status = None;
    loop {
        // Drain finished readers; an overflowing stream ends the run.
        while let Ok((is_stdout, data, overflow)) = receiver.try_recv() {
            finished_streams += 1;
            if overflow {
                teardown(&mut child);
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("output exceeded {} bytes", limits.max_output),
                ));
            }
            if is_stdout {
                stdout = data
            } else {
                stderr = data
            }
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_some() && finished_streams >= 2 {
            break;
        }
        if started.elapsed() >= limits.deadline {
            teardown(&mut child);
            return Err(Error::new(
                ErrorKind::TimedOut,
                format!("timed out after {}s", limits.deadline.as_secs_f64()),
            ));
        }
        // A descendant holding a pipe after the leader exits is bounded by
        // the same deadline above.
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(std::process::Output {
        status: status.expect("leader status recorded"),
        stdout,
        stderr,
    })
}

fn run_gh(
    program: &std::ffi::OsStr,
    args: &[&std::ffi::OsStr],
    limits: HelperLimits,
) -> Result<std::process::Output, std::io::Error> {
    run_bounded(std::process::Command::new(program).args(args), limits)
}

/// The workflow that signs release attestations: only artifacts built by this
/// workflow count, not those of a branch or pull-request workflow.
pub(crate) fn release_signer_workflow(repo: &str) -> String {
    format!("{repo}/.github/workflows/release.yml")
}

/// Mirrors `install.sh`'s `verify_attestation`: skip on request, report an
/// unusable `gh` as unavailable, and treat only a rejection by a working `gh`
/// as a failure. `program` is the GitHub CLI executable (injectable for tests).
pub(crate) fn check_attestation(
    program: &std::ffi::OsStr,
    archive: &Path,
    repo: &str,
    tag: &str,
    skip: bool,
    no_gh: bool,
) -> Attestation {
    check_attestation_with(
        program,
        archive,
        repo,
        tag,
        skip,
        no_gh,
        HelperLimits::from_env(),
    )
}

/// [`check_attestation`] with explicit helper limits. A `gh` that stalls or
/// floods output is "unavailable" during the sign-in and capability probes
/// (still needing confirmation) and a rejection during verification itself.
pub(crate) fn check_attestation_with(
    program: &std::ffi::OsStr,
    archive: &Path,
    repo: &str,
    tag: &str,
    skip: bool,
    no_gh: bool,
    limits: HelperLimits,
) -> Attestation {
    use std::ffi::OsStr;
    if skip {
        return Attestation::Skipped;
    }
    if no_gh {
        return Attestation::Unavailable(
            "MINI_AGENT_INSTALL_NO_GH=1 disables the GitHub CLI".into(),
        );
    }
    match run_gh(program, &[OsStr::new("auth"), OsStr::new("status")], limits) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Attestation::Unavailable("the GitHub CLI ('gh') is not installed".into());
        }
        Err(error) => return Attestation::Unavailable(format!("could not run gh: {error}")),
        Ok(output) if !output.status.success() => {
            return Attestation::Unavailable(
                "the GitHub CLI is not signed in (run 'gh auth login')".into(),
            );
        }
        Ok(_) => {}
    }
    // Probe the help text for the exact pin flags used below: a gh that has
    // the subcommand but not `--signer-workflow`/`--source-ref` would fail
    // with "unknown flag", which is not a rejection of the archive.
    let help = run_gh(
        program,
        &[
            OsStr::new("attestation"),
            OsStr::new("verify"),
            OsStr::new("--help"),
        ],
        limits,
    );
    let Some(help) = help.ok().filter(|output| output.status.success()) else {
        return Attestation::Unavailable(
            "this GitHub CLI does not support 'gh attestation verify'; upgrade gh".into(),
        );
    };
    let help_text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr)
    );
    if let Some(flag) = ["--signer-workflow", "--source-ref"]
        .into_iter()
        .find(|flag| !help_text.contains(flag))
    {
        return Attestation::Unavailable(format!(
            "this GitHub CLI's 'gh attestation verify' lacks {flag}; upgrade gh"
        ));
    }
    match run_gh(
        program,
        &[
            OsStr::new("attestation"),
            OsStr::new("verify"),
            archive.as_os_str(),
            OsStr::new("--repo"),
            OsStr::new(repo),
            OsStr::new("--signer-workflow"),
            OsStr::new(&release_signer_workflow(repo)),
            OsStr::new("--source-ref"),
            OsStr::new(&format!("refs/tags/{tag}")),
        ],
        limits,
    ) {
        Ok(output) if output.status.success() => Attestation::Verified,
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stdout));
            Attestation::Failed(text.trim().chars().take(2000).collect())
        }
        Err(error) => Attestation::Failed(format!("could not run gh: {error}")),
    }
}

/// What to do about the attestation result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttestationAction {
    Proceed,
    /// Needs `--allow-unattested` or an interactive "yes"; carries the reason.
    Confirm(String),
    Abort(String),
}

pub(crate) fn attestation_action(
    outcome: &Attestation,
    allow_unattested: bool,
) -> AttestationAction {
    match outcome {
        Attestation::Verified | Attestation::Skipped => AttestationAction::Proceed,
        Attestation::Failed(detail) => AttestationAction::Abort(format!(
            "build provenance verification failed; nothing was installed.\n{detail}\n\
             To install without provenance verification anyway, rerun with \
             MINI_AGENT_SKIP_ATTESTATION=1."
        )),
        Attestation::Unavailable(_) if allow_unattested => AttestationAction::Proceed,
        Attestation::Unavailable(reason) => AttestationAction::Confirm(reason.clone()),
    }
}

fn manual_attestation_hint(archive_name: &str) -> String {
    format!(
        "  Verify it yourself with the GitHub CLI (https://cli.github.com):\n    \
         gh attestation verify {archive_name} --repo {slug} --signer-workflow {workflow} \
         --source-ref refs/tags/<tag>",
        slug = crate::product::REPOSITORY_SLUG,
        workflow = release_signer_workflow(crate::product::REPOSITORY_SLUG),
    )
}

/// Asks on stderr/stdin; only an interactive terminal can answer "yes".
fn confirm(question: &str) -> bool {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok()
        && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Writes the archive into a fresh private directory so `gh` can read it.
fn stage_archive(name: &str, archive: &[u8]) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("mini-agent-update-{}", uuid::Uuid::new_v4()));
    crate::fs::ensure_private_directory(&dir)
        .map_err(|error| format!("cannot create a staging directory: {error}"))?;
    let path = dir.join(name);
    std::fs::write(&path, archive).map_err(|error| {
        let _ = std::fs::remove_dir_all(&dir);
        format!("cannot stage the archive for verification: {error}")
    })?;
    Ok(path)
}

async fn verify_provenance(
    archive_name: &str,
    archive: &[u8],
    tag: &str,
    allow_unattested: bool,
) -> Result<Attestation, String> {
    let staged = stage_archive(archive_name, archive)?;
    let skip = std::env::var("MINI_AGENT_SKIP_ATTESTATION").is_ok_and(|v| v == "1");
    let no_gh = std::env::var("MINI_AGENT_INSTALL_NO_GH").is_ok_and(|v| v == "1");
    let path = staged.clone();
    let pinned_tag = tag.to_owned();
    let outcome = tokio::task::spawn_blocking(move || {
        check_attestation(
            std::ffi::OsStr::new("gh"),
            &path,
            crate::product::REPOSITORY_SLUG,
            &pinned_tag,
            skip,
            no_gh,
        )
    })
    .await
    .map_err(|error| error.to_string());
    if let Some(dir) = staged.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let outcome = outcome?;
    match &outcome {
        Attestation::Verified => println!(
            "Verified build provenance for {archive_name} (gh attestation verify --repo {}, release workflow, {tag}).",
            crate::product::REPOSITORY_SLUG
        ),
        Attestation::Skipped => {
            eprintln!(
                "Warning: skipping build provenance verification (MINI_AGENT_SKIP_ATTESTATION=1)."
            );
            eprintln!("{}", manual_attestation_hint(archive_name));
        }
        Attestation::Unavailable(reason) => {
            eprintln!("Warning: build provenance was not verified: {reason}.");
            eprintln!(
                "  The archive matched SHA256SUMS, which is served from the same release;\n  \
                 that detects a damaged download, not a replaced release."
            );
            eprintln!("{}", manual_attestation_hint(archive_name));
        }
        Attestation::Failed(_) => {}
    }
    match attestation_action(&outcome, allow_unattested) {
        AttestationAction::Proceed => Ok(outcome),
        AttestationAction::Abort(message) => Err(message),
        AttestationAction::Confirm(_) => {
            if confirm("Install without build provenance verification?") {
                Ok(outcome)
            } else {
                Err(
                    "not installed: provenance could not be verified. Install and sign in to \
                     the GitHub CLI, or rerun with --allow-unattested to accept a \
                     checksum-only install."
                        .into(),
                )
            }
        }
    }
}

/// Whether `--version` output names exactly `version` as its version token:
/// `mini-agent 1.1.10` must not satisfy a request for `1.1.1`.
fn version_probe_matches(stdout: &str, version: &str) -> bool {
    let mut tokens = stdout.split_whitespace();
    tokens.next() == Some(BINARY_NAME) && tokens.next() == Some(version)
}

/// Runs the staged binary's `--version` before replacing anything, so a wrong
/// or broken executable never takes the installed one's place.
async fn install_verified(exe: &Path, binary: &[u8], version: &str) -> Result<(), String> {
    let directory: PathBuf = exe
        .parent()
        .ok_or("the executable has no parent directory")?
        .to_path_buf();
    let probe = directory.join(format!(".{BINARY_NAME}.probe.{}", std::process::id()));
    let binary = binary.to_vec();
    let probe_clone = probe.clone();
    let expected = format!("{BINARY_NAME} {version}");
    let requested_version = version.to_owned();
    let outcome = tokio::task::spawn_blocking(move || -> Result<(), String> {
        install_atomically(&probe_clone, &binary).map_err(|error| {
            format!(
                "cannot write to {}: {error} (is the directory writable? rerun with the permissions that installed it)",
                probe_clone.parent().map_or_else(String::new, |p| p.display().to_string())
            )
        })?;
        let output = run_bounded(
            std::process::Command::new(&probe_clone).arg("--version"),
            HelperLimits::from_env(),
        )
        .map_err(|error| format!("the downloaded executable does not run: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || !version_probe_matches(&stdout, &requested_version) {
            return Err(format!(
                "the downloaded executable reports {:?}, expected {expected:?}",
                stdout.trim()
            ));
        }
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?;
    let finish = outcome.and_then(|()| {
        std::fs::rename(&probe, exe)
            .map_err(|error| format!("cannot replace {}: {error}", exe.display()))
    });
    if finish.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    finish
}

#[cfg(test)]
mod tests {
    // mini-agent-et59d
    #[test]
    fn version_probe_requires_a_whole_version_token() {
        use super::version_probe_matches as m;
        assert!(m("mini-agent 1.1.1 (build abc)\n", "1.1.1"));
        assert!(m("  mini-agent 1.1.1\n", "1.1.1"));
        assert!(!m("mini-agent 1.1.10 (build abc)\n", "1.1.1"));
        assert!(!m("mini-agent 1.2.30\n", "1.2.3"));
        assert!(!m("other 1.1.1\n", "1.1.1"));
        assert!(!m("mini-agent-evil 1.1.1\n", "1.1.1"));
        assert!(!m("", "1.1.1"));
    }

    use super::*;
    use std::io::Write as _;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn tar_entry(name: &str, kind: u8, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[156] = kind;
        let mut out = header.to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    fn gz_tar(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut tar: Vec<u8> = entries.concat();
        tar.extend_from_slice(&[0u8; 1024]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn manifest_for(archive: &[u8], file: &str) -> String {
        format!("{}  {file}\n", sha256_hex(archive))
    }

    #[test]
    fn only_the_exact_update_forms_are_a_subcommand() {
        assert_eq!(
            parse_update_args(&args(&["mini-agent", "update"])),
            Some(UpdateArgs {
                check: false,
                release: None,
                allow_unattested: false,
                allow_downgrade: false
            })
        );
        assert_eq!(
            parse_update_args(&args(&[
                "mini-agent",
                "update",
                "--check",
                "--release",
                "1.2.3",
                "--allow-unattested",
                "--allow-downgrade"
            ])),
            Some(UpdateArgs {
                check: true,
                release: Some("1.2.3".into()),
                allow_unattested: true,
                allow_downgrade: true
            })
        );
        // Ordinary prompts and malformed forms stay with the normal command line.
        for other in [
            vec!["mini-agent", "update", "the", "docs"],
            vec!["mini-agent", "update", "--release"],
            vec!["mini-agent", "update", "--check", "--check"],
            vec![
                "mini-agent",
                "update",
                "--allow-downgrade",
                "--allow-downgrade",
            ],
            vec!["mini-agent", "-p", "update"],
            vec!["mini-agent"],
            vec!["mini-agent", "updates"],
        ] {
            assert_eq!(parse_update_args(&args(&other)), None, "{other:?}");
        }
    }

    #[test]
    fn package_managed_and_build_paths_are_refused_with_a_command() {
        let home = Some("/home/u");
        let managed = |path: &str| match classify_install(path, home, "1.13.0") {
            InstallChannel::Managed(message) => message,
            InstallChannel::SelfManaged => panic!("{path} must be refused"),
        };
        for brew in [
            "/opt/homebrew/Cellar/mini-agent/1.0/bin/mini-agent",
            "/home/linuxbrew/.linuxbrew/bin/mini-agent",
        ] {
            let message = managed(brew);
            // Never advertises a formula that does not exist.
            assert!(message.contains("`zerostack`"), "{message}");
            assert!(message.contains("releases page"), "{message}");
            assert!(message.contains("does not exist"), "{message}");
        }
        assert!(managed("/home/u/.cargo/bin/mini-agent").contains("cargo install --locked"));
        assert!(managed("/nix/store/abc-mini-agent/bin/mini-agent").contains("Nix"));
        assert!(managed("/usr/bin/mini-agent").contains("package manager"));
        assert!(
            managed("/home/u/.vscode/extensions/mini-agent-1/bin/linux-x64/mini-agent")
                .contains("extension")
        );
        assert!(managed("/work/mini-agent/target/release/mini-agent").contains("rebuild"));
        assert!(managed("/usr/lib/node_modules/x/bin/mini-agent").contains("npm"));
        for ok in [
            "/home/u/.local/bin/mini-agent",
            "/usr/local/bin/mini-agent",
            "/Users/u/bin/mini-agent",
        ] {
            if cfg!(unix) {
                assert_eq!(
                    classify_install(ok, home, "1.13.0"),
                    InstallChannel::SelfManaged,
                    "{ok}"
                );
            }
        }
    }

    #[test]
    fn manifest_rules_match_install_sh() {
        let hash = "a".repeat(64);
        let file = "mini-agent-x86_64-unknown-linux-musl.tar.gz";
        assert_eq!(
            manifest_digest(&format!("{hash}  {file}\n"), file).unwrap(),
            hash
        );
        for (bad, why) in [
            ("", "no entry"),
            (&format!("{hash}  other.tar.gz\n"), "no entry"),
            (&format!("{hash}  {file}\n{hash}  {file}\n"), "duplicate"),
            (&format!("{}  {file}\n", "A".repeat(64)), "malformed hash"),
            (&format!("{}  {file}\n", "a".repeat(63)), "malformed hash"),
            (&format!("{hash} {file}\n"), "malformed entry"),
            (&format!("{hash}  {file} extra\n"), "malformed entry"),
            (&format!("{hash}  *{file}\n"), "no entry"),
        ] {
            let error = manifest_digest(bad, file).unwrap_err();
            assert!(error.contains(why), "{bad:?}: {error}");
        }
    }

    #[test]
    fn a_verified_archive_yields_only_the_exact_executable() {
        let archive = gz_tar(&[
            tar_entry("LICENSE", b'0', b"gpl"),
            tar_entry("./mini-agent", b'0', b"#!binary"),
        ]);
        let manifest = manifest_for(&archive, "a.tar.gz");
        assert_eq!(
            verified_binary(&manifest, "a.tar.gz", &archive).unwrap(),
            b"#!binary"
        );
    }

    #[test]
    fn notices_from_the_archive_replace_installed_ones_when_the_directory_exists() {
        let archive = gz_tar(&[
            tar_entry("mini-agent", b'0', b"bin"),
            tar_entry("LICENSE", b'0', b"new gpl"),
            tar_entry("./THIRD_PARTY_LICENSES", b'0', b"new inventory"),
            tar_entry("evil/../NOTICE", b'0', b"must not match"),
        ]);
        let documents = archive_documents(&archive).unwrap();
        let names: Vec<&str> = documents.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["LICENSE", "THIRD_PARTY_LICENSES"]);

        let root = std::env::temp_dir().join(format!("ma-notice-{}", std::process::id()));
        let exe = root.join("bin").join("mini-agent");
        let directory = notice_directory(&exe).unwrap();
        assert_eq!(directory, root.join("share/doc/mini-agent"));
        // Missing directory: nothing is created, the outcome names it.
        assert_eq!(
            refresh_notice_documents(&directory, &documents),
            NoticeRefresh::Missing(directory.clone())
        );
        assert!(!directory.exists());
        // Existing directory: documents are replaced in place.
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("LICENSE"), "old gpl").unwrap();
        assert_eq!(
            refresh_notice_documents(&directory, &documents),
            NoticeRefresh::Refreshed(2)
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("LICENSE")).unwrap(),
            "new gpl"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("THIRD_PARTY_LICENSES")).unwrap(),
            "new inventory"
        );
        // A directory that cannot take the write reports a failure, not a panic.
        let blocked = root.join("blocked");
        std::fs::create_dir_all(blocked.join("LICENSE")).unwrap();
        assert!(matches!(
            refresh_notice_documents(&blocked, &documents),
            NoticeRefresh::Failed(..)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hostile_archives_and_manifests_install_nothing() {
        let good = gz_tar(&[tar_entry("mini-agent", b'0', b"x")]);
        let good_manifest = manifest_for(&good, "a.tar.gz");
        // Digest mismatch (tampered download) is rejected before extraction.
        let mut tampered = good.clone();
        let last = tampered.len() - 3;
        tampered[last] ^= 0xff;
        assert!(
            verified_binary(&good_manifest, "a.tar.gz", &tampered)
                .unwrap_err()
                .contains("checksum mismatch")
        );
        // Traversal-named, directory, symlink and suffix-named entries are not "mini-agent".
        for entry in [
            tar_entry("../mini-agent", b'0', b"x"),
            tar_entry("bin/mini-agent", b'0', b"x"),
            tar_entry("mini-agent", b'5', b""),
            tar_entry("mini-agent", b'2', b""),
            tar_entry("mini-agent.exe", b'0', b"x"),
        ] {
            let archive = gz_tar(&[entry]);
            let manifest = manifest_for(&archive, "a.tar.gz");
            assert!(
                verified_binary(&manifest, "a.tar.gz", &archive)
                    .unwrap_err()
                    .contains("does not contain")
            );
        }
        // Duplicate executables, an empty one, and a lying size field.
        let duplicate = gz_tar(&[
            tar_entry("mini-agent", b'0', b"a"),
            tar_entry("mini-agent", b'0', b"b"),
        ]);
        assert!(
            verified_binary(
                &manifest_for(&duplicate, "a.tar.gz"),
                "a.tar.gz",
                &duplicate
            )
            .unwrap_err()
            .contains("more than once")
        );
        let empty = gz_tar(&[tar_entry("mini-agent", b'0', b"")]);
        assert!(
            verified_binary(&manifest_for(&empty, "a.tar.gz"), "a.tar.gz", &empty)
                .unwrap_err()
                .contains("unreasonable size")
        );
        let mut lying = tar_entry("mini-agent", b'0', b"abc");
        lying[124..136].copy_from_slice(b"77777777777\0");
        let huge = gz_tar(&[lying]);
        assert!(
            verified_binary(&manifest_for(&huge, "a.tar.gz"), "a.tar.gz", &huge)
                .unwrap_err()
                .contains("unreasonable size")
        );
        // Not gzip / truncated archives fail cleanly.
        let junk = b"not an archive".to_vec();
        assert!(verified_binary(&manifest_for(&junk, "a.tar.gz"), "a.tar.gz", &junk).is_err());
        let truncated = good[..good.len() / 2].to_vec();
        assert!(
            verified_binary(
                &manifest_for(&truncated, "a.tar.gz"),
                "a.tar.gz",
                &truncated
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_replaces_atomically_and_keeps_a_running_inode_intact() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("self-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("mini-agent");
        std::fs::write(&target, b"old").unwrap();
        let held = std::fs::File::open(&target).unwrap();
        install_atomically(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let mut old = String::new();
        (&held).read_to_string(&mut old).unwrap();
        assert_eq!(old, "old", "an open handle still sees the previous file");
        // No staging file is left behind, also when the target directory is gone.
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1);
        assert!(install_atomically(&dir.join("missing").join("mini-agent"), b"x").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    fn stub_gh(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("gh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn short_limits() -> HelperLimits {
        HelperLimits {
            deadline: Duration::from_millis(400),
            max_output: 4096,
        }
    }

    // mini-agent-xorxl.3: a stalled gh or probe is bounded and its tree reaped.
    #[cfg(unix)]
    #[test]
    fn stalled_gh_times_out_without_surviving_descendants() {
        let dir = std::env::temp_dir().join(format!("stall-gh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("a.tar.gz");
        let marker = dir.join("leaked");
        let run = |gh: &Path| {
            let started = std::time::Instant::now();
            let outcome = check_attestation_with(
                gh.as_os_str(),
                &archive,
                "owner/repo",
                "v1.2.3",
                false,
                false,
                short_limits(),
            );
            assert!(started.elapsed() < Duration::from_secs(5));
            outcome
        };
        // Stalled sign-in probe: unavailable (still needs confirmation), never verified.
        let stall_auth = stub_gh(
            &dir,
            &format!(
                "if [ \"$1\" = auth ]; then trap '' TERM; (sleep 2; echo x > {}) & wait; fi\nexit 0",
                marker.display()
            ),
        );
        let outcome = run(&stall_auth);
        assert!(
            matches!(&outcome, Attestation::Unavailable(m) if m.contains("timed out")),
            "{outcome:?}"
        );
        // Stalled verification: a failure, which aborts even with --allow-unattested.
        let stall_verify = stub_gh(
            &dir,
            "if [ \"$3\" = --help ]; then echo '--signer-workflow --source-ref'; exit 0; fi\nif [ \"$1\" = attestation ]; then sleep 30; fi\nexit 0",
        );
        let outcome = run(&stall_verify);
        assert!(
            matches!(&outcome, Attestation::Failed(m) if m.contains("timed out")),
            "{outcome:?}"
        );
        assert!(matches!(
            attestation_action(&outcome, true),
            AttestationAction::Abort(_)
        ));
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!marker.exists(), "descendant survived the timeout");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_that_never_exits_or_floods_is_bounded() {
        let dir = std::env::temp_dir().join(format!("stall-probe-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let hang = stub_gh(&dir, "sleep 30");
        let error = run_bounded(
            std::process::Command::new(&hang).arg("--version"),
            short_limits(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let flood = stub_gh(&dir, "yes | head -c 100000000");
        let error = run_bounded(
            std::process::Command::new(&flood).arg("--version"),
            short_limits(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("exceeded"), "{error}");
        // Normal output within bounds still passes through exactly.
        let ok = stub_gh(&dir, "echo mini-agent 1.2.3");
        let output = run_bounded(&mut std::process::Command::new(&ok), short_limits()).unwrap();
        assert!(version_probe_matches(
            &String::from_utf8_lossy(&output.stdout),
            "1.2.3"
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn attestation_uses_gh_and_only_a_rejection_by_a_working_gh_aborts() {
        let dir = std::env::temp_dir().join(format!("stub-gh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("a.tar.gz");
        let log = dir.join("calls.log");
        let run = |gh: &Path, skip: bool, no_gh: bool| {
            check_attestation(
                gh.as_os_str(),
                &archive,
                "owner/repo",
                "v1.2.3",
                skip,
                no_gh,
            )
        };
        // A working gh that accepts the archive: the verify call names archive and repo.
        let ok = stub_gh(
            &dir,
            &format!(
                "echo \"$@\" >> {}\nif [ \"$3\" = --help ]; then echo '--signer-workflow --source-ref'; fi\nexit 0",
                log.display()
            ),
        );
        assert_eq!(run(&ok, false, false), Attestation::Verified);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains(&format!(
                "attestation verify {} --repo owner/repo --signer-workflow \
                 owner/repo/.github/workflows/release.yml --source-ref refs/tags/v1.2.3",
                archive.display()
            )),
            "{calls}"
        );
        // Rejected archive: failure carrying gh's message, and the action aborts.
        let reject = stub_gh(
            &dir,
            "if [ \"$1\" = attestation ] && [ \"$3\" = --help ]; then echo '--signer-workflow --source-ref'; exit 0; fi\nif [ \"$1\" = attestation ]; then echo 'no matching attestation' >&2; exit 1; fi\nexit 0",
        );
        let outcome = run(&reject, false, false);
        assert!(
            matches!(&outcome, Attestation::Failed(m) if m.contains("no matching attestation"))
        );
        // Even the escape flag for unattested installs cannot override a rejection.
        assert!(matches!(
            attestation_action(&outcome, true),
            AttestationAction::Abort(_)
        ));
        // Unauthenticated gh and a gh without the subcommand are "unavailable".
        let unauth = stub_gh(&dir, "if [ \"$1\" = auth ]; then exit 1; fi\nexit 0");
        assert!(
            matches!(run(&unauth, false, false), Attestation::Unavailable(m) if m.contains("signed in"))
        );
        let old = stub_gh(&dir, "if [ \"$1\" = attestation ]; then exit 2; fi\nexit 0");
        assert!(
            matches!(run(&old, false, false), Attestation::Unavailable(m) if m.contains("upgrade gh"))
        );
        // A gh with the subcommand but without the pin flags: unavailable, never
        // a failed verification (the verify call must not even be made).
        let no_flags_log = dir.join("no-flags.log");
        let no_flags = stub_gh(
            &dir,
            &format!(
                "echo \"$@\" >> {}\nif [ \"$1\" = attestation ] && [ \"$3\" = --help ]; then echo '--repo --owner'; exit 0; fi\nif [ \"$1\" = attestation ]; then echo 'unknown flag: --signer-workflow' >&2; exit 1; fi\nexit 0",
                no_flags_log.display()
            ),
        );
        let outcome = run(&no_flags, false, false);
        assert!(
            matches!(&outcome, Attestation::Unavailable(m) if m.contains("--signer-workflow") && m.contains("upgrade gh")),
            "{outcome:?}"
        );
        let calls = std::fs::read_to_string(&no_flags_log).unwrap();
        assert!(!calls.contains("--repo owner/repo"), "{calls}");
        // Only one of the two flags advertised is still unavailable.
        let half = stub_gh(
            &dir,
            "if [ \"$1\" = attestation ] && [ \"$3\" = --help ]; then echo '--signer-workflow' >&2; fi\nexit 0",
        );
        assert!(
            matches!(run(&half, false, false), Attestation::Unavailable(m) if m.contains("--source-ref"))
        );
        // Missing gh, MINI_AGENT_INSTALL_NO_GH and the skip switch.
        let missing = dir.join("nope");
        assert!(
            matches!(run(&missing, false, false), Attestation::Unavailable(m) if m.contains("not installed"))
        );
        assert!(matches!(run(&ok, false, true), Attestation::Unavailable(_)));
        assert_eq!(run(&reject, true, false), Attestation::Skipped);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unattested_installs_need_the_flag_or_a_confirmation() {
        let unavailable = Attestation::Unavailable("gh missing".into());
        assert_eq!(
            attestation_action(&unavailable, false),
            AttestationAction::Confirm("gh missing".into())
        );
        assert_eq!(
            attestation_action(&unavailable, true),
            AttestationAction::Proceed
        );
        assert_eq!(
            attestation_action(&Attestation::Verified, false),
            AttestationAction::Proceed
        );
        assert_eq!(
            attestation_action(&Attestation::Skipped, false),
            AttestationAction::Proceed
        );
        assert!(matches!(
            attestation_action(&Attestation::Failed("bad".into()), false),
            AttestationAction::Abort(m) if m.contains("MINI_AGENT_SKIP_ATTESTATION=1")
        ));
        assert!(
            manual_attestation_hint("a.tar.gz").contains("gh attestation verify a.tar.gz --repo ")
        );
    }
}
