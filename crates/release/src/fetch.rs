//! OCI Distribution pull over an in-process HTTPS client. Transport success
//! never grants release trust.
use crate::{Result, manifest, reject};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use ureq::{
    Agent, Body,
    http::{Response, StatusCode, Uri, header},
    tls::{Certificate, PemItem, RootCerts, TlsConfig},
};

/// The whole retrieval, including every layer download.
const DEADLINE: Duration = Duration::from_secs(300);
const MAX_MANIFEST: u64 = 4 * 1024 * 1024;
const MAX_TOKEN_RESPONSE: u64 = 64 * 1024;
const MAX_TOKEN: usize = 16 * 1024;
const MAX_CA_FILE: u64 = 1024 * 1024;
const MAX_LAYERS: usize = 256;
/// Total declared size of the materialised layers.
const MAX_RELEASE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_TITLE: usize = 1024;
const MAX_REDIRECTS: usize = 5;
const MANIFEST_TYPES: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
/// Only layers naming a file are materialised, as ORAS pull does.
const TITLE: &str = "org.opencontainers.image.title";
const FAILED: &str = "OCI release retrieval failed";
const TIMED_OUT: &str = "OCI release retrieval timed out";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseReference {
    pub oci: String,
    /// Digest of release.json, distinct from the enclosing OCI manifest digest.
    pub release_digest: String,
}

impl ReleaseReference {
    /// # Errors
    /// Rejects tags, URLs, credentials and malformed SHA-256 references.
    pub fn validate(&self) -> Result<()> {
        let Some((repository, digest)) = self.oci.split_once('@') else {
            return reject("release reference requires an immutable OCI digest");
        };
        if !manifest::is_digest(digest)
            || !manifest::is_digest(&self.release_digest)
            || repository.is_empty()
            || !repository.contains('/')
            || repository.starts_with(['/', '-', '.'])
            || repository.ends_with('/')
            || repository.contains("//")
            || repository.contains("..")
            || !repository
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-_/:".contains(&c))
            || repository
                .rsplit('/')
                .next()
                .is_some_and(|last| last.contains(':'))
        {
            return reject("invalid immutable OCI release reference");
        }
        Ok(())
    }
}

/// Trusted CI registry settings. Never sourced from an artifact.
#[derive(Clone, Debug, Default)]
pub struct RegistryAccess {
    /// Optional CA certificate file for a private registry, trusted in
    /// addition to the Mozilla roots.
    pub ca_file: Option<PathBuf>,
    /// Sent to a Bearer token realm or a Basic challenge; anonymous otherwise.
    pub credentials: Option<RegistryCredentials>,
}

#[derive(Clone)]
pub struct RegistryCredentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for RegistryCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Downloaded content bound to a digest, but not yet approved by any trust policy.
/// Its path may be passed to the normal verifier; it cannot supply staging authority.
pub struct FetchedRelease {
    reference: ReleaseReference,
    directory: tempfile::TempDir,
}

