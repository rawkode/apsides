#![forbid(unsafe_code)]

pub mod generate;

use std::fmt::Write as _;
use std::io::{self, Read, Write};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

pub const MAGIC: [u8; 4] = *b"OXLP";
pub const PROTOCOL_VERSION_V0: u16 = 0;
pub const RENDERER_ABI_VERSION_V0: u16 = 0;
pub const HEADER_LENGTH: usize = 104;
pub const MAX_DURATION_MILLIS: u16 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum PhaseV0 {
    Descriptor,
    Render,
    Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum ResponseStatusV0 {
    Ok,
    Recoverable,
    Fatal,
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCategoryV0 {
    InvalidInput,
    GuestFailure,
    BundleUnavailable,
    DeadlineExceeded,
    HeapLimit,
    MemoryKilled,
    ProcessLost,
    IntegrityFailure,
    Internal,
    ProtocolViolation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct RequestIdV0 {
    #[ts(type = "number[]")]
    pub bytes: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestV0 {
    pub request_id: RequestIdV0,
    pub phase: PhaseV0,
    pub bundle_digest: String,
    pub module_digest: Option<String>,
    #[ts(type = "number")]
    pub duration_millis: u16,
    #[ts(type = "number[]")]
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseV0 {
    pub request_id: RequestIdV0,
    pub phase: PhaseV0,
    pub bundle_digest: String,
    pub module_digest: Option<String>,
    pub status: ResponseStatusV0,
    pub error_category: Option<ErrorCategoryV0>,
    #[ts(type = "number[]")]
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("renderer protocol header is invalid")]
    Header,
    #[error("renderer protocol version is unsupported")]
    Version,
    #[error("renderer ABI version is unsupported")]
    Abi,
    #[error("renderer protocol frame is truncated")]
    Truncated,
    #[error("renderer protocol frame has trailing bytes")]
    Trailing,
    #[error("renderer protocol phase is invalid")]
    Phase,
    #[error("renderer protocol digest is invalid")]
    Digest,
    #[error("renderer protocol duration is invalid")]
    Duration,
    #[error("renderer protocol payload exceeds its phase limit")]
    PayloadLimit,
    #[error("renderer protocol response does not match request")]
    ResponseMismatch,
    #[error("renderer protocol I/O failed")]
    Io,
}

impl From<io::Error> for ProtocolError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameV0 {
    Request(RequestV0),
    Response(ResponseV0),
}

impl RequestV0 {
    /// Validate the request's fixed protocol bounds and identity fields.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] for invalid digests, duration, or payload size.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_digests(
            self.phase,
            &self.bundle_digest,
            self.module_digest.as_deref(),
        )?;
        validate_duration(self.duration_millis)?;
        validate_payload(self.phase, self.payload.len())
    }
}

impl ResponseV0 {
    /// Validate the response's fixed protocol bounds and opaque error shape.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError`] for invalid digests, payload size, or status.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_digests(
            self.phase,
            &self.bundle_digest,
            self.module_digest.as_deref(),
        )?;
        validate_payload(self.phase, self.payload.len())?;
        match (self.status, self.error_category, self.payload.is_empty()) {
            (ResponseStatusV0::Ok, None, _)
            | (
                ResponseStatusV0::Recoverable,
                Some(
                    ErrorCategoryV0::InvalidInput
                    | ErrorCategoryV0::GuestFailure
                    | ErrorCategoryV0::BundleUnavailable,
                ),
                true,
            )
            | (
                ResponseStatusV0::Fatal,
                Some(
                    ErrorCategoryV0::DeadlineExceeded
                    | ErrorCategoryV0::HeapLimit
                    | ErrorCategoryV0::MemoryKilled
                    | ErrorCategoryV0::ProcessLost
                    | ErrorCategoryV0::IntegrityFailure
                    | ErrorCategoryV0::Internal,
                ),
                true,
            )
            | (ResponseStatusV0::Protocol, Some(ErrorCategoryV0::ProtocolViolation), true) => {
                Ok(())
            }
            _ => Err(ProtocolError::Header),
        }
    }

    /// Require this response to bind to the supplied request.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::ResponseMismatch`] for a mismatched ID, phase,
    /// bundle digest, or module digest.
    pub fn matches(&self, request: &RequestV0) -> Result<(), ProtocolError> {
        self.validate()?;
        request.validate()?;
        if self.request_id != request.request_id
            || self.phase != request.phase
            || self.bundle_digest != request.bundle_digest
            || self.module_digest != request.module_digest
        {
            return Err(ProtocolError::ResponseMismatch);
        }
        Ok(())
    }
}

