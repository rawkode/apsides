//! The pinned `rusty_v8` (`v8` crate) prebuilt static archive.
//!
//! The `v8` crate's build script downloads this archive unless
//! `RUSTY_V8_ARCHIVE` names a local copy, and the controller compiles with no
//! network. So it is fetched here, before the compile, verified against
//! `scripts/rusty-v8/pins.json` and against the `v8` version the controller
//! lock pins, and cached outside the checkout and any installed distribution.
//! Only the pin is source-controlled, never the archive.

use crate::{
    Error, Result, Stage,
    digest::{HashingReader, is_hex64},
    fs::read_regular,
    redact::redact,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// The pins, relative to the assets root.
pub const PINS: &str = "scripts/rusty-v8/pins.json";
/// Where the archive is staged in the build context, whatever the target.
pub const CONTEXT_PATH: &str = "rusty-v8/librusty_v8.a.gz";
/// The most bytes a download may hold.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PINS_BYTES: u64 = 1024 * 1024;

fn fail(detail: impl Into<String>) -> Error {
    Error::new(Stage::RustyV8, detail)
}

/// `scripts/rusty-v8/pins.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pins {
    pub v8_crate_version: String,
    archives: Map<String, Value>,
}

/// The archive pinned for one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivePin {
    pub target: String,
    pub url: String,
    pub sha256_hex: String,
    pub name: String,
    pub version: String,
}

/// The cache directory: `APSIDES_RUSTY_V8_CACHE`, else
/// `$XDG_CACHE_HOME/apsides/rusty-v8`, else `$HOME/.cache/apsides/rusty-v8`.
/// `lookup` reads the environment; an empty value counts as unset.
///
/// # Errors
/// Fails when none of the three is set.
pub fn cache_dir(lookup: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf> {
    let set = |name: &str| {
        lookup(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    set("APSIDES_RUSTY_V8_CACHE")
        .or_else(|| set("XDG_CACHE_HOME").map(|cache| cache.join("apsides/rusty-v8")))
        .or_else(|| set("HOME").map(|home| home.join(".cache/apsides/rusty-v8")))
        .ok_or_else(|| {
            fail(
                "cannot determine a rusty_v8 archive cache directory: set APSIDES_RUSTY_V8_CACHE \
                 (or XDG_CACHE_HOME, or HOME) to a writable directory outside any installed \
                 Apsides CLI distribution prefix",
            )
        })
}

/// Reads the pins under `assets`.
///
/// # Errors
/// Fails when the file is unreadable or malformed, lacks `v8CrateVersion`,
/// or requires a binding file, which this build does not stage yet.
pub fn load_pins(assets: &Path) -> Result<Pins> {
    let path = assets.join(PINS);
    let bytes = read_regular(&path, MAX_PINS_BYTES)
        .map_err(|error| fail(format!("cannot read {}: {error}", path.display())))?;
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|error| fail(format!("{PINS} is not JSON: {error}")))?;
    if document["bindingFile"]["required"] == true {
        return Err(fail(format!(
            "{PINS} now requires a rusty_v8 binding file; crates/packaging (src/rusty_v8.rs and \
             src/compile.rs) must fetch, verify and pass RUSTY_V8_SRC_BINDING_PATH before building"
        )));
    }
    let v8_crate_version = document["v8CrateVersion"]
        .as_str()
        .filter(|version| !version.is_empty())
        .ok_or_else(|| fail(format!("{PINS} lacks v8CrateVersion")))?
        .to_owned();
    let archives = document["archives"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    Ok(Pins {
        v8_crate_version,
        archives,
    })
}

/// The archive pinned for `target`.
///
/// # Errors
/// Fails for a target with no pin, a URL that is not `https://` and a digest
/// that is not 64 lowercase hex characters.
pub fn archive_pin(pins: &Pins, target: &str) -> Result<ArchivePin> {
    let entry = pins
        .archives
        .get(target)
        .and_then(Value::as_object)
        .ok_or_else(|| {
            fail(format!(
                "no pinned rusty_v8 archive for target \"{target}\"; add one to {PINS}"
            ))
        })?;
    let url = entry
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("https://"))
        .ok_or_else(|| {
            fail(format!(
                "pinned rusty_v8 archive URL for {target} in {PINS} must be an explicit https URL"
            ))
        })?;
    let sha256_hex = entry
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|hex| is_hex64(hex))
        .ok_or_else(|| {
            fail(format!(
                "pinned rusty_v8 archive sha256 for {target} in {PINS} must be 64 lowercase hex \
                 characters"
            ))
        })?;
    Ok(ArchivePin {
        target: target.to_owned(),
        url: url.to_owned(),
        sha256_hex: sha256_hex.to_owned(),
        name: "librusty_v8".to_owned(),
        version: pins.v8_crate_version.clone(),
    })
}

/// Fails unless the controller lock pins exactly the `v8` version the pins
/// were made for. `locked_v8` lists the version of every crates.io `v8`
/// package in the lock.
///
/// # Errors
/// Fails when the lock pins no `v8`, more than one version, or another
/// version than the pins.
pub fn ensure_pin_matches_lock(pins: &Pins, locked_v8: &[String]) -> Result<()> {
    let versions: BTreeSet<&str> = locked_v8.iter().map(String::as_str).collect();
    match versions.into_iter().collect::<Vec<_>>().as_slice() {
        [] => Err(fail(format!(
            "controller Cargo.lock does not lock the v8 crate; cannot verify the rusty_v8 archive \
             pin in {PINS} against it"
        ))),
        [locked] if *locked == pins.v8_crate_version => Ok(()),
        [locked] => Err(fail(format!(
            "{PINS} pins v8 crate version \"{}\" but controller/Cargo.lock locks \"{locked}\"; \
             update pins.json's archive URLs and SHA-256 digests to match the locked v8 crate \
             version before building",
            pins.v8_crate_version
        ))),
        _ => Err(fail(
            "controller Cargo.lock pins more than one v8 crate version; resolve the duplicate \
             lock entries (cargo update -p v8 --precise <version>) before building",
        )),
    }
}

/// Downloads a URL into a sink.
pub trait Fetch {
    /// Streams the body at `url` into `sink` and returns its length.
    ///
    /// # Errors
    /// Describes why the download failed.
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> std::result::Result<u64, String>;
}

/// [`Fetch`] over HTTPS with rustls, honouring `ALL_PROXY`, `HTTPS_PROXY`,
/// `HTTP_PROXY` and `NO_PROXY`.
#[derive(Clone, Copy, Debug, Default)]
pub struct HttpsFetch;

/// The agent configuration [`HttpsFetch`] uses with `proxy`.
fn agent_config(proxy: Option<ureq::Proxy>) -> ureq::config::Config {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(600)))
        .max_redirects(10)
        .proxy(proxy)
        .build()
}