impl FetchedRelease {
    #[must_use]
    pub const fn reference(&self) -> &ReleaseReference {
        &self.reference
    }
    #[must_use]
    pub fn release_digest(&self) -> &str {
        &self.reference.release_digest
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

/// Fetch the exact referenced content before discovering protected cluster policy.
/// # Errors
/// Rejects invalid references, failed transport and a mismatched release digest.
/// Success establishes content identity only, never signature or deployment trust.
pub async fn fetch_release(
    reference: &ReleaseReference,
    access: &RegistryAccess,
) -> Result<FetchedRelease> {
    reference.validate()?;
    let deadline = Instant::now() + DEADLINE;
    let (pull_reference, pull_access) = (reference.clone(), access.clone());
    // The task owns the directory, so a pull abandoned at the deadline
    // removes what it wrote when it stops.
    let task = tokio::task::spawn_blocking(move || {
        let directory = tempfile::tempdir()?;
        pull(&pull_reference, &pull_access, directory.path(), deadline)?;
        Ok::<_, crate::Error>(directory)
    });
    let directory = match tokio::time::timeout(DEADLINE, task).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return reject(FAILED),
        Err(_) => return reject(TIMED_OUT),
    };
    let bytes = manifest::bounded_read(
        &manifest::contained(directory.path(), "release.json")?,
        manifest::MAX_DOCUMENT,
    )?;
    if manifest::raw_digest(&bytes) != reference.release_digest {
        return reject("resolved release differs from protected reference");
    }
    Ok(FetchedRelease {
        directory,
        reference: reference.clone(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageManifest {
    schema_version: u32,
    media_type: Option<String>,
    manifests: Option<serde_json::Value>,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

/// A Distribution token response; registries use either field name.
#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

fn pull(
    reference: &ReleaseReference,
    access: &RegistryAccess,
    destination: &Path,
    deadline: Instant,
) -> Result<()> {
    let (repository, digest) = reference
        .oci
        .split_once('@')
        .ok_or_else(|| crate::Error::Rejected("invalid immutable OCI release reference".into()))?;
    let (host, name) = repository
        .split_once('/')
        .ok_or_else(|| crate::Error::Rejected("invalid immutable OCI release reference".into()))?;
    // Docker Hub serves the Distribution API from a different host than its name.
    let (host, name) = if host == "docker.io" {
        let name = if name.contains('/') {
            name.to_owned()
        } else {
            format!("library/{name}")
        };
        ("registry-1.docker.io", name)
    } else {
        (host, name.to_owned())
    };
    let mut registry = Registry {
        agent: agent(access)?,
        origin: format!("https://{host}"),
        authority: normal_authority(host),
        repository: name,
        credentials: access.credentials.as_ref(),
        authorization: None,
        deadline,
    };
    let bytes = read_bounded(
        registry.get(
            &format!("/v2/{}/manifests/{digest}", registry.repository),
            Some(MANIFEST_TYPES),
        )?,
        MAX_MANIFEST,
    )?;
    if manifest::raw_digest(&bytes) != digest {
        return reject("OCI manifest differs from its reference digest");
    }
    let image: ImageManifest = manifest::json(&bytes, "OCI manifest")
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| crate::Error::Rejected("OCI manifest is malformed".into()))?;
    if image.manifests.is_some()
        || image
            .media_type
            .as_deref()
            .is_some_and(|media_type| INDEX_TYPES.contains(&media_type))
    {
        return reject("OCI release reference names an index, not an image manifest");
    }
    if image.schema_version != 2 {
        return reject("OCI manifest is malformed");
    }
    for (title, layer) in titled_layers(&image.layers)? {
        registry.download(layer, &destination.join(title))?;
    }
    Ok(())
}

/// The layers to materialise, each under a validated, unique relative path.
fn titled_layers(layers: &[Descriptor]) -> Result<Vec<(&str, &Descriptor)>> {
    if layers.len() > MAX_LAYERS {
        return reject("OCI manifest has too many layers");
    }
    let mut titles = BTreeSet::new();
    let mut total: u64 = 0;
    let mut selected = Vec::new();
    for layer in layers {
        let Some(title) = layer.annotations.get(TITLE) else {
            continue;
        };
        if !safe_title(title) {
            return reject("OCI layer title is not a safe relative path");
        }
        if !manifest::is_digest(&layer.digest) {
            return reject("OCI layer digest is not SHA-256");
        }
        if !titles.insert(title.as_str()) {
            return reject("OCI manifest repeats a layer title");
        }
        total = total
            .checked_add(layer.size)
            .filter(|total| *total <= MAX_RELEASE_BYTES)
            .ok_or_else(|| crate::Error::Rejected("OCI release exceeds the size limit".into()))?;
        selected.push((title.as_str(), layer));
    }
    // A title that is also another title's directory cannot be materialised.
    for title in &titles {
        if title
            .match_indices('/')
            .any(|(end, _)| titles.contains(&title[..end]))
        {
            return reject("OCI layer title conflicts with another title's directory");
        }
    }
    Ok(selected)
}

fn safe_title(title: &str) -> bool {
    !title.is_empty()
        && title.len() <= MAX_TITLE
        && !title.starts_with('/')
        && !title.bytes().any(|c| c == b'\\' || c.is_ascii_control())
        && title
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

struct Registry<'a> {
    agent: Agent,
    origin: String,
    authority: String,
    repository: String,
    credentials: Option<&'a RegistryCredentials>,
    authorization: Option<String>,
    deadline: Instant,
}

impl Registry<'_> {
    /// GET a registry path, authenticating on a challenge and following only
    /// bounded HTTPS redirects. The registry `Authorization` header is never
    /// sent to another host.
    fn get(&mut self, path: &str, accept: Option<&str>) -> Result<Body> {
        let mut location = format!("{}{path}", self.origin);
        let mut response = self.send(&location, accept, self.authorization.as_deref())?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.authorization = Some(self.authenticate(&response)?);
            response = self.send(&location, accept, self.authorization.as_deref())?;
        }
        for _ in 0..MAX_REDIRECTS {
            if !response.status().is_redirection() {
                break;
            }
            location = redirect_target(&location, &response)?;
            let authorization = if authority(&location)? == self.authority {
                self.authorization.as_deref()
            } else {
                None
            };
            response = self.send(&location, accept, authorization)?;
        }
        if response.status() != StatusCode::OK {
            return reject(FAILED);
        }
        Ok(response.into_body())
    }

    fn send(
        &self,
        url: &str,
        accept: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<Response<Body>> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| crate::Error::Rejected(TIMED_OUT.into()))?;
        let mut request = self.agent.get(url);
        if let Some(accept) = accept {
            request = request.header(header::ACCEPT, accept);
        }
        if let Some(authorization) = authorization {
            request = request.header(header::AUTHORIZATION, authorization);
        }
        request
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
            .map_err(|error| match error {
                ureq::Error::Timeout(_) => crate::Error::Rejected(TIMED_OUT.into()),
                _ => crate::Error::Rejected(FAILED.into()),
            })
    }

    /// The `Authorization` value answering a registry challenge.
    fn authenticate(&self, response: &Response<Body>) -> Result<String> {
        let challenges: Vec<Challenge> = response
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(Challenge::parse)
            .collect();
        if let Some(bearer) = challenges
            .iter()
            .find(|challenge| challenge.scheme.eq_ignore_ascii_case("bearer"))
        {
            return Ok(format!("Bearer {}", self.token(bearer)?));
        }
        if let Some(credentials) = self.credentials
            && challenges
                .iter()
                .any(|challenge| challenge.scheme.eq_ignore_ascii_case("basic"))
        {
            return Ok(basic(credentials));
        }
        reject(FAILED)
    }

    /// A Distribution token from the challenge's HTTPS realm, requested with
    /// the configured credentials or anonymously.
    fn token(&self, challenge: &Challenge) -> Result<String> {
        let realm = challenge.params.get("realm").ok_or_else(failed)?;
        if !realm.starts_with("https://") {
            return reject("OCI token realm must use HTTPS");
        }
        authority(realm)?;
        let scope = challenge.params.get("scope").map_or_else(
            || format!("repository:{}:pull", self.repository),
            Clone::clone,
        );
        let query: Vec<String> = challenge
            .params
            .get("service")
            .map(|service| format!("service={}", query_component(service)))
            .into_iter()
            .chain([format!("scope={}", query_component(&scope))])
            .collect();
        let separator = if realm.contains('?') { '&' } else { '?' };
        let url = format!("{realm}{separator}{}", query.join("&"));
        let authorization = self.credentials.map(basic);
        let response = self.send(&url, Some("application/json"), authorization.as_deref())?;
        if response.status() != StatusCode::OK {
            return reject(FAILED);
        }
        let bytes = read_bounded(response.into_body(), MAX_TOKEN_RESPONSE)?;
        // A parse error can quote the response, so it never escapes.
        let parsed: TokenResponse = serde_json::from_slice(&bytes).map_err(|_| failed())?;
        parsed
            .token
            .or(parsed.access_token)
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= MAX_TOKEN
                    && token.bytes().all(|c| c.is_ascii_graphic())
            })
            .ok_or_else(failed)
    }

