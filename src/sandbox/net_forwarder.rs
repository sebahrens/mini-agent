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

/// Relay every loopback connection to the proxy socket.
pub(crate) fn serve(listener: &TcpListener, socket: &Path) {
    for client in listener.incoming() {
        let Ok(client) = client else {
            continue;
        };
        let socket = socket.to_path_buf();
        let _ = std::thread::Builder::new()
            .name("net-forwarder-conn".into())
            .spawn(move || relay(client, &socket));
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
