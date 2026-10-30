//! Opt-in, anonymous interactive release notices. No downloads or provider data.

use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 128 * 1024;
const MAX_CACHE_BYTES: u64 = 4096;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    schema: u32,
    checked_at: u64,
    latest_version: Option<String>,
}

struct CheckLease(std::fs::File);

impl Drop for CheckLease {
    fn drop(&mut self) {
        // An owned child may briefly inherit the open description before exec.
        // Explicit unlock releases admission without waiting for that child.
        let _ = self.0.unlock();
    }
}

fn stable_version(value: &str) -> Option<[u64; 3]> {
    if value.len() > 64 {
        return None;
    }
    let mut parts = value.strip_prefix('v').unwrap_or(value).split('.');
    let mut version = [0; 3];
    for component in &mut version {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *component = part.parse().ok()?;
    }
    parts.next().is_none().then_some(version)
}

fn normalized_version(value: &str) -> Option<String> {
    let [major, minor, patch] = stable_version(value)?;
    Some(format!("{major}.{minor}.{patch}"))
}

fn github_latest_url(repository: &str) -> Option<String> {
    let repository = repository.strip_prefix("https://github.com/")?;
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    let mut parts = repository.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some()
        || [owner, name].iter().any(|part| {
            part.is_empty()
                || part.len() > 100
                || matches!(*part, "." | "..")
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
    {
        return None;
    }
    Some(format!(
        "https://api.github.com/repos/{owner}/{name}/releases/latest"
    ))
}

fn notice(version: Option<&str>, current: &str) -> Option<String> {
    let version = version?;
    if stable_version(version)? <= stable_version(current)? {
        return None;
    }
    let version = normalized_version(version)?;
    Some(format!(
        "[update] v{version} available. Install with:\n  curl -fsSL https://raw.githubusercontent.com/{}/main/install.sh | bash -s -- --release {version}",
        crate::product::REPOSITORY_SLUG
    ))
}

pub(crate) fn should_check(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    interactive: bool,
) -> bool {
    if !interactive || cli.print || !cfg.update_check.unwrap_or(false) {
        return false;
    }
    #[cfg(feature = "loop")]
    if cli.loop_mode {
        return false;
    }
    #[cfg(feature = "acp")]
    if cli.acp_enabled {
        return false;
    }
    true
}

static PENDING_STARTUP_NOTICE: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<String>>> =
    std::sync::Mutex::new(None);

/// Starts the opt-in check in the background so the TUI never waits on the
/// network or its cache writes; the run loop shows the notice when it lands.
pub(crate) fn spawn_startup_notice(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    interactive: bool,
    state: &Path,
) {
    if !should_check(cli, cfg, interactive) {
        return;
    }
    let state = state.to_path_buf();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Some(notice) = checked_notice(&state).await {
            let _ = tx.send(notice);
        }
    });
    *PENDING_STARTUP_NOTICE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(rx);
}