    /// Stream one blob into a new file, bounded by and verified against its
    /// descriptor.
    fn download(&mut self, layer: &Descriptor, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut body = self
            .get(
                &format!("/v2/{}/blobs/{}", self.repository, layer.digest),
                None,
            )?
            .into_reader();
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let count = body
                .read(&mut buffer)
                .map_err(|error| read_failure(&error))?;
            if count == 0 {
                break;
            }
            written += count as u64;
            if written > layer.size {
                return reject("OCI layer exceeds its declared size");
            }
            hasher.update(&buffer[..count]);
            file.write_all(&buffer[..count])?;
        }
        if written != layer.size {
            return reject("OCI layer size differs from its descriptor");
        }
        if format!("sha256:{:x}", hasher.finalize()) != layer.digest {
            return reject("OCI layer differs from its descriptor digest");
        }
        file.flush()?;
        Ok(())
    }
}

fn agent(access: &RegistryAccess) -> Result<Agent> {
    let mut roots: Vec<Certificate<'static>> = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|certificate| Certificate::from_der(certificate.as_ref()))
        .collect();
    if let Some(ca_file) = &access.ca_file {
        if !ca_file.is_absolute() || !ca_file.is_file() {
            return reject("registry CA must be a trusted absolute file path");
        }
        let pem = manifest::bounded_read(ca_file, MAX_CA_FILE)?;
        let mut added = 0;
        for item in ureq::tls::parse_pem(&pem) {
            match item {
                Ok(PemItem::Certificate(certificate)) => {
                    roots.push(certificate);
                    added += 1;
                }
                Ok(_) => {}
                Err(_) => return reject("registry CA file is not valid PEM"),
            }
        }
        if added == 0 {
            return reject("registry CA file contains no certificates");
        }
    }
    Ok(Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        // Redirects are followed by Registry::get, which checks each target.
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(30)))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::from(roots))
                .build(),
        )
        .build()
        .new_agent())
}

