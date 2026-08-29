use oxideloop_renderer_protocol::{
    ErrorCategoryV0, FrameV0, MAX_DURATION_MILLIS, PhaseV0, ProtocolError, RequestIdV0, RequestV0,
    ResponseStatusV0, ResponseV0, read_frame, write_frame,
};
use std::io::Cursor;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
fn request() -> RequestV0 {
    RequestV0 {
        request_id: RequestIdV0 { bytes: [7; 16] },
        phase: PhaseV0::Render,
        bundle_digest: DIGEST.into(),
        module_digest: Some(DIGEST.into()),
        duration_millis: MAX_DURATION_MILLIS,
        payload: b"{}".to_vec(),
    }
}
fn encode(frame: &FrameV0) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame).unwrap();
    bytes
}

#[test]
fn exact_one_frame_round_trips_at_duration_boundary() {
    assert_eq!(
        read_frame(Cursor::new(encode(&FrameV0::Request(request())))).unwrap(),
        FrameV0::Request(request())
    );
    let mut over = request();
    over.duration_millis += 1;
    assert_eq!(
        write_frame(Vec::new(), &FrameV0::Request(over)).unwrap_err(),
        ProtocolError::Duration
    );
    let mut zero = request();
    zero.duration_millis = 0;
    assert_eq!(
        write_frame(Vec::new(), &FrameV0::Request(zero)).unwrap_err(),
        ProtocolError::Duration
    );
}

#[test]
fn non_ok_responses_cannot_exfiltrate_guest_bytes() {
    let response = ResponseV0 {
        request_id: RequestIdV0 { bytes: [7; 16] },
        phase: PhaseV0::Render,
        bundle_digest: DIGEST.into(),
        module_digest: Some(DIGEST.into()),
        status: ResponseStatusV0::Recoverable,
        error_category: Some(ErrorCategoryV0::GuestFailure),
        payload: b"guest-secret".to_vec(),
    };
    assert_eq!(
        write_frame(Vec::new(), &FrameV0::Response(response)).unwrap_err(),
        ProtocolError::Header
    );
}

#[test]
fn response_status_and_category_matrix_is_unambiguous() {
    let categories = [
        ErrorCategoryV0::InvalidInput,
        ErrorCategoryV0::GuestFailure,
        ErrorCategoryV0::BundleUnavailable,
        ErrorCategoryV0::DeadlineExceeded,
        ErrorCategoryV0::HeapLimit,
        ErrorCategoryV0::MemoryKilled,
        ErrorCategoryV0::ProcessLost,
        ErrorCategoryV0::IntegrityFailure,
        ErrorCategoryV0::Internal,
        ErrorCategoryV0::ProtocolViolation,
    ];
    for status in [
        ResponseStatusV0::Recoverable,
        ResponseStatusV0::Fatal,
        ResponseStatusV0::Protocol,
    ] {
        for category in categories {
            let response = ResponseV0 {
                request_id: RequestIdV0 { bytes: [7; 16] },
                phase: PhaseV0::Render,
                bundle_digest: DIGEST.into(),
                module_digest: Some(DIGEST.into()),
                status,
                error_category: Some(category),
                payload: vec![],
            };
            let expected = matches!(
                (status, category),
                (
                    ResponseStatusV0::Recoverable,
                    ErrorCategoryV0::InvalidInput
                        | ErrorCategoryV0::GuestFailure
                        | ErrorCategoryV0::BundleUnavailable
                ) | (
                    ResponseStatusV0::Fatal,
                    ErrorCategoryV0::DeadlineExceeded
                        | ErrorCategoryV0::HeapLimit
                        | ErrorCategoryV0::MemoryKilled
                        | ErrorCategoryV0::ProcessLost
                        | ErrorCategoryV0::IntegrityFailure
                        | ErrorCategoryV0::Internal
                ) | (
                    ResponseStatusV0::Protocol,
                    ErrorCategoryV0::ProtocolViolation
                )
            );
            assert_eq!(
                response.validate().is_ok(),
                expected,
                "{status:?}/{category:?}"
            );
        }
    }
}

#[test]
fn absent_module_flag_requires_zero_digest_bytes() {
    let mut descriptor = request();
    descriptor.phase = PhaseV0::Descriptor;
    descriptor.module_digest = None;
    let mut bytes = encode(&FrameV0::Request(descriptor));
    bytes[60] = 1;
    assert_eq!(
        read_frame(Cursor::new(bytes)).unwrap_err(),
        ProtocolError::Digest
    );
}

#[test]
fn parser_rejects_truncated_trailing_phase_abi_and_digest() {
    let bytes = encode(&FrameV0::Request(request()));
    assert_eq!(
        read_frame(&bytes[..12]).unwrap_err(),
        ProtocolError::Truncated
    );
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        read_frame(Cursor::new(trailing)).unwrap_err(),
        ProtocolError::Trailing
    );
    let mut phase = bytes.clone();
    phase[9] = 99;
    assert_eq!(
        read_frame(Cursor::new(phase)).unwrap_err(),
        ProtocolError::Phase
    );
    let mut abi = bytes.clone();
    abi[7] = 1;
    assert_eq!(
        read_frame(Cursor::new(abi)).unwrap_err(),
        ProtocolError::Abi
    );
    let mut missing_module = request();
    missing_module.module_digest = None;
    assert_eq!(
        write_frame(Vec::new(), &FrameV0::Request(missing_module)).unwrap_err(),
        ProtocolError::Digest
    );
}

#[test]
fn response_identity_must_match_the_request() {
    let request = request();
    let response = ResponseV0 {
        request_id: RequestIdV0 { bytes: [9; 16] },
        phase: PhaseV0::Render,
        bundle_digest: DIGEST.into(),
        module_digest: Some(DIGEST.into()),
        status: ResponseStatusV0::Recoverable,
        error_category: Some(ErrorCategoryV0::GuestFailure),
        payload: vec![],
    };
    assert_eq!(
        response.matches(&request),
        Err(ProtocolError::ResponseMismatch)
    );
}
