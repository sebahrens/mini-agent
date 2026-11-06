use super::*;
use std::os::unix::net::UnixListener;

fn os(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

#[test]
fn arguments_require_socket_port_separator_and_command() {
    let args = os(&["/s.sock", "3128", "--", "sh", "-c", "true"]);
    let (socket, port, command) = parse(&args).unwrap();
    assert_eq!(socket, PathBuf::from("/s.sock"));
    assert_eq!(port, 3128);
    assert_eq!(command.len(), 3);
    for bad in [
        os(&[]),
        os(&["/s", "3128"]),
        os(&["/s", "3128", "--"]),
        os(&["/s", "3128", "sh"]),
        os(&["/s", "0", "--", "sh"]),
        os(&["/s", "x", "--", "sh"]),
    ] {
        assert!(parse(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn ordinary_start_is_not_forwarder_mode() {
    // The test binary's argv[1] is never the forwarder argument.
    assert!(maybe_run_from_args().is_none());
}

#[test]
fn bridge_relays_bytes_both_ways_and_nowhere_else() {
    let dir = PathBuf::from("/tmp").join(format!(
        "mafwd-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    std::fs::create_dir(&dir).unwrap();
    let socket = dir.join("p.sock");
    let upstream = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for stream in upstream.incoming() {
            let mut stream = stream.unwrap();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 64];
                let read = stream.read(&mut buffer).unwrap();
                let mut reply = b"echo:".to_vec();
                reply.extend_from_slice(&buffer[..read]);
                stream.write_all(&reply).unwrap();
            });
        }
    });
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let socket_for_bridge = socket.clone();
    std::thread::spawn(move || serve(&listener, &socket_for_bridge));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    client.write_all(b"hello").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut answer = String::new();
    client.read_to_string(&mut answer).unwrap();
    assert_eq!(answer, "echo:hello");

    // A dead upstream closes the client instead of hanging or leaking.
    std::fs::remove_file(&socket).unwrap();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    let mut rest = Vec::new();
    client.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn connections_beyond_the_cap_are_refused_and_slots_are_freed() {
    let dir = PathBuf::from("/tmp").join(format!(
        "mafwdc-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    std::fs::create_dir(&dir).unwrap();
    let socket = dir.join("p.sock");
    let upstream = UnixListener::bind(&socket).unwrap();
    // Holds each connection open until the client side closes.
    std::thread::spawn(move || {
        for stream in upstream.incoming() {
            let mut stream = stream.unwrap();
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = stream.read_to_end(&mut sink);
            });
        }
    });
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let socket_for_bridge = socket.clone();
    std::thread::spawn(move || serve_capped(&listener, &socket_for_bridge, 1));

    let mut first = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    first.write_all(b"x").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut second = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    second
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut rest = Vec::new();
    // Refused: closed at once (EOF or reset), not held open until timeout.
    let result = second.read_to_end(&mut rest);
    assert!(rest.is_empty());
    assert!(
        result.is_ok() || result.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset,
        "the extra connection must be closed, not left hanging"
    );
    // Closing the first frees the slot for a new connection.
    drop(first);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut third = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    third.write_all(b"y").unwrap();
    third
        .set_read_timeout(Some(std::time::Duration::from_millis(400)))
        .unwrap();
    let mut buffer = [0u8; 1];
    let error = third.read(&mut buffer).unwrap_err();
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "accepted connection stays open: {error:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