/// Write a single v0 frame. Connections are intentionally one-frame only.
///
/// # Errors
///
/// Returns [`ProtocolError`] for invalid frames or failed output.
pub fn write_frame(mut writer: impl Write, frame: &FrameV0) -> Result<(), ProtocolError> {
    let (kind, request_id, phase, bundle, module, duration, status, category, payload) = match frame
    {
        FrameV0::Request(request) => {
            request.validate()?;
            (
                0,
                request.request_id,
                request.phase,
                &request.bundle_digest,
                request.module_digest.as_deref(),
                request.duration_millis,
                None,
                None,
                &request.payload,
            )
        }
        FrameV0::Response(response) => {
            response.validate()?;
            (
                1,
                response.request_id,
                response.phase,
                &response.bundle_digest,
                response.module_digest.as_deref(),
                0,
                Some(response.status),
                response.error_category,
                &response.payload,
            )
        }
    };
    let payload_length = u32::try_from(payload.len()).map_err(|_| ProtocolError::PayloadLimit)?;
    let mut header = [0_u8; HEADER_LENGTH];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&PROTOCOL_VERSION_V0.to_be_bytes());
    header[6..8].copy_from_slice(&RENDERER_ABI_VERSION_V0.to_be_bytes());
    header[8] = kind;
    header[9] = encode_phase(phase);
    header[10] = status.map_or(0, encode_status);
    header[11] = category.map_or(0, |value| encode_category(value) + 1);
    header[12..28].copy_from_slice(&request_id.bytes);
    header[28..60].copy_from_slice(&decode_digest(bundle)?);
    if let Some(module) = module {
        header[60..92].copy_from_slice(&decode_digest(module)?);
        header[92] = 1;
    }
    header[93..95].copy_from_slice(&duration.to_be_bytes());
    header[96..100].copy_from_slice(&payload_length.to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    Ok(())
}

/// Parse a header before allocating a payload, read exactly one frame, then
/// require EOF. This is intended for one request per loopback connection.
///
/// # Errors
///
/// Returns [`ProtocolError`] for malformed, oversized, truncated, or trailing
/// frames, or failed input.
pub fn read_frame(mut reader: impl Read) -> Result<FrameV0, ProtocolError> {
    let mut header = [0_u8; HEADER_LENGTH];
    read_exact_protocol(&mut reader, &mut header)?;
    if header[..4] != MAGIC {
        return Err(ProtocolError::Header);
    }
    if u16::from_be_bytes([header[4], header[5]]) != PROTOCOL_VERSION_V0 {
        return Err(ProtocolError::Version);
    }
    if u16::from_be_bytes([header[6], header[7]]) != RENDERER_ABI_VERSION_V0 {
        return Err(ProtocolError::Abi);
    }
    if header[95] != 0 || header[100..].iter().any(|byte| *byte != 0) {
        return Err(ProtocolError::Header);
    }
    let kind = header[8];
    let phase = decode_phase(header[9])?;
    let payload_length = u32::from_be_bytes([header[96], header[97], header[98], header[99]]);
    validate_payload(
        phase,
        usize::try_from(payload_length).map_err(|_| ProtocolError::PayloadLimit)?,
    )?;
    let duration = u16::from_be_bytes([header[93], header[94]]);
    if kind == 0 {
        validate_duration(duration)?;
    } else if duration != 0 {
        return Err(ProtocolError::Duration);
    }
    let bundle = encode_digest(
        header[28..60]
            .try_into()
            .map_err(|_| ProtocolError::Header)?,
    );
    let module = if header[92] == 0 {
        if header[60..92].iter().any(|byte| *byte != 0) {
            return Err(ProtocolError::Digest);
        }
        None
    } else if header[92] == 1 {
        Some(encode_digest(
            header[60..92]
                .try_into()
                .map_err(|_| ProtocolError::Header)?,
        ))
    } else {
        return Err(ProtocolError::Header);
    };
    validate_digests(phase, &bundle, module.as_deref())?;
    let mut payload =
        vec![0_u8; usize::try_from(payload_length).map_err(|_| ProtocolError::PayloadLimit)?];
    read_exact_protocol(&mut reader, &mut payload)?;
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(ProtocolError::Trailing);
    }
    let request_id = RequestIdV0 {
        bytes: header[12..28]
            .try_into()
            .map_err(|_| ProtocolError::Header)?,
    };
    match kind {
        0 if header[10] == 0 && header[11] == 0 => Ok(FrameV0::Request(RequestV0 {
            request_id,
            phase,
            bundle_digest: bundle,
            module_digest: module,
            duration_millis: duration,
            payload,
        })),
        1 => {
            let status = decode_status(header[10])?;
            let category = if header[11] == 0 {
                None
            } else {
                Some(decode_category(header[11] - 1)?)
            };
            let response = ResponseV0 {
                request_id,
                phase,
                bundle_digest: bundle,
                module_digest: module,
                status,
                error_category: category,
                payload,
            };
            response.validate()?;
            Ok(FrameV0::Response(response))
        }
        _ => Err(ProtocolError::Header),
    }
}