/// The receiver for a notice started by [`spawn_startup_notice`], once.
pub(crate) fn take_startup_notice() -> Option<tokio::sync::oneshot::Receiver<String>> {
    PENDING_STARTUP_NOTICE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

#[cfg(test)]
pub(crate) async fn startup_notice(
    cli: &crate::cli::Cli,
    cfg: &crate::config::Config,
    interactive: bool,
    state: &Path,
) -> Option<String> {
    if !should_check(cli, cfg, interactive) {
        return None;
    }
    checked_notice(state).await
}

async fn checked_notice(state: &Path) -> Option<String> {
    let endpoint = github_latest_url(env!("CARGO_PKG_REPOSITORY"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    match check_cached(state, now, env!("CARGO_PKG_VERSION"), || {
        fetch_latest(&endpoint)
    })
    .await
    {
        Ok(notice) => notice,
        Err(_) => {
            tracing::debug!("optional update check unavailable");
            None
        }
    }
}

fn read_cache(path: &Path) -> io::Result<Option<Cache>> {
    let file = match crate::fs::open_private_file(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(io::Error::other("update cache exceeds its bound"));
    }
    let cache: Cache = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if cache.schema != 1
        || cache
            .latest_version
            .as_deref()
            .is_some_and(|v| stable_version(v).is_none())
    {
        return Err(io::Error::other("unsupported update cache"));
    }
    Ok(Some(cache))
}

fn save_cache(path: &Path, cache: &Cache) -> io::Result<()> {
    crate::fs::private_atomic_write_sync(
        path,
        &serde_json::to_vec(cache).map_err(io::Error::other)?,
    )
}

fn recently_checked(checked_at: u64, now: u64) -> bool {
    now.saturating_sub(checked_at) < CHECK_INTERVAL_SECS
}

async fn check_cached<F, Fut>(
    state: &Path,
    now: u64,
    current: &str,
    fetch: F,
) -> io::Result<Option<String>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let directory = state.join("update");
    crate::fs::ensure_private_directory(&directory)?;
    let lock_path = directory.join("check.lock");
    match crate::fs::private_atomic_create_sync(&lock_path, b"") {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let lease = CheckLease(crate::fs::open_private_file(&lock_path)?);
    match lease.0.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    let cache_path = directory.join("latest.json");
    let previous = read_cache(&cache_path)?;
    if let Some(cache) = &previous
        && recently_checked(cache.checked_at, now)
    {
        return Ok(notice(cache.latest_version.as_deref(), current));
    }
    let mut cache = Cache {
        schema: 1,
        checked_at: now,
        latest_version: previous.and_then(|cache| cache.latest_version),
    };
    // Reserve this day's attempt durably before network work. Cancellation or
    // an offline/rate-limited response must not trigger a request every startup.
    save_cache(&cache_path, &cache)?;
    if let Ok(Some(version)) = tokio::time::timeout(REQUEST_TIMEOUT, fetch()).await {
        cache.latest_version = normalized_version(&version);
        save_cache(&cache_path, &cache)?;
    }
    Ok(notice(cache.latest_version.as_deref(), current))
}

#[cfg(test)]
pub(crate) async fn verify_cross_process_admission(
    state: &Path,
    now: u64,
    hold: bool,
) -> io::Result<()> {
    if hold {
        let admitted = check_cached(state, now, env!("CARGO_PKG_VERSION"), || {
            std::future::pending()
        });
        tokio::pin!(admitted);
        assert!(futures::poll!(admitted.as_mut()).is_pending());
        crate::tests::update_tests::run_admission_child(state, now).await;
    } else {
        assert!(
            check_cached(state, now, env!("CARGO_PKG_VERSION"), || async {
                panic!("a second process must not admit another request in the same interval")
            })
            .await?
            .is_none()
        );
    }
    Ok(())
}

async fn fetch_latest(endpoint: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("mini-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let mut response = client
        .get(endpoint)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
        draft: bool,
        prerelease: bool,
    }
    let release: Release = serde_json::from_slice(&bytes).ok()?;
    if release.draft || release.prerelease {
        return None;
    }
    normalized_version(&release.tag_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn update_versions_compare_numerically_and_reject_command_text() {
        assert!(
            notice(Some("v1.10.0"), "1.9.4")
                .unwrap()
                .contains("--release 1.10.0")
        );
        assert!(notice(Some("1.9.4"), "1.9.4").is_none());
        assert!(notice(Some("1.9.3"), "1.9.4").is_none());
        assert!(notice(Some("2.0.0"), "1.99.99").is_some());
        for invalid in [
            "1.2",
            "1.2.3.4",
            "1.02.3",
            "1.2.3-rc1",
            "1.2.3+meta",
            "1.2.3; touch /tmp/effect",
            "\u{1b}[31m1.2.3",
            "18446744073709551616.0.0",
        ] {
            assert!(stable_version(invalid).is_none(), "{invalid:?}");
            assert!(notice(Some(invalid), "1.0.0").is_none());
        }
    }

    #[test]
    fn update_cache_ttl_has_exact_day_boundary_and_clock_rollback_does_not_retry() {
        assert!(recently_checked(100, 100));
        assert!(recently_checked(100, 99));
        assert!(recently_checked(100, 100 + CHECK_INTERVAL_SECS - 1));
        assert!(!recently_checked(100, 100 + CHECK_INTERVAL_SECS));
        assert!(!recently_checked(100, u64::MAX));
    }

    #[test]
    fn update_endpoint_uses_only_a_literal_github_repository() {
        assert_eq!(
            github_latest_url(env!("CARGO_PKG_REPOSITORY")).as_deref(),
            Some("https://api.github.com/repos/sebahrens/mini-agent/releases/latest")
        );
        assert_eq!(
            github_latest_url("https://github.com/example/repo.git").as_deref(),
            Some("https://api.github.com/repos/example/repo/releases/latest")
        );
        for invalid in [
            "http://github.com/a/b",
            "https://github.com/a/b/extra",
            "https://github.com/a/b?secret=key",
            "https://github.com/../b",
            "https://github.com//b",
            "https://other.example/a/b",
        ] {
            assert!(github_latest_url(invalid).is_none());
        }
    }

    #[tokio::test]
    async fn update_checks_are_opt_in_and_inert_outside_interactive_mode() {
        let root = crate::tests::OwnedTestDirectory::new("update-modes");
        let state = root.path().join("must-stay-absent");
        let cli = crate::cli::Cli::default();
        assert!(
            startup_notice(&cli, &crate::config::Config::default(), true, &state)
                .await
                .is_none()
        );
        let cfg = crate::config::Config {
            update_check: Some(true),
            ..Default::default()
        };
        assert!(should_check(&cli, &cfg, true));
        assert!(startup_notice(&cli, &cfg, false, &state).await.is_none());
        let headless = crate::cli::Cli {
            print: true,
            ..Default::default()
        };
        assert!(
            startup_notice(&headless, &cfg, true, &state)
                .await
                .is_none()
        );
        #[cfg(feature = "loop")]
        assert!(!should_check(
            &crate::cli::Cli {
                loop_mode: true,
                ..Default::default()
            },
            &cfg,
            true
        ));
        #[cfg(feature = "acp")]
        assert!(!should_check(
            &crate::cli::Cli {
                acp_enabled: true,
                ..Default::default()
            },
            &cfg,
            true
        ));
        assert!(
            !state.exists(),
            "excluded modes must not even create cache state"
        );
    }

    #[tokio::test]
    async fn update_cache_reserves_failed_and_cancelled_attempts_without_redownloading() {
        let root = crate::tests::OwnedTestDirectory::new("update-cache");
        let calls = AtomicUsize::new(0);
        let first = check_cached(root.path(), 100, "1.9.4", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Some("v1.10.0".into())
        })
        .await
        .unwrap();
        assert!(first.unwrap().contains("--release 1.10.0"));
        let cached = check_cached(root.path(), 101, "1.9.4", || async {
            panic!("cache hit must not request a release")
        })
        .await
        .unwrap();
        assert!(cached.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let next_day = 100 + CHECK_INTERVAL_SECS;
        let offline = check_cached(root.path(), next_day, "1.10.0", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            None
        })
        .await
        .unwrap();
        assert!(offline.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        check_cached(root.path(), next_day + 1, "1.10.0", || async {
            panic!("offline attempts must be cached too")
        })
        .await
        .unwrap();

        let cancellation_day = next_day + CHECK_INTERVAL_SECS;
        {
            let cancelled = check_cached(root.path(), cancellation_day, "1.10.0", || {
                std::future::pending()
            });
            tokio::pin!(cancelled);
            assert!(futures::poll!(cancelled.as_mut()).is_pending());
            // A second opener has independent ownership: it cannot borrow the
            // first check's lock or create another in-flight request.
            assert!(
                check_cached(root.path(), cancellation_day, "1.9.4", || async {
                    panic!("concurrent check must not request")
                })
                .await
                .unwrap()
                .is_none()
            );
        }
        // Polling above durably wrote the attempt before waiting for network.
        let cache = read_cache(&root.path().join("update/latest.json"))
            .unwrap()
            .unwrap();
        assert_eq!(cache.checked_at, cancellation_day);
        check_cached(root.path(), cancellation_day + 1, "1.10.0", || async {
            panic!("cancelled attempt must remain reserved after lease retirement")
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_cache_rejects_unbounded_or_untrusted_state_before_network() {
        let root = crate::tests::OwnedTestDirectory::new("update-cache-invalid");
        let directory = root.path().join("update");
        crate::fs::ensure_private_directory(&directory).unwrap();
        let path = directory.join("latest.json");
        for contents in [
            vec![b'x'; MAX_CACHE_BYTES as usize + 1],
            br#"{"schema":2,"checked_at":0,"latest_version":null}"#.to_vec(),
            br#"{"schema":1,"checked_at":0,"latest_version":"1.2.3;evil"}"#.to_vec(),
        ] {
            crate::fs::private_atomic_write_sync(&path, &contents).unwrap();
            assert!(
                check_cached(root.path(), 100, "1.0.0", || async {
                    panic!("invalid cache must not cause network work")
                })
                .await
                .is_err()
            );
        }
        #[cfg(unix)]
        {
            let target = root.path().join("other.json");
            std::fs::write(&target, "untouched").unwrap();
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            assert!(
                check_cached(root.path(), 100, "1.0.0", || async {
                    panic!("linked cache must not cause network work")
                })
                .await
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
        }
    }

    struct HttpFixture {
        endpoint: String,
        task: Option<tokio::task::JoinHandle<String>>,
        release: std::sync::Arc<tokio::sync::Notify>,
    }

    impl HttpFixture {
        async fn start(response: Vec<u8>, stall_after_headers: bool) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/releases/latest", listener.local_addr().unwrap());
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            let task_release = release.clone();
            let task = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        let count = socket.read(&mut buffer).await.unwrap();
                        assert!(count > 0 && request.len() + count <= 16 * 1024);
                        request.extend_from_slice(&buffer[..count]);
                    }
                    // A response-limit test may close the socket before its
                    // oversized body is completely sent.
                    let _ = socket.write_all(&response).await;
                    if stall_after_headers {
                        task_release.notified().await;
                    }
                    String::from_utf8(request).unwrap()
                })
                .await
                .expect("owned HTTP fixture deadline")
            });
            Self {
                endpoint,
                task: Some(task),
                release,
            }
        }

        async fn finish(mut self) -> String {
            self.release.notify_one();
            let request = tokio::time::timeout(Duration::from_secs(5), self.task.as_mut().unwrap())
                .await
                .expect("owned HTTP fixture must retire")
                .unwrap();
            self.task.take();
            request
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.release.notify_one();
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    fn response(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn update_http_reads_only_valid_stable_release_metadata_without_credentials() {
        for (body, expected) in [
            (
                br#"{"tag_name":"v1.10.0","draft":false,"prerelease":false,"body":"ignored"}"#
                    .as_slice(),
                Some("1.10.0"),
            ),
            (
                br#"{"tag_name":"v2.0.0","draft":true,"prerelease":false}"#.as_slice(),
                None,
            ),
            (
                br#"{"tag_name":"v2.0.0","draft":false,"prerelease":true}"#.as_slice(),
                None,
            ),
            (
                br#"{"tag_name":"v2.0.0-rc1","draft":false,"prerelease":false}"#.as_slice(),
                None,
            ),
            (br#"{"tag_name":"v2.0.0"}"#.as_slice(), None),
        ] {
            let fixture = HttpFixture::start(response(body), false).await;
            assert_eq!(fetch_latest(&fixture.endpoint).await.as_deref(), expected);
            let request = fixture.finish().await.to_ascii_lowercase();
            assert!(request.starts_with("get /releases/latest http/1.1\r\n"));
            assert!(!request.contains("authorization:") && !request.contains("cookie:"));
            assert!(request.contains("accept: application/vnd.github+json"));
        }
    }

    #[tokio::test]
    async fn update_http_refuses_redirects_and_bounds_declared_and_chunked_bodies() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap()
        );
        let fixture = HttpFixture::start(redirect.into_bytes(), false).await;
        assert!(fetch_latest(&fixture.endpoint).await.is_none());
        fixture.finish().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err(),
            "redirect destination must receive no connection"
        );

        let fixture =
            HttpFixture::start(response(&vec![b'x'; MAX_RESPONSE_BYTES + 1]), false).await;
        assert!(fetch_latest(&fixture.endpoint).await.is_none());
        fixture.finish().await;
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        chunked.extend(vec![b'x'; MAX_RESPONSE_BYTES + 1]);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let fixture = HttpFixture::start(chunked, false).await;
        assert!(fetch_latest(&fixture.endpoint).await.is_none());
        fixture.finish().await;
    }

    #[tokio::test]
    async fn update_http_body_stall_has_a_finite_deadline_and_retires_its_fixture() {
        let fixture = HttpFixture::start(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".to_vec(),
            true,
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_secs(5), fetch_latest(&fixture.endpoint))
                .await
                .expect("release body must obey the two-second request deadline")
                .is_none()
        );
        fixture.finish().await;
    }
}