impl Fetch for HttpsFetch {
    fn fetch(&self, url: &str, sink: &mut dyn Write) -> std::result::Result<u64, String> {
        let proxy = ureq::Proxy::try_from_env();
        // Name the proxy route (never its value), so a broken proxy is not
        // mistaken for an unreachable download host.
        let via = if proxy.is_some() {
            " (through the proxy that ALL_PROXY, HTTPS_PROXY or HTTP_PROXY names)"
        } else {
            ""
        };
        let agent = agent_config(proxy).new_agent();
        let response = agent
            .get(url)
            .call()
            .map_err(|error| format!("{error}{via}"))?;
        let mut body = response
            .into_body()
            .into_with_config()
            .limit(MAX_ARCHIVE_BYTES)
            .reader();
        io::copy(&mut body, sink).map_err(|error| error.to_string())
    }
}

/// Hashes and counts what is written through it, refusing more than
/// [`MAX_ARCHIVE_BYTES`].
struct Download<'a> {
    file: &'a mut File,
    hasher: Sha256,
    written: u64,
}

impl Write for Download<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.written + buffer.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(io::Error::other(format!(
                "the archive exceeds {} MiB",
                MAX_ARCHIVE_BYTES / (1024 * 1024)
            )));
        }
        let count = self.file.write(buffer)?;
        self.hasher.update(&buffer[..count]);
        self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Whether the regular, non-symlink file at `path` hashes to `hex`.
fn cached_matches(path: &Path, hex: &str) -> Result<bool> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(false);
    }
    let unreadable = |error: io::Error| {
        fail(format!(
            "cannot read the cached rusty_v8 archive {} ({error}); set APSIDES_RUSTY_V8_CACHE to \
             a different, readable directory, or remove that file so it can be re-fetched",
            path.display()
        ))
    };
    let mut reader = HashingReader::new(File::open(path).map_err(unreadable)?);
    io::copy(&mut reader, &mut io::sink()).map_err(unreadable)?;
    Ok(reader.finish().0 == format!("sha256:{hex}"))
}

