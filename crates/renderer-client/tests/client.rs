use std::{
    io::Write,
    net::{Shutdown, TcpListener},
    thread,
    time::{Duration, Instant},
};

use renderer_client::{RendererClient, RendererClientError};
use renderer_protocol::{
    ErrorCategoryV0, FrameV0, PhaseV0, RequestIdV0, RequestV0, ResponseStatusV0, ResponseV0,
    read_frame, write_frame,
};
#[cfg(unix)]
use std::os::unix::net::UnixListener;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn request() -> RequestV0 {
    RequestV0 {
        request_id: RequestIdV0 { bytes: [3; 16] },
        phase: PhaseV0::Render,
        bundle_digest: DIGEST.into(),
        module_digest: Some(DIGEST.into()),
        duration_millis: 500,
        payload: b"{}".to_vec(),
    }
}

fn server(reply: impl FnOnce(RequestV0) -> Vec<u8> + Send + 'static) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let FrameV0::Request(request) = read_frame(&mut stream).unwrap() else {
            panic!("client must send a request");
        };
        stream.write_all(&reply(request)).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        drop(stream);
    });
    address
}

#[test]
fn client_accepts_only_an_exactly_matching_response() {
    let address = server(|request| {
        let response = ResponseV0 {
            request_id: request.request_id,
            phase: request.phase,
            bundle_digest: request.bundle_digest,
            module_digest: request.module_digest,
            status: ResponseStatusV0::Recoverable,
            error_category: Some(ErrorCategoryV0::GuestFailure),
            payload: vec![],
        };
        let mut bytes = vec![];
        write_frame(&mut bytes, &FrameV0::Response(response)).unwrap();
        bytes
    });
    let mut client = RendererClient::new(address).unwrap();
    let result = client.call(&request());
    assert!(
        matches!(
            result,
            Ok(ResponseV0 {
                status: ResponseStatusV0::Recoverable,
                ..
            }),
        ),
        "{result:?}"
    );
}

#[test]
fn client_maps_response_identity_mismatch_and_disconnect_to_renderer_loss() {
    let address = server(|mut request| {
        request.request_id.bytes[0] = 9;
        let response = ResponseV0 {
            request_id: request.request_id,
            phase: request.phase,
            bundle_digest: request.bundle_digest,
            module_digest: request.module_digest,
            status: ResponseStatusV0::Recoverable,
            error_category: Some(ErrorCategoryV0::GuestFailure),
            payload: vec![],
        };
        let mut bytes = vec![];
        write_frame(&mut bytes, &FrameV0::Response(response)).unwrap();
        bytes
    });
    let mut client = RendererClient::new(address).unwrap();
    assert!(matches!(
        client.call(&request()),
        Err(RendererClientError::RendererLost)
    ));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        drop(stream);
    });
    let mut client = RendererClient::new(address).unwrap();
    assert!(matches!(
        client.call(&request()),
        Err(RendererClientError::RendererLost)
    ));
}

#[test]
fn client_rejects_terminal_response_statuses_as_renderer_loss() {
    let address = server(|request| {
        let response = ResponseV0 {
            request_id: request.request_id,
            phase: request.phase,
            bundle_digest: request.bundle_digest,
            module_digest: request.module_digest,
            status: ResponseStatusV0::Fatal,
            error_category: Some(ErrorCategoryV0::ProcessLost),
            payload: vec![],
        };
        let mut bytes = vec![];
        write_frame(&mut bytes, &FrameV0::Response(response)).unwrap();
        bytes
    });
    let mut client = RendererClient::new(address).unwrap();
    assert!(matches!(
        client.call(&request()),
        Err(RendererClientError::RendererLost)
    ));
}

#[test]
fn client_rejects_every_non_explicit_loopback_address() {
    for address in ["0.0.0.0:9000", "127.0.0.2:9000", "[::1]:9000"] {
        assert!(matches!(
            RendererClient::new(address.parse().unwrap()),
            Err(RendererClientError::NonLoopbackAddress)
        ));
    }
}

