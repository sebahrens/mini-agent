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
//! checked here: run `gh attestation verify` on the archive, or reinstall with
//! `install.sh`, which also refreshes the bundled notice documents.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

const BINARY_NAME: &str = "mini-agent";
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UpdateArgs {
    pub check: bool,
    pub release: Option<String>,
}

/// Recognises `mini-agent update` with only `--check` and `--release X.Y.Z`
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
    };
    while let Some(arg) = args.next() {
        match arg.to_str()? {
            "--check" if !parsed.check => parsed.check = true,
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
        return managed("Installed with Homebrew: run `brew upgrade mini-agent`.");
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
            if size == 0 || size > MAX_BINARY_BYTES {
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
    found.ok_or_else(|| format!("archive does not contain the {name} executable"))
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

fn https_only_redirects() -> reqwest::redirect::Policy {
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
    crate::update::fetch_latest_within(&endpoint, API_TIMEOUT)
        .await
        .ok_or_else(|| "could not read the latest release from GitHub".to_string())
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
    install_verified(&exe, &binary, &requested).await?;
    println!(
        "Updated mini-agent {current} -> {requested} at {}",
        exe.display()
    );
    eprintln!(
        "Note: build provenance was not checked. Verify it with \
         `gh attestation verify {archive_name} --repo {}`; install.sh also refreshes the \
         bundled notice documents.",
        crate::product::REPOSITORY_SLUG
    );
    Ok(())
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
    let outcome = tokio::task::spawn_blocking(move || -> Result<(), String> {
        install_atomically(&probe_clone, &binary).map_err(|error| {
            format!(
                "cannot write to {}: {error} (is the directory writable? rerun with the permissions that installed it)",
                probe_clone.parent().map_or_else(String::new, |p| p.display().to_string())
            )
        })?;
        let output = std::process::Command::new(&probe_clone)
            .arg("--version")
            .output()
            .map_err(|error| format!("the downloaded executable does not run: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || !stdout.trim_start().starts_with(&expected) {
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
                release: None
            })
        );
        assert_eq!(
            parse_update_args(&args(&[
                "mini-agent",
                "update",
                "--check",
                "--release",
                "1.2.3"
            ])),
            Some(UpdateArgs {
                check: true,
                release: Some("1.2.3".into())
            })
        );
        // Ordinary prompts and malformed forms stay with the normal command line.
        for other in [
            vec!["mini-agent", "update", "the", "docs"],
            vec!["mini-agent", "update", "--release"],
            vec!["mini-agent", "update", "--check", "--check"],
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
        assert!(
            managed("/opt/homebrew/Cellar/mini-agent/1.0/bin/mini-agent").contains("brew upgrade")
        );
        assert!(managed("/home/linuxbrew/.linuxbrew/bin/mini-agent").contains("brew upgrade"));
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
}