/// A verified local copy of the archive `pin` names, cached under `cache` by
/// its digest.
///
/// A cached copy that still hashes to the pin is used without any network
/// access; a corrupt one is fetched again. A download is streamed into a
/// temporary file in the cache, verified, synced and only then moved into
/// place, so the cache never holds an unverified archive.
///
/// # Errors
/// Fails when the cache cannot be created, read or written, when the archive
/// is not cached and cannot be fetched, and when the fetched bytes differ
/// from the pin.
pub fn fetch(pin: &ArchivePin, cache: &Path, fetcher: &dyn Fetch) -> Result<PathBuf> {
    let unwritable = |error: &dyn std::fmt::Display| {
        fail(format!(
            "cannot write the fetched rusty_v8 archive into {} ({error}); set \
             APSIDES_RUSTY_V8_CACHE to a writable directory",
            cache.display()
        ))
    };
    fs::create_dir_all(cache).map_err(|error| {
        fail(format!(
            "cannot create the rusty_v8 archive cache directory {} ({error}); set \
             APSIDES_RUSTY_V8_CACHE to a writable directory",
            cache.display()
        ))
    })?;
    let cached = cache.join(format!("{}.a.gz", pin.sha256_hex));
    if cached_matches(&cached, &pin.sha256_hex)? {
        return Ok(cached);
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".librusty_v8-")
        .suffix(".tmp")
        .tempfile_in(cache)
        .map_err(|error| unwritable(&error))?;
    let mut download = Download {
        file: temporary.as_file_mut(),
        hasher: Sha256::new(),
        written: 0,
    };
    fetcher.fetch(&pin.url, &mut download).map_err(|error| {
        fail(format!(
            "rusty_v8 archive for {} is not cached under {} and fetching {} failed ({}); connect \
             to the network once to populate the cache, or restore a previously fetched archive \
             into that directory",
            pin.target,
            cache.display(),
            pin.url,
            redact(&error)
        ))
    })?;
    let actual = format!("{:x}", download.hasher.finalize());
    if actual != pin.sha256_hex {
        return Err(fail(format!(
            "rusty_v8 archive for {} failed sha256 verification: expected {}, got {actual}; check \
             {PINS}'s pinned sha256 against the archive actually published at its url",
            pin.target, pin.sha256_hex
        )));
    }
    temporary
        .as_file_mut()
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| unwritable(&error))?;
    temporary
        .persist(&cached)
        .map_err(|error| unwritable(&error.error))?;
    Ok(cached)
}

