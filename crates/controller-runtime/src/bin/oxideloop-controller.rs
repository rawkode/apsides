//! v0 production single-root controller entrypoint.
//!
//! The process has one explicit root authority boundary. Bundle bytes come
//! only from the root's same-namespace `spec.program` `ConfigMap` reference.

use std::{path::PathBuf, process::ExitCode};

use oxideloop_controller_runtime::{ControllerDriverV0, DriverConfigV0, RuntimeErrorV0};

#[derive(Debug)]
struct Args {
    namespace: String,
    root_name: String,
    bundle_store: PathBuf,
    renderer_socket: PathBuf,
    #[cfg(feature = "test-failpoints")]
    failpoint_socket: Option<PathBuf>,
}

fn usage() -> &'static str {
    #[cfg(feature = "test-failpoints")]
    {
        "usage: oxideloop-controller --namespace NS --root-name NAME --bundle-store DIR --renderer-unix-socket SOCKET [--failpoint-unix-socket SOCKET]"
    }
    #[cfg(not(feature = "test-failpoints"))]
    {
        "usage: oxideloop-controller --namespace NS --root-name NAME --bundle-store DIR --renderer-unix-socket SOCKET"
    }
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(values: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut values = values.into_iter();
    let mut namespace = None;
    let mut root_name = None;
    let mut bundle_store = None;
    let mut renderer_socket = None;
    #[cfg(feature = "test-failpoints")]
    let mut failpoint_socket = None;
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--namespace" => namespace = Some(value),
            "--root-name" => root_name = Some(value),
            "--bundle-store" => bundle_store = Some(PathBuf::from(value)),
            "--renderer-unix-socket" => renderer_socket = Some(PathBuf::from(value)),
            #[cfg(feature = "test-failpoints")]
            "--failpoint-unix-socket" => failpoint_socket = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    let args = Args {
        namespace: namespace.ok_or("--namespace is required")?,
        root_name: root_name.ok_or("--root-name is required")?,
        bundle_store: bundle_store.ok_or("--bundle-store is required")?,
        renderer_socket: renderer_socket.ok_or("--renderer-unix-socket is required")?,
        #[cfg(feature = "test-failpoints")]
        failpoint_socket,
    };
    if args.namespace.is_empty() || args.root_name.is_empty() || !args.renderer_socket.is_absolute()
    {
        return Err(
            "namespace/root name must be nonempty and renderer socket must be absolute".into(),
        );
    }
    #[cfg(feature = "test-failpoints")]
    if args
        .failpoint_socket
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err("failpoint socket must be absolute".into());
    }
    Ok(args)
}

/// Select the process-wide rustls backend before `KubeTransportV0::in_cluster`
/// constructs its HTTPS client. The production dependency graph deliberately
/// disables rustls default features, so this must not rely on transitive
/// feature selection.
fn install_crypto_provider() -> Result<(), String> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| "rustls CryptoProvider was already installed incompatibly".to_owned())
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = install_crypto_provider() {
        eprintln!("controller setup failed: {error}");
        return ExitCode::from(1);
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{}", usage());
            return ExitCode::from(2);
        }
    };
    let config = DriverConfigV0 {
        namespace: args.namespace,
        root_name: args.root_name,
        bundle_store: args.bundle_store,
        renderer_unix_socket: args.renderer_socket,
        #[cfg(feature = "test-failpoints")]
        failpoint_unix_socket: args.failpoint_socket,
    };
    let driver = match ControllerDriverV0::in_cluster(config).await {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("controller setup failed: {error}");
            return ExitCode::from(1);
        }
    };
    match Box::pin(driver.run()).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(RuntimeErrorV0::InjectedCrash(point)) => {
            eprintln!("injected crash at {point:?}");
            ExitCode::from(75)
        }
        Err(error) => {
            eprintln!("controller stopped: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parser_accepts_only_the_production_surface() {
        assert!(!usage().contains("bundle-archive"));
        let args = parse_args_from([
            "--namespace".into(),
            "ns".into(),
            "--root-name".into(),
            "root".into(),
            "--bundle-store".into(),
            "/tmp/store".into(),
            "--renderer-unix-socket".into(),
            "/tmp/renderer.sock".into(),
        ])
        .expect("production arguments accepted");
        assert_eq!(args.namespace, "ns");
        assert_eq!(args.root_name, "root");
    }

    #[cfg(not(feature = "test-failpoints"))]
    #[test]
    fn default_parser_rejects_debug_only_failpoint_socket() {
        assert!(matches!(
            parse_args_from(["--failpoint-unix-socket".into(), "/tmp/f.sock".into()]),
            Err(error) if error == "unknown argument --failpoint-unix-socket"
        ));
    }
}
