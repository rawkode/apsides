use std::{
    io::Write,
    net::{Shutdown, TcpListener},
    thread,
    time::{Duration, Instant},
};

use oxideloop_renderer_protocol::{
    ErrorCategoryV0, FrameV0, PhaseV0, RequestIdV0, RequestV0, ResponseStatusV0, ResponseV0,
    read_frame, write_frame,
};
use renderer_client::{RendererClient, RendererClientError};

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
