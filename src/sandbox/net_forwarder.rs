//! In-namespace half of `sandbox-network = "allowlist"` on Linux `bwrap`.
//!
//! The contained command runs in an empty network namespace, so it cannot
//! reach the host proxy's loopback port. `bwrap` bind-mounts the proxy's
//! private Unix socket into the namespace and starts the command through
//! `mini-agent __mini-agent-net-forwarder SOCKET PORT -- COMMAND...`. This
//! helper binds `127.0.0.1:PORT` (the namespace's own loopback), forks a
//! detached bridge that relays each connection to the Unix socket, and then
//! `exec`s the command, so the command keeps its process identity, stdio and
//! exit status. The bridge relays bytes only; host filtering, prompting and
//! the token check all happen in the host proxy. The namespace has no other
//! interface, so the proxy is the only way out.

// `fork` and raw descriptor hygiene have no safe std equivalent; each unsafe
// block below is a plain libc call with a stated precondition.
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// First argument that selects forwarder mode.
pub const FORWARDER_ARG: &str = "__mini-agent-net-forwarder";

/// Run forwarder mode when the process was started as the helper. Returns
/// `None` for an ordinary start. On success the command is `exec`ed and this
/// never returns.
pub(crate) fn maybe_run_from_args() -> Option<ExitCode> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).map(OsString::as_os_str) != Some(std::ffi::OsStr::new(FORWARDER_ARG)) {
        return None;
    }
    Some(match run(&args[2..]) {
        Ok(never) => match never {},
        Err(why) => {
            eprintln!("mini-agent net forwarder: {why}");
            ExitCode::from(127)
        }
    })
}

fn run(args: &[OsString]) -> Result<std::convert::Infallible, String> {
    let (socket, port, command) = parse(args)?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .map_err(|error| format!("cannot listen on 127.0.0.1:{port}: {error}"))?;
    // SAFETY: the helper is single-threaded at this point (it runs before any
    // runtime starts), so the child may allocate and spawn threads.
    match unsafe { libc::fork() } {
        -1 => return Err(format!("fork failed: {}", std::io::Error::last_os_error())),
        0 => {
            detach_bridge();
            close_other_descriptors(listener.as_raw_fd());
            serve(&listener, &socket);
            // `serve` only returns if accepting fails for good.
            std::process::exit(0);
        }
        _ => {}
    }
    drop(listener);
    let error = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    Err(format!("cannot exec {:?}: {error}", command[0]))
}

fn parse(args: &[OsString]) -> Result<(PathBuf, u16, &[OsString]), String> {
    let usage = "usage: SOCKET PORT -- COMMAND [ARG...]";
    let [socket, port, separator, command @ ..] = args else {
        return Err(usage.into());
    };
    if separator != "--" || command.is_empty() {
        return Err(usage.into());
    }
    let port = port
        .to_str()
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or("bad port")?;
    Ok((PathBuf::from(socket), port, command))
}

/// The bridge must not hold the command's stdio open or outlive it.
fn detach_bridge() {
    // SAFETY: plain syscalls on descriptors owned by this process.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..=2 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
        #[cfg(target_os = "linux")]
        {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
            if libc::getppid() == 1 {
                libc::_exit(0);
            }
        }
    }
}

/// Drop every inherited descriptor (notably the workspace-authority fd) except
/// the listener.
fn close_other_descriptors(keep: i32) {
    for fd in 3..4096 {
        if fd != keep {
            // SAFETY: closing an arbitrary descriptor number; EBADF is fine.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

/// Most connections relayed at once; each costs two threads, so extra
/// connections are refused (closed) instead of exhausting the namespace.
const MAX_CONCURRENT_RELAYS: usize = 256;

/// Frees a relay slot when the connection ends, however it ends.
struct RelaySlot(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for RelaySlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Relay every loopback connection to the proxy socket.
pub(crate) fn serve(listener: &TcpListener, socket: &Path) {
    serve_capped(listener, socket, MAX_CONCURRENT_RELAYS);
}

fn serve_capped(listener: &TcpListener, socket: &Path, cap: usize) {
    use std::sync::atomic::Ordering;
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut backoff = super::net_proxy::AcceptBackoff::default();
    for client in listener.incoming() {
        let Ok(client) = client else {
            // EMFILE and friends fail at once, forever: sleep instead of spinning.
            std::thread::sleep(backoff.failed().0);
            continue;
        };
        backoff.succeeded();
        if active.fetch_add(1, Ordering::AcqRel) >= cap {
            active.fetch_sub(1, Ordering::AcqRel);
            drop(client);
            continue;
        }
        let slot = RelaySlot(active.clone());
        let socket = socket.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("net-forwarder-conn".into())
            .spawn(move || {
                let _slot = slot;
                relay(client, &socket);
            });
        // A failed spawn drops the closure, which frees the slot.
        drop(spawned);
    }
}

fn relay(client: TcpStream, socket: &Path) {
    let Ok(upstream) = UnixStream::connect(socket) else {
        return;
    };
    let (Ok(mut client_read), Ok(mut upstream_write)) = (client.try_clone(), upstream.try_clone())
    else {
        return;
    };
    let uplink = std::thread::spawn(move || {
        let _ = copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(std::net::Shutdown::Write);
    });
    let (mut upstream_read, mut client_write) = (upstream, client);
    let _ = copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Write);
    let _ = uplink.join();
}

fn copy(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = from.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        to.write_all(&buffer[..read])?;
    }
}

#[cfg(test)]
mod tests;