/// An absolute `https://` redirect target, or a path on the current origin.
fn redirect_target(current: &str, response: &Response<Body>) -> Result<String> {
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(failed)?;
    if location.starts_with("https://") {
        authority(location)?;
        return Ok(location.to_owned());
    }
    if location.starts_with('/') && !location.starts_with("//") {
        let uri: Uri = current.parse().map_err(|_| failed())?;
        let origin = uri.authority().ok_or_else(failed)?;
        return Ok(format!("https://{origin}{location}"));
    }
    reject("OCI registry redirected to a non-HTTPS location")
}

/// The lowercase `host:port` of an HTTPS URL.
fn authority(url: &str) -> Result<String> {
    let uri: Uri = url.parse().map_err(|_| failed())?;
    if uri.scheme_str() != Some("https") {
        return reject("OCI registry redirected to a non-HTTPS location");
    }
    let authority = uri.authority().ok_or_else(failed)?;
    if authority.as_str().contains('@') {
        return reject(FAILED);
    }
    Ok(normal_authority(authority.as_str()))
}

fn normal_authority(authority: &str) -> String {
    let authority = authority.to_ascii_lowercase();
    if authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()))
    {
        authority
    } else {
        format!("{authority}:443")
    }
}

fn basic(credentials: &RegistryCredentials) -> String {
    let pair = format!("{}:{}", credentials.username, credentials.password);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

fn query_component(value: &str) -> String {
    value
        .bytes()
        .map(|c| {
            if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) {
                char::from(c).to_string()
            } else {
                format!("%{c:02X}")
            }
        })
        .collect()
}

fn read_bounded(body: Body, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    body.into_reader()
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| read_failure(&error))?;
    if bytes.len() as u64 > max {
        return reject("OCI registry response exceeds its size limit");
    }
    Ok(bytes)
}

fn read_failure(error: &std::io::Error) -> crate::Error {
    if error.kind() == std::io::ErrorKind::TimedOut {
        crate::Error::Rejected(TIMED_OUT.into())
    } else {
        failed()
    }
}

fn failed() -> crate::Error {
    crate::Error::Rejected(FAILED.into())
}

/// One `WWW-Authenticate` challenge: a scheme and its `key=value` parameters.
struct Challenge {
    scheme: String,
    params: BTreeMap<String, String>,
}

