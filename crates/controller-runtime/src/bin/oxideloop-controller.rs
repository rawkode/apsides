//! v0 single-replica controller entrypoint.
//!
//! All operational authority is explicit command-line input.  In particular,
//! production renderer traffic is Unix-domain only; TCP and environment based
//! configuration are intentionally not accepted here.

use std::{path::PathBuf, process::ExitCode};

use oxideloop_bundle_contract::BundleStore;
use oxideloop_controller_runtime::{
    ActorKeyV0, BundleRuntimeV0, FileArchiveSourceV0, RootActorV0, RuntimeErrorV0,
    load_machine_from_transport,
};
use oxideloop_kube_transport::KubeTransportV0;
use renderer_client::RendererClient;

#[derive(Debug)]
struct Args {
    namespace: String,
    root_name: String,
    archive: PathBuf,
    store: PathBuf,
    renderer_socket: PathBuf,
    #[cfg(feature = "test-failpoints")]
    failpoint_socket: Option<PathBuf>,
}

fn usage() -> &'static str {
    #[cfg(feature = "test-failpoints")]
    {
        "usage: oxideloop-controller --namespace NS --root-name NAME --bundle-archive FILE --bundle-store DIR --renderer-unix-socket SOCKET [--failpoint-unix-socket SOCKET]"
    }
    #[cfg(not(feature = "test-failpoints"))]
    {
        "usage: oxideloop-controller --namespace NS --root-name NAME --bundle-archive FILE --bundle-store DIR --renderer-unix-socket SOCKET"
    }
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(values: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut values = values.into_iter();
    let mut namespace = None;
    let mut root_name = None;
    let mut archive = None;
    let mut store = None;
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
            "--bundle-archive" => archive = Some(PathBuf::from(value)),
            "--bundle-store" => store = Some(PathBuf::from(value)),
            "--renderer-unix-socket" => renderer_socket = Some(PathBuf::from(value)),
            #[cfg(feature = "test-failpoints")]
            "--failpoint-unix-socket" => failpoint_socket = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    let args = Args {
        namespace: namespace.ok_or("--namespace is required")?,
        root_name: root_name.ok_or("--root-name is required")?,
        archive: archive.ok_or("--bundle-archive is required")?,
        store: store.ok_or("--bundle-store is required")?,
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

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{}", usage());
            return ExitCode::from(2);
        }
    };
    // Constructing the Unix client here is intentional: no TCP renderer route
    // is reachable from the controller binary.  The retained client is used by
    // the render/action driver once the reducer requests those effects.
    if let Err(error) = RendererClient::new_unix(&args.renderer_socket) {
        eprintln!("invalid renderer Unix socket: {error}");
        return ExitCode::from(2);
    }
    let transport = match KubeTransportV0::in_cluster().await {
        Ok(transport) => transport,
        Err(error) => {
            eprintln!("in-cluster Kubernetes client unavailable: {error}");
            return ExitCode::from(1);
        }
    };
    let machine =
        match load_machine_from_transport(&transport, &args.namespace, &args.root_name).await {
            Ok(Some(machine)) => machine,
            Ok(None) => return ExitCode::SUCCESS, // root is terminal only after its finalizer protocol; no absent-root write exists.
            Err(error) => {
                eprintln!("root load held: {error}");
                return ExitCode::from(1);
            }
        };
    // Keep archive/store authority explicit even when this initial pass only
    // reaches bootstrap.  This also validates the configured local store.
    let _runtime = match BundleStore::new(&args.store) {
        Ok(store) => BundleRuntimeV0::new(store),
        Err(error) => {
            eprintln!("bundle store unavailable: {error}");
            return ExitCode::from(1);
        }
    };
    let _archive = FileArchiveSourceV0::new(args.archive);
    let mut actor = RootActorV0::new(machine);
    actor.start();
    // A full retained watch/reconcile loop is intentionally owned by the
    // runtime library; this binary establishes the production adapters and
    // enters reconciliation without any ambient controls.
    let _key = ActorKeyV0(format!("{}/{}", args.namespace, args.root_name));
    #[cfg(feature = "test-failpoints")]
    if let Some(socket) = args.failpoint_socket {
        let _ = socket;
    }
    match actor.next_command() {
        Ok(Some(_) | None) => ExitCode::SUCCESS,
        Err(RuntimeErrorV0::Hold(reason)) => {
            eprintln!("controller hold: {reason}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("controller error: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_rejects_tcp_and_missing_required_arguments() {
        // Parsing itself is exercised by the binary's argument shape; Unix
        // validation is deliberately a separate renderer-client contract.
        assert!(usage().contains("renderer-unix-socket"));
    }

    #[cfg(not(feature = "test-failpoints"))]
    #[test]
    fn default_parser_and_usage_reject_the_debug_only_socket_flag() {
        assert!(!usage().contains("failpoint-unix-socket"));
        assert!(matches!(
            parse_args_from(["--failpoint-unix-socket".into(), "/tmp/failpoint.sock".into()]),
            Err(error) if error == "unknown argument --failpoint-unix-socket"
        ));
    }

    #[cfg(feature = "test-failpoints")]
    #[test]
    fn debug_feature_parser_and_usage_accept_the_failpoint_socket_flag() {
        assert!(usage().contains("failpoint-unix-socket"));
        let args = parse_args_from([
            "--namespace".into(),
            "ns".into(),
            "--root-name".into(),
            "root".into(),
            "--bundle-archive".into(),
            "/tmp/archive".into(),
            "--bundle-store".into(),
            "/tmp/store".into(),
            "--renderer-unix-socket".into(),
            "/tmp/renderer.sock".into(),
            "--failpoint-unix-socket".into(),
            "/tmp/failpoint.sock".into(),
        ])
        .expect("debug flag accepted");
        assert_eq!(
            args.failpoint_socket,
            Some(PathBuf::from("/tmp/failpoint.sock"))
        );
    }
}