#[test]
fn slow_drip_response_cannot_extend_the_absolute_client_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let FrameV0::Request(request) = read_frame(&mut stream).unwrap() else {
            panic!("client must send a request");
        };
        let response = ResponseV0 {
            request_id: request.request_id,
            phase: request.phase,
            bundle_digest: request.bundle_digest,
            module_digest: request.module_digest,
            status: ResponseStatusV0::Recoverable,
            error_category: Some(ErrorCategoryV0::GuestFailure),
            payload: vec![],
        };
        let mut bytes = vec![];
        write_frame(&mut bytes, &FrameV0::Response(response)).unwrap();
        for byte in bytes {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut short = request();
    short.duration_millis = 60;
    let mut client = RendererClient::new(address).unwrap();
    let started = Instant::now();
    assert!(matches!(
        client.call(&short),
        Err(RendererClientError::RendererLost)
    ));
    assert!(started.elapsed() < Duration::from_millis(300));
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn unix_roundtrip_response_mismatch_and_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("renderer.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let FrameV0::Request(r) = read_frame(&mut s).unwrap() else {
            panic!()
        };
        let response = ResponseV0 {
            request_id: r.request_id,
            phase: r.phase,
            bundle_digest: r.bundle_digest,
            module_digest: r.module_digest,
            status: ResponseStatusV0::Recoverable,
            error_category: Some(ErrorCategoryV0::GuestFailure),
            payload: vec![],
        };
        write_frame(&mut s, &FrameV0::Response(response)).unwrap();
    });
    let mut client = RendererClient::new_unix(&path).unwrap();
    assert!(client.call(&request()).is_ok());
    server.join().unwrap();
    std::fs::remove_file(&path).unwrap();

    let mismatch = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = mismatch.accept().unwrap();
        let FrameV0::Request(mut request) = read_frame(&mut stream).unwrap() else {
            panic!()
        };
        request.request_id.bytes[0] ^= 1;
        write_frame(
            &mut stream,
            &FrameV0::Response(ResponseV0 {
                request_id: request.request_id,
                phase: request.phase,
                bundle_digest: request.bundle_digest,
                module_digest: request.module_digest,
                status: ResponseStatusV0::Recoverable,
                error_category: Some(ErrorCategoryV0::GuestFailure),
                payload: vec![],
            }),
        )
        .unwrap();
    });
    let mut client = RendererClient::new_unix(&path).unwrap();
    assert!(matches!(
        client.call(&request()),
        Err(RendererClientError::RendererLost)
    ));
    server.join().unwrap();
    std::fs::remove_file(&path).unwrap();

    let disconnect = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || drop(disconnect.accept().unwrap().0));
    let mut client = RendererClient::new_unix(&path).unwrap();
    assert!(matches!(
        client.call(&request()),
        Err(RendererClientError::RendererLost)
    ));
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn unix_paths_and_pre_call_replacements_are_rejected() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    assert!(matches!(
        RendererClient::new_unix("relative.sock"),
        Err(RendererClientError::InvalidUnixPath)
    ));
    assert!(matches!(
        RendererClient::new_unix(dir.path()),
        Err(RendererClientError::InvalidUnixPath)
    ));
    assert!(matches!(
        RendererClient::new_unix(std::path::PathBuf::from(std::ffi::OsString::from_vec(
            vec![b'/', 0]
        ))),
        Err(RendererClientError::InvalidUnixPath)
    ));
    assert!(matches!(
        RendererClient::new_unix(format!("/{}", "a".repeat(100))),
        Err(RendererClientError::InvalidUnixPath)
    ));
    for kind in ["regular", "symlink", "directory"] {
        let path = canonical_dir.join(format!("{kind}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = RendererClient::new_unix(&path).unwrap();
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        match kind {
            "regular" => std::fs::write(&path, b"not a socket").unwrap(),
            "symlink" => std::os::unix::fs::symlink(&canonical_dir, &path).unwrap(),
            "directory" => std::fs::create_dir(&path).unwrap(),
            _ => unreachable!(),
        }
        assert!(matches!(
            client.call(&request()),
            Err(RendererClientError::RendererLost)
        ));
        if kind == "directory" {
            std::fs::remove_dir(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
    let trusted_parent = canonical_dir.join("trusted");
    let aliased_parent = canonical_dir.join("aliased");
    std::fs::create_dir(&trusted_parent).unwrap();
    std::os::unix::fs::symlink(&trusted_parent, &aliased_parent).unwrap();
    let socket = trusted_parent.join("renderer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut client = RendererClient::new_unix(aliased_parent.join("renderer.sock")).unwrap();
    assert!(matches!(
        client.call(&request()),
        Err(RendererClientError::RendererLost)
    ));
    drop(listener);
}

#[cfg(unix)]
#[test]
fn unix_slow_drip_and_unavailable_connect_are_deadline_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let canonical_dir = dir.path().canonicalize().unwrap();
    let path = canonical_dir.join("renderer.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let FrameV0::Request(request) = read_frame(&mut stream).unwrap() else {
            panic!()
        };
        let mut bytes = vec![];
        write_frame(
            &mut bytes,
            &FrameV0::Response(ResponseV0 {
                request_id: request.request_id,
                phase: request.phase,
                bundle_digest: request.bundle_digest,
                module_digest: request.module_digest,
                status: ResponseStatusV0::Recoverable,
                error_category: Some(ErrorCategoryV0::GuestFailure),
                payload: vec![],
            }),
        )
        .unwrap();
        for byte in bytes {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let mut short = request();
    short.duration_millis = 60;
    let started = Instant::now();
    assert!(matches!(
        RendererClient::new_unix(&path).unwrap().call(&short),
        Err(RendererClientError::RendererLost)
    ));
    assert!(started.elapsed() < Duration::from_millis(300));
    server.join().unwrap();
    let missing = canonical_dir.join("missing.sock");
    let started = Instant::now();
    assert!(matches!(
        RendererClient::new_unix(missing).unwrap().call(&request()),
        Err(RendererClientError::RendererLost)
    ));
    assert!(started.elapsed() < Duration::from_millis(100));
}