impl Challenge {
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (scheme, mut rest) = value.split_once(' ').unwrap_or((value, ""));
        let mut params = BTreeMap::new();
        rest = rest.trim_start();
        while !rest.is_empty() {
            let (key, after) = rest.split_once('=')?;
            let after = after.trim_start();
            let (parameter, remainder) = if let Some(quoted) = after.strip_prefix('"') {
                let mut parameter = String::new();
                let mut characters = quoted.char_indices();
                let mut end = None;
                while let Some((index, character)) = characters.next() {
                    match character {
                        '\\' => parameter.push(characters.next()?.1),
                        '"' => {
                            end = Some(index + 1);
                            break;
                        }
                        character => parameter.push(character),
                    }
                }
                (parameter, &quoted[end?..])
            } else {
                let end = after.find(',').unwrap_or(after.len());
                (after[..end].trim().to_owned(), &after[end..])
            };
            params.insert(key.trim().to_ascii_lowercase(), parameter);
            let remainder = remainder.trim_start();
            rest = match remainder.strip_prefix(',') {
                Some(next) => next.trim_start(),
                None if remainder.is_empty() => remainder,
                None => return None,
            };
        }
        Some(Self {
            scheme: scheme.to_owned(),
            params,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use serde_json::{Value, json};
    use std::{
        io::{BufRead as _, BufReader},
        net::{TcpListener, TcpStream},
        sync::{Arc, Mutex},
    };

    type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;
    type Handler = dyn Fn(&Request, u16) -> Reply + Send + Sync;

    const TOKEN: &str = "t0k3n";

    #[derive(Clone, Debug)]
    struct Request {
        path: String,
        authorization: Option<String>,
    }

    struct Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    impl Reply {
        fn ok(body: Vec<u8>) -> Self {
            Self {
                status: 200,
                headers: vec![],
                body,
            }
        }
        fn status(status: u16) -> Self {
            Self {
                status,
                headers: vec![],
                body: vec![],
            }
        }
        fn redirect(location: String) -> Self {
            Self {
                status: 307,
                headers: vec![("Location", location)],
                body: vec![],
            }
        }
    }

    /// An HTTPS registry on localhost whose certificate chains to a CA that
    /// only `access()` trusts.
    struct Fake {
        port: u16,
        requests: Arc<Mutex<Vec<Request>>>,
        directory: tempfile::TempDir,
    }

    impl Fake {
        fn serve(handler: impl Fn(&Request, u16) -> Reply + Send + Sync + 'static) -> Result<Self> {
            let fake = || crate::Error::Rejected("test registry could not start".into());
            let mut ca_params =
                rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|_| fake())?;
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let ca = rcgen::CertifiedIssuer::self_signed(
                ca_params,
                rcgen::KeyPair::generate().map_err(|_| fake())?,
            )
            .map_err(|_| fake())?;
            let key = rcgen::KeyPair::generate().map_err(|_| fake())?;
            let certificate =
                rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                    .and_then(|params| params.signed_by(&key, &*ca))
                    .map_err(|_| fake())?;
            let config = Arc::new(
                rustls::ServerConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .and_then(|builder| {
                    builder.with_no_client_auth().with_single_cert(
                        vec![certificate.der().clone()],
                        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                    )
                })
                .map_err(|_| fake())?,
            );
            let directory = tempfile::tempdir()?;
            fs::write(directory.path().join("ca.pem"), ca.pem())?;
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let handler: Arc<Handler> = Arc::new(handler);
            let recorded = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (config, handler, recorded) = (
                        Arc::clone(&config),
                        Arc::clone(&handler),
                        Arc::clone(&recorded),
                    );
                    std::thread::spawn(move || {
                        let _ = respond(stream, config, &*handler, &recorded, port);
                    });
                }
            });
            Ok(Self {
                port,
                requests,
                directory,
            })
        }

        fn access(&self) -> RegistryAccess {
            RegistryAccess {
                ca_file: Some(self.directory.path().join("ca.pem")),
                credentials: None,
            }
        }

        fn reference(&self, manifest: &[u8], release: &[u8]) -> ReleaseReference {
            ReleaseReference {
                oci: format!(
                    "localhost:{}/org/release@{}",
                    self.port,
                    manifest::raw_digest(manifest)
                ),
                release_digest: manifest::raw_digest(release),
            }
        }

        fn requests(&self) -> Vec<Request> {
            self.requests
                .lock()
                .map(|requests| requests.clone())
                .unwrap_or_default()
        }
    }

    /// Serve one request per connection.
    fn respond(
        stream: TcpStream,
        config: Arc<rustls::ServerConfig>,
        handler: &Handler,
        recorded: &Mutex<Vec<Request>>,
        port: u16,
    ) -> std::io::Result<()> {
        let connection = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
        let mut stream = rustls::StreamOwned::new(connection, stream);
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
        let mut authorization = None;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
                break;
            }
            if let Some((name, value)) = line.trim_end().split_once(':')
                && name.eq_ignore_ascii_case("authorization")
            {
                authorization = Some(value.trim().to_owned());
            }
        }
        let request = Request {
            path,
            authorization,
        };
        if let Ok(mut requests) = recorded.lock() {
            requests.push(request.clone());
        }
        let reply = handler(&request, port);
        let mut head = format!(
            "HTTP/1.1 {} Fake\r\nContent-Length: {}\r\nConnection: close\r\n",
            reply.status,
            reply.body.len()
        );
        for (name, value) in reply.headers {
            head = head + name + ": " + &value + "\r\n";
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&reply.body)?;
        stream.conn.send_close_notify();
        stream.flush()
    }

    /// A Distribution registry for `org/release` with an optional token realm.
    #[derive(Clone)]
    struct Registry {
        manifest: Vec<u8>,
        blobs: BTreeMap<String, Vec<u8>>,
        auth: Auth,
    }

    #[derive(Clone)]
    enum Auth {
        None,
        /// A Bearer token, issued anonymously in the `token` field.
        AnonymousToken,
        /// A Bearer token, issued in the `access_token` field only to this
        /// `Authorization`.
        TokenFor(String),
    }

    impl Registry {
        fn new(manifest: Vec<u8>, blobs: &[&[u8]]) -> Self {
            Self {
                manifest,
                blobs: blobs
                    .iter()
                    .map(|blob| (manifest::raw_digest(blob), blob.to_vec()))
                    .collect(),
                auth: Auth::None,
            }
        }

        fn reply(&self, request: &Request, port: u16) -> Reply {
            // A CDN serves blobs without the registry's authorization.
            if let Some(digest) = request.path.strip_prefix("/cdn/") {
                return self
                    .blobs
                    .get(digest)
                    .map_or_else(|| Reply::status(404), |blob| Reply::ok(blob.clone()));
            }
            if !matches!(self.auth, Auth::None) {
                if request.path.starts_with("/token?") {
                    return match &self.auth {
                        Auth::TokenFor(expected)
                            if request.authorization.as_ref() == Some(expected) =>
                        {
                            Reply::ok(json!({"access_token": TOKEN}).to_string().into_bytes())
                        }
                        Auth::AnonymousToken => {
                            Reply::ok(json!({"token": TOKEN}).to_string().into_bytes())
                        }
                        _ => Reply::status(401),
                    };
                }
                if request.authorization.as_deref() != Some(&format!("Bearer {TOKEN}")) {
                    return Reply {
                        status: 401,
                        headers: vec![(
                            "WWW-Authenticate",
                            format!(
                                "Bearer realm=\"https://localhost:{port}/token\",service=\"fake\",scope=\"repository:org/release:pull\""
                            ),
                        )],
                        body: vec![],
                    };
                }
            }
            if request.path.starts_with("/v2/org/release/manifests/") {
                return Reply::ok(self.manifest.clone());
            }
            request
                .path
                .strip_prefix("/v2/org/release/blobs/")
                .and_then(|digest| self.blobs.get(digest))
                .map_or_else(|| Reply::status(404), |blob| Reply::ok(blob.clone()))
        }
    }

    fn layer(title: Option<&str>, bytes: &[u8]) -> Value {
        json!({
            "mediaType": "application/octet-stream",
            "digest": manifest::raw_digest(bytes),
            "size": bytes.len(),
            "annotations": title.map_or_else(|| json!({}), |title| json!({ TITLE: title })),
        })
    }

    fn image(layers: &[Value]) -> Vec<u8> {
        json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.empty.v1+json",
                "digest": manifest::raw_digest(b"{}"),
                "size": 2,
            },
            "layers": layers,
        })
        .to_string()
        .into_bytes()
    }

    const RELEASE: &[u8] = b"{}";
    const PAYLOAD: &[u8] = b"payload bytes";
    const UNTITLED: &[u8] = b"untitled layer";

    fn release_image() -> Vec<u8> {
        image(&[
            layer(Some("release.json"), RELEASE),
            layer(Some("payload/nested/data.bin"), PAYLOAD),
            layer(None, UNTITLED),
        ])
    }

    fn serve(registry: Registry) -> Result<Fake> {
        Fake::serve(move |request, port| registry.reply(request, port))
    }

    fn rejected<T>(result: Result<T>, expected: &str) -> TestResult {
        match result {
            Err(crate::Error::Rejected(message)) if message == expected => Ok(()),
            Err(error) => Err(format!("expected {expected:?}, got {error}").into()),
            Ok(_) => Err(format!("expected {expected:?}, got success").into()),
        }
    }

    #[test]
    fn immutable_references_reject_ambiguous_transport_and_tags() -> Result<()> {
        let digest = format!("sha256:{}", "a".repeat(64));
        for repository in ["ghcr.io/org/release", "localhost:5000/org/release"] {
            ReleaseReference {
                oci: format!("{repository}@{digest}"),
                release_digest: digest.clone(),
            }
            .validate()?;
        }
        for reference in [
            "ghcr.io/org/release:latest".into(),
            format!("https://ghcr.io/org/release@{digest}"),
            format!("ghcr.io/org/release:tag@{digest}"),
            format!("user:password@ghcr.io/org/release@{digest}"),
            format!("ghcr.io/../release@{digest}"),
            format!("--output/path@{digest}"),
        ] {
            assert!(
                ReleaseReference {
                    oci: reference,
                    release_digest: digest.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            ReleaseReference {
                oci: format!("ghcr.io/org/release@{digest}"),
                release_digest: "bad".into()
            }
            .validate()
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn credentials_debug_hides_the_password() {
        let credentials = RegistryCredentials {
            username: "ci".into(),
            password: "private-token-canary".into(),
        };
        assert!(!format!("{credentials:?}").contains("private-token-canary"));
    }

    #[test]
    fn challenges_parse_quoted_parameters() {
        let challenge = Challenge::parse(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:org/a\"b:pull""#,
        );
        assert!(challenge.is_some_and(|challenge| {
            challenge.scheme == "Bearer"
                && challenge.params.get("realm").map(String::as_str)
                    == Some("https://auth.example/token")
                && challenge.params.get("scope").map(String::as_str)
                    == Some("repository:org/a\"b:pull")
        }));
        assert!(Challenge::parse(r#"Bearer realm="unterminated"#).is_none());
    }

    #[tokio::test]
    async fn titled_layers_are_materialised_and_others_ignored() -> TestResult {
        let manifest = release_image();
        let fake = serve(Registry::new(
            manifest.clone(),
            &[RELEASE, PAYLOAD, UNTITLED],
        ))?;
        let reference = fake.reference(&manifest, RELEASE);
        let fetched = fetch_release(&reference, &fake.access()).await?;
        assert_eq!(fetched.release_digest(), reference.release_digest);
        assert_eq!(fs::read(fetched.path().join("release.json"))?, RELEASE);
        assert_eq!(
            fs::read(fetched.path().join("payload/nested/data.bin"))?,
            PAYLOAD
        );
        let requests = fake.requests();
        assert!(
            !requests
                .iter()
                .any(|request| request.path.contains(&manifest::raw_digest(UNTITLED)))
        );
        assert!(
            requests
                .iter()
                .all(|request| request.authorization.is_none())
        );
        // The registry certificate is trusted only through the configured CA.
        rejected(
            fetch_release(&reference, &RegistryAccess::default()).await,
            FAILED,
        )
    }

    #[tokio::test]
    async fn anonymous_bearer_challenge_obtains_a_token() -> TestResult {
        let manifest = release_image();
        let mut registry = Registry::new(manifest.clone(), &[RELEASE, PAYLOAD]);
        registry.auth = Auth::AnonymousToken;
        let fake = serve(registry)?;
        fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await?;
        let requests = fake.requests();
        let tokens: Vec<_> = requests
            .iter()
            .filter(|request| request.path.starts_with("/token?"))
            .collect();
        assert_eq!(tokens.len(), 1);
        assert_eq!(
            tokens[0].path,
            "/token?service=fake&scope=repository%3Aorg%2Frelease%3Apull"
        );
        assert!(tokens[0].authorization.is_none());
        let bearer = format!("Bearer {TOKEN}");
        assert!(requests.iter().any(|request| {
            request.path.contains("/blobs/") && request.authorization.as_deref() == Some(&bearer)
        }));
        Ok(())
    }

    #[tokio::test]
    async fn credentials_are_sent_only_to_the_token_realm() -> TestResult {
        let manifest = release_image();
        let credentials = RegistryCredentials {
            username: "ci".into(),
            password: "secret".into(),
        };
        let mut registry = Registry::new(manifest.clone(), &[RELEASE, PAYLOAD]);
        registry.auth = Auth::TokenFor(basic(&credentials));
        let fake = serve(registry)?;
        let reference = fake.reference(&manifest, RELEASE);
        rejected(fetch_release(&reference, &fake.access()).await, FAILED)?;
        let access = RegistryAccess {
            credentials: Some(credentials.clone()),
            ..fake.access()
        };
        fetch_release(&reference, &access).await?;
        for request in fake.requests() {
            if request.path.starts_with("/v2/") {
                assert_ne!(request.authorization, Some(basic(&credentials)));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn manifest_must_match_the_reference_digest() -> TestResult {
        let manifest = release_image();
        let fake = serve(Registry::new(manifest, &[RELEASE, PAYLOAD]))?;
        let reference = fake.reference(b"another manifest", RELEASE);
        rejected(
            fetch_release(&reference, &fake.access()).await,
            "OCI manifest differs from its reference digest",
        )
    }

    #[tokio::test]
    async fn blobs_must_match_their_descriptor() -> TestResult {
        let manifest = release_image();
        let mut registry = Registry::new(manifest.clone(), &[RELEASE]);
        registry
            .blobs
            .insert(manifest::raw_digest(PAYLOAD), b"altered bytes".to_vec());
        let fake = serve(registry.clone())?;
        rejected(
            fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await,
            "OCI layer differs from its descriptor digest",
        )?;
        registry
            .blobs
            .insert(manifest::raw_digest(PAYLOAD), [PAYLOAD, b"!"].concat());
        let fake = serve(registry)?;
        rejected(
            fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await,
            "OCI layer exceeds its declared size",
        )
    }

    #[tokio::test]
    async fn unsafe_and_duplicate_titles_are_rejected() -> TestResult {
        let unsafe_titles = [
            "../escape",
            "/absolute",
            "a//b",
            "a\\b",
            "",
            "./release.json",
            "a/../b",
            "nul\0byte",
        ];
        let mut cases: Vec<(Vec<Value>, &str)> = unsafe_titles
            .iter()
            .map(|title| {
                (
                    vec![layer(Some(title), PAYLOAD)],
                    "OCI layer title is not a safe relative path",
                )
            })
            .collect();
        cases.push((
            vec![layer(Some("x"), PAYLOAD), layer(Some("x"), RELEASE)],
            "OCI manifest repeats a layer title",
        ));
        cases.push((
            vec![layer(Some("a/b"), PAYLOAD), layer(Some("a"), RELEASE)],
            "OCI layer title conflicts with another title's directory",
        ));
        for (layers, expected) in cases {
            let manifest = image(&layers);
            let fake = serve(Registry::new(manifest.clone(), &[RELEASE, PAYLOAD]))?;
            rejected(
                fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await,
                expected,
            )?;
            assert!(
                !fake
                    .requests()
                    .iter()
                    .any(|request| request.path.contains("/blobs/"))
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn an_index_is_not_an_image_manifest() -> TestResult {
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [],
        })
        .to_string()
        .into_bytes();
        let fake = serve(Registry::new(manifest.clone(), &[]))?;
        rejected(
            fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await,
            "OCI release reference names an index, not an image manifest",
        )
    }

    #[tokio::test]
    async fn redirects_stay_on_https_without_forwarding_authorization() -> TestResult {
        let manifest = release_image();
        let mut registry = Registry::new(manifest.clone(), &[RELEASE, PAYLOAD]);
        registry.auth = Auth::AnonymousToken;
        let cdn = registry.clone();
        // 127.0.0.1 is another authority than the registry's localhost.
        let fake = Fake::serve(move |request, port| {
            match request.path.strip_prefix("/v2/org/release/blobs/") {
                Some(digest) if request.authorization.is_some() => {
                    Reply::redirect(format!("https://127.0.0.1:{port}/cdn/{digest}"))
                }
                _ => cdn.reply(request, port),
            }
        })?;
        fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await?;
        let requests = fake.requests();
        assert!(
            requests
                .iter()
                .any(|request| request.path.starts_with("/cdn/"))
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.path.starts_with("/cdn/"))
                .all(|request| request.authorization.is_none())
        );

        let plain = registry.clone();
        let fake = Fake::serve(move |request, port| {
            match request.path.strip_prefix("/v2/org/release/blobs/") {
                Some(digest) if request.authorization.is_some() => {
                    Reply::redirect(format!("http://localhost:{port}/cdn/{digest}"))
                }
                _ => plain.reply(request, port),
            }
        })?;
        rejected(
            fetch_release(&fake.reference(&manifest, RELEASE), &fake.access()).await,
            "OCI registry redirected to a non-HTTPS location",
        )?;
        assert!(
            !fake
                .requests()
                .iter()
                .any(|request| request.path.starts_with("/cdn/"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn registry_diagnostics_do_not_escape_failure_boundary() -> TestResult {
        let manifest = release_image();
        let fake = Fake::serve(|request, port| {
            if request.path.starts_with("/token") {
                return Reply::ok(b"{\"token\": 7, \"detail\": \"private-token-canary\"}".to_vec());
            }
            Reply {
                status: if request.path.contains("/blobs/") {
                    500
                } else {
                    401
                },
                headers: vec![
                    ("X-Debug", "private-token-canary".into()),
                    (
                        "WWW-Authenticate",
                        format!(
                            "Bearer realm=\"https://localhost:{port}/token\",error=\"private-token-canary\""
                        ),
                    ),
                ],
                body: b"private-token-canary".to_vec(),
            }
        })?;
        for access in [
            fake.access(),
            RegistryAccess {
                credentials: Some(RegistryCredentials {
                    username: "ci".into(),
                    password: "private-token-canary".into(),
                }),
                ..fake.access()
            },
        ] {
            match fetch_release(&fake.reference(&manifest, RELEASE), &access).await {
                Err(error) => {
                    assert!(error.to_string().contains("retrieval failed"));
                    assert!(!error.to_string().contains("private-token-canary"));
                    assert!(!format!("{error:?}").contains("private-token-canary"));
                }
                Ok(_) => return Err("failed registry response was accepted".into()),
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn retrieved_release_must_match_the_protected_digest() -> TestResult {
        let manifest = release_image();
        let fake = serve(Registry::new(manifest.clone(), &[RELEASE, PAYLOAD]))?;
        let reference = fake.reference(&manifest, b"another release");
        rejected(
            fetch_release(&reference, &fake.access()).await,
            "resolved release differs from protected reference",
        )
    }
}