fn read_exact_protocol(reader: &mut impl Read, bytes: &mut [u8]) -> Result<(), ProtocolError> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::Truncated
        } else {
            ProtocolError::Io
        }
    })
}
fn validate_duration(value: u16) -> Result<(), ProtocolError> {
    if (1..=MAX_DURATION_MILLIS).contains(&value) {
        Ok(())
    } else {
        Err(ProtocolError::Duration)
    }
}

/// The socket owner must impose this deadline externally and half-close its
/// write side after a request. Frame parsing then requires the peer's EOF,
/// ensuring exactly one bounded request per loopback connection.
pub const SOCKET_DEADLINE_AND_HALF_CLOSE_REQUIREMENT: &str =
    "apply the request duration as a socket deadline and half-close after one frame";
fn validate_payload(phase: PhaseV0, length: usize) -> Result<(), ProtocolError> {
    if length <= phase_cap(phase) {
        Ok(())
    } else {
        Err(ProtocolError::PayloadLimit)
    }
}
#[must_use]
pub fn phase_cap(phase: PhaseV0) -> usize {
    match phase {
        PhaseV0::Descriptor => 256 * 1024,
        PhaseV0::Render => 1024 * 1024,
        PhaseV0::Action => 512 * 1024,
    }
}
fn validate_digests(
    phase: PhaseV0,
    bundle: &str,
    module: Option<&str>,
) -> Result<(), ProtocolError> {
    if !is_digest(bundle)
        || matches!(phase, PhaseV0::Render | PhaseV0::Action) != module.is_some()
        || module.is_some_and(|value| !is_digest(value))
    {
        Err(ProtocolError::Digest)
    } else {
        Ok(())
    }
}
fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn decode_digest(value: &str) -> Result<[u8; 32], ProtocolError> {
    if !is_digest(value) {
        return Err(ProtocolError::Digest);
    }
    let mut result = [0; 32];
    for (index, output) in result.iter_mut().enumerate() {
        *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| ProtocolError::Digest)?;
    }
    Ok(result)
}
fn encode_digest(value: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}
fn encode_phase(value: PhaseV0) -> u8 {
    match value {
        PhaseV0::Descriptor => 0,
        PhaseV0::Render => 1,
        PhaseV0::Action => 2,
    }
}
fn decode_phase(value: u8) -> Result<PhaseV0, ProtocolError> {
    match value {
        0 => Ok(PhaseV0::Descriptor),
        1 => Ok(PhaseV0::Render),
        2 => Ok(PhaseV0::Action),
        _ => Err(ProtocolError::Phase),
    }
}
fn encode_status(value: ResponseStatusV0) -> u8 {
    match value {
        ResponseStatusV0::Ok => 0,
        ResponseStatusV0::Recoverable => 1,
        ResponseStatusV0::Fatal => 2,
        ResponseStatusV0::Protocol => 3,
    }
}
fn decode_status(value: u8) -> Result<ResponseStatusV0, ProtocolError> {
    match value {
        0 => Ok(ResponseStatusV0::Ok),
        1 => Ok(ResponseStatusV0::Recoverable),
        2 => Ok(ResponseStatusV0::Fatal),
        3 => Ok(ResponseStatusV0::Protocol),
        _ => Err(ProtocolError::Header),
    }
}
fn encode_category(value: ErrorCategoryV0) -> u8 {
    match value {
        ErrorCategoryV0::InvalidInput => 0,
        ErrorCategoryV0::GuestFailure => 1,
        ErrorCategoryV0::BundleUnavailable => 2,
        ErrorCategoryV0::DeadlineExceeded => 3,
        ErrorCategoryV0::HeapLimit => 4,
        ErrorCategoryV0::MemoryKilled => 5,
        ErrorCategoryV0::ProcessLost => 6,
        ErrorCategoryV0::IntegrityFailure => 7,
        ErrorCategoryV0::Internal => 8,
        ErrorCategoryV0::ProtocolViolation => 9,
    }
}
fn decode_category(value: u8) -> Result<ErrorCategoryV0, ProtocolError> {
    match value {
        0 => Ok(ErrorCategoryV0::InvalidInput),
        1 => Ok(ErrorCategoryV0::GuestFailure),
        2 => Ok(ErrorCategoryV0::BundleUnavailable),
        3 => Ok(ErrorCategoryV0::DeadlineExceeded),
        4 => Ok(ErrorCategoryV0::HeapLimit),
        5 => Ok(ErrorCategoryV0::MemoryKilled),
        6 => Ok(ErrorCategoryV0::ProcessLost),
        7 => Ok(ErrorCategoryV0::IntegrityFailure),
        8 => Ok(ErrorCategoryV0::Internal),
        9 => Ok(ErrorCategoryV0::ProtocolViolation),
        _ => Err(ProtocolError::Header),
    }
}