/// `path` made absolute with every existing ancestor's symlinks resolved, so
/// a containment check holds for a directory that does not exist yet.
pub(crate) fn resolved(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(canonical) => {
                return Ok(rest
                    .iter()
                    .rev()
                    .fold(canonical, |path, part| path.join(part)));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(error);
                };
                rest.push(name.to_owned());
                existing = parent;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Fails when `cache` lies inside `assets`: an installed distribution
/// verifies its whole asset tree on every `aps` run, so a cache there would
/// break every later command.
///
/// # Errors
/// As described, and when either path cannot be resolved.
pub fn ensure_outside(cache: &Path, assets: &Path) -> Result<()> {
    let unresolvable =
        |path: &Path, error: io::Error| fail(format!("cannot resolve {}: {error}", path.display()));
    let cache_path = resolved(cache).map_err(|error| unresolvable(cache, error))?;
    let assets_path = assets
        .canonicalize()
        .map_err(|error| unresolvable(assets, error))?;
    if cache_path.starts_with(&assets_path) {
        return Err(fail(format!(
            "the rusty_v8 archive cache {} is inside {}; set APSIDES_RUSTY_V8_CACHE to a writable \
             directory outside any installed Apsides CLI distribution prefix",
            cache_path.display(),
            assets_path.display()
        )));
    }
    Ok(())
}

/// Loads the pin for `target`, checks it against the locked `v8` versions,
/// and returns a verified local copy of its archive with the pin.
///
/// # Errors
/// As [`load_pins`], [`ensure_pin_matches_lock`], [`archive_pin`],
/// [`ensure_outside`] and [`fetch`]; the pin and lock are checked before any
/// network access.
pub fn resolve(
    assets: &Path,
    target: &str,
    locked_v8: &[String],
    cache: &Path,
    fetcher: &dyn Fetch,
) -> Result<(PathBuf, ArchivePin)> {
    let pins = load_pins(assets)?;
    ensure_pin_matches_lock(&pins, locked_v8)?;
    let pin = archive_pin(&pins, target)?;
    ensure_outside(cache, assets)?;
    Ok((fetch(&pin, cache, fetcher)?, pin))
}

/// The digest record of an archive pin, as the build context and compiler
/// closure state it.
#[must_use]
pub fn component(pin: &ArchivePin) -> release::RustyV8Component {
    release::RustyV8Component {
        name: pin.name.clone(),
        version: pin.version.clone(),
        target: pin.target.clone(),
        url: pin.url.clone(),
        sha256: format!("sha256:{}", pin.sha256_hex),
        path: CONTEXT_PATH.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::sha256_hex;
    use serde_json::json;
    use std::{cell::RefCell, collections::BTreeMap};

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const DATA: &[u8] = b"a fake rusty_v8 prebuilt static archive\n";

    /// Serves fixed bytes, or fails, and records every URL asked for.
    struct FakeFetch {
        body: std::result::Result<&'static [u8], &'static str>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeFetch {
        fn serving(body: &'static [u8]) -> Self {
            Self {
                body: Ok(body),
                calls: RefCell::default(),
            }
        }

        fn offline() -> Self {
            Self {
                body: Err("network is unreachable; token=SEKRET1234567890"),
                calls: RefCell::default(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl Fetch for FakeFetch {
        fn fetch(&self, url: &str, sink: &mut dyn Write) -> std::result::Result<u64, String> {
            self.calls.borrow_mut().push(url.to_owned());
            let body = self.body.map_err(str::to_owned)?;
            sink.write_all(body).map_err(|error| error.to_string())?;
            Ok(body.len() as u64)
        }
    }

    fn pins_document() -> Value {
        json!({
            "v8CrateVersion": "150.4.0",
            "bindingFile": {"required": false},
            "archives": {TARGET: {"url": "https://example.invalid/rusty-v8/x86_64.a.gz", "sha256": sha256_hex(DATA)}},
        })
    }

    fn pins(document: &Value) -> Result<Pins> {
        let assets = tempfile::tempdir().map_err(|error| fail(error.to_string()))?;
        write_pins(assets.path(), document).map_err(|error| fail(error.to_string()))?;
        load_pins(assets.path())
    }

    fn write_pins(assets: &Path, document: &Value) -> io::Result<()> {
        fs::create_dir_all(assets.join("scripts/rusty-v8"))?;
        fs::write(assets.join(PINS), document.to_string())
    }

    fn pin() -> Result<ArchivePin> {
        archive_pin(&pins(&pins_document())?, TARGET)
    }

    fn files(directory: &Path) -> io::Result<Vec<String>> {
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let mut names = fs::read_dir(directory)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    #[test]
    fn a_fetch_is_verified_cached_and_never_repeated() -> TestResult {
        let cache = tempfile::tempdir()?;
        let pin = pin()?;
        let fetcher = FakeFetch::serving(DATA);
        let path = fetch(&pin, cache.path(), &fetcher)?;
        assert_eq!(
            path,
            cache.path().join(format!("{}.a.gz", sha256_hex(DATA)))
        );
        assert_eq!(fs::read(&path)?, DATA);
        assert_eq!(fetcher.calls(), 1);
        assert_eq!(fetch(&pin, cache.path(), &fetcher)?, path);
        assert_eq!(fetcher.calls(), 1);
        assert_eq!(files(cache.path())?, [format!("{}.a.gz", sha256_hex(DATA))]);
        Ok(())
    }

    #[test]
    fn a_wrong_digest_fails_and_caches_nothing() -> TestResult {
        let cache = tempfile::tempdir()?;
        let mut pin = pin()?;
        pin.sha256_hex = "0".repeat(64);
        let error = fetch(&pin, cache.path(), &FakeFetch::serving(DATA))
            .err()
            .ok_or("accepted")?;
        assert!(
            error
                .detail
                .contains("failed sha256 verification: expected 0000"),
            "{}",
            error.detail
        );
        assert_eq!(files(cache.path())?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn an_offline_empty_cache_says_what_to_do_without_leaking_secrets() -> TestResult {
        let cache = tempfile::tempdir()?;
        let error = fetch(&pin()?, cache.path(), &FakeFetch::offline())
            .err()
            .ok_or("accepted")?;
        assert!(
            error.detail.contains("is not cached under"),
            "{}",
            error.detail
        );
        assert!(
            error.detail.contains("token=[REDACTED]"),
            "{}",
            error.detail
        );
        assert!(!error.detail.contains("SEKRET"));
        assert!(error.detail.contains("connect to the network once"));
        assert_eq!(files(cache.path())?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_corrupt_or_symlinked_cache_entry_is_fetched_again() -> TestResult {
        let cache = tempfile::tempdir()?;
        let pin = pin()?;
        let cached = cache.path().join(format!("{}.a.gz", pin.sha256_hex));
        fs::write(&cached, b"corrupted, not the real archive")?;
        let fetcher = FakeFetch::serving(DATA);
        assert_eq!(fs::read(fetch(&pin, cache.path(), &fetcher)?)?, DATA);
        assert_eq!(fetcher.calls(), 1);
        let elsewhere = cache.path().join("elsewhere");
        fs::write(&elsewhere, DATA)?;
        fs::remove_file(&cached)?;
        std::os::unix::fs::symlink(&elsewhere, &cached)?;
        fetch(&pin, cache.path(), &fetcher)?;
        assert_eq!(fetcher.calls(), 2);
        assert!(fs::symlink_metadata(&cached)?.is_file());
        Ok(())
    }

    #[test]
    fn an_uncreatable_cache_is_a_clear_error() -> TestResult {
        let root = tempfile::tempdir()?;
        let blocking = root.path().join("not-a-directory");
        fs::write(&blocking, "occupies the path")?;
        let error = fetch(&pin()?, &blocking.join("nested"), &FakeFetch::serving(DATA))
            .err()
            .ok_or("accepted")?;
        assert!(
            error
                .detail
                .starts_with("cannot create the rusty_v8 archive cache directory"),
            "{}",
            error.detail
        );
        Ok(())
    }

    #[test]
    fn a_required_binding_file_fails_closed() {
        let mut document = pins_document();
        document["bindingFile"]["required"] = json!(true);
        let error = pins(&document).err();
        assert!(
            error.is_some_and(|error| error.detail.contains("requires a rusty_v8 binding file"))
        );
    }

    #[test]
    fn malformed_pins_are_rejected() -> TestResult {
        let loaded = pins(&pins_document())?;
        let error = archive_pin(&loaded, "x86_64-pc-windows-msvc")
            .err()
            .ok_or("accepted")?;
        assert!(
            error
                .detail
                .starts_with("no pinned rusty_v8 archive for target \"x86_64-pc-windows-msvc\"")
        );
        let upper = sha256_hex(DATA).to_uppercase();
        for (field, value, expected) in [
            ("url", "http://insecure.example/archive.gz", "https URL"),
            ("sha256", "not-hex", "64 lowercase hex"),
            ("sha256", upper.as_str(), "64 lowercase hex"),
            ("sha256", "abcd", "64 lowercase hex"),
        ] {
            let mut document = pins_document();
            document["archives"][TARGET][field] = json!(value);
            let error = archive_pin(&pins(&document)?, TARGET)
                .err()
                .ok_or("accepted")?;
            assert!(
                error.detail.contains(expected),
                "{field}={value}: {}",
                error.detail
            );
        }
        let mut document = pins_document();
        document["v8CrateVersion"] = json!("");
        assert!(pins(&document).is_err());
        Ok(())
    }

    #[test]
    fn the_pin_must_match_the_locked_version() -> TestResult {
        let loaded = pins(&pins_document())?;
        ensure_pin_matches_lock(&loaded, &["150.4.0".into()])?;
        ensure_pin_matches_lock(&loaded, &["150.4.0".into(), "150.4.0".into()])?;
        let cases: [(&[&str], &str); 3] = [
            (&[], "does not lock the v8 crate"),
            (
                &["150.4.1"],
                "pins v8 crate version \"150.4.0\" but controller/Cargo.lock locks \"150.4.1\"",
            ),
            (&["150.4.0", "150.4.1"], "more than one v8 crate version"),
        ];
        for (locked, expected) in cases {
            let locked: Vec<String> = locked.iter().map(|version| (*version).to_owned()).collect();
            let error = ensure_pin_matches_lock(&loaded, &locked)
                .err()
                .ok_or("accepted")?;
            assert!(
                error.detail.contains(expected),
                "{locked:?}: {}",
                error.detail
            );
        }
        Ok(())
    }

    #[test]
    fn a_pin_lock_mismatch_fails_before_any_fetch() -> TestResult {
        let assets = tempfile::tempdir()?;
        write_pins(assets.path(), &pins_document())?;
        let cache = tempfile::tempdir()?;
        let fetcher = FakeFetch::serving(DATA);
        let error = resolve(
            assets.path(),
            TARGET,
            &["999.0.0".into()],
            cache.path(),
            &fetcher,
        )
        .err()
        .ok_or("accepted")?;
        assert!(error.detail.contains("999.0.0"));
        assert_eq!(fetcher.calls(), 0);
        let (path, pin) = resolve(
            assets.path(),
            TARGET,
            &["150.4.0".into()],
            cache.path(),
            &fetcher,
        )?;
        assert_eq!(fs::read(path)?, DATA);
        assert_eq!(pin.sha256_hex, sha256_hex(DATA));
        let record = component(&pin);
        assert_eq!(record.sha256, format!("sha256:{}", sha256_hex(DATA)));
        assert_eq!(
            (
                record.name.as_str(),
                record.version.as_str(),
                record.path.as_str()
            ),
            ("librusty_v8", "150.4.0", CONTEXT_PATH)
        );
        Ok(())
    }

    #[test]
    fn the_cache_directory_follows_its_precedence() -> TestResult {
        let lookup = |values: &[(&str, &str)]| {
            let values: BTreeMap<String, OsString> = values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
                .collect();
            move |name: &str| values.get(name).cloned()
        };
        let all = [
            ("APSIDES_RUSTY_V8_CACHE", "/a"),
            ("XDG_CACHE_HOME", "/x"),
            ("HOME", "/h"),
        ];
        assert_eq!(cache_dir(lookup(&all))?, Path::new("/a"));
        assert_eq!(
            cache_dir(lookup(&all[1..]))?,
            Path::new("/x/apsides/rusty-v8")
        );
        assert_eq!(
            cache_dir(lookup(&all[2..]))?,
            Path::new("/h/.cache/apsides/rusty-v8")
        );
        let empty = [
            ("APSIDES_RUSTY_V8_CACHE", ""),
            ("XDG_CACHE_HOME", ""),
            ("HOME", "/h"),
        ];
        assert_eq!(
            cache_dir(lookup(&empty))?,
            Path::new("/h/.cache/apsides/rusty-v8")
        );
        let error = cache_dir(lookup(&[("HOME", "")])).err().ok_or("accepted")?;
        assert!(
            error
                .detail
                .starts_with("cannot determine a rusty_v8 archive cache directory")
        );
        Ok(())
    }

    #[test]
    fn a_distribution_prefix_is_never_written() -> TestResult {
        let root = tempfile::tempdir()?;
        let assets = root.path().join("prefix/share/apsides");
        write_pins(&assets, &pins_document())?;
        let home = root.path().join("home");
        fs::create_dir(&home)?;
        let home_value = home.clone().into_os_string();
        let cache = cache_dir(|name| (name == "HOME").then(|| home_value.clone()))?;
        assert_eq!(cache, home.join(".cache/apsides/rusty-v8"));
        let before = files(&assets.join("scripts/rusty-v8"))?;
        let (path, _) = resolve(
            &assets,
            TARGET,
            &["150.4.0".into()],
            &cache,
            &FakeFetch::serving(DATA),
        )?;
        assert!(path.starts_with(&home));
        assert_eq!(files(&assets.join("scripts/rusty-v8"))?, before);
        assert_eq!(files(&assets.join("scripts"))?, ["rusty-v8"]);

        let inside = assets.join("cache/rusty-v8");
        let error = resolve(
            &assets,
            TARGET,
            &["150.4.0".into()],
            &inside,
            &FakeFetch::serving(DATA),
        )
        .err()
        .ok_or("a cache inside the assets was accepted")?;
        assert!(error.detail.contains("is inside"), "{}", error.detail);
        assert!(!assets.join("cache").exists());
        Ok(())
    }

    #[test]
    fn the_agent_takes_its_proxy_from_the_environment_reader() -> TestResult {
        let proxy = ureq::Proxy::new("http://proxy.invalid:3128")?;
        let config = agent_config(Some(proxy));
        let configured = config.proxy().ok_or("no proxy configured")?;
        assert_eq!(
            (configured.host(), configured.port()),
            ("proxy.invalid", 3128)
        );
        assert!(config.https_only());
        assert_eq!(config.max_redirects(), 10);
        assert!(agent_config(None).proxy().is_none());
        Ok(())
    }
}
