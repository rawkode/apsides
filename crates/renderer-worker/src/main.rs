#![forbid(unsafe_code)]

use std::{ffi::OsString, net::SocketAddr, path::PathBuf, process};

use renderer_worker::{Supervisor, WorkerConfig, evaluator_once};

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    let outcome = match first.as_deref().and_then(|value| value.to_str()) {
        Some("--evaluator") => {
            let Some(flag) = arguments.next() else {
                process::exit(64);
            };
            let Some(root) = arguments.next() else {
                process::exit(64);
            };
            if flag != "--store-root" || arguments.next().is_some() {
                process::exit(64);
            }
            evaluator_once(&PathBuf::from(root))
        }
        Some("--listen") => {
            let Some(address) = arguments.next() else {
                process::exit(64);
            };
            let Some(flag) = arguments.next() else {
                process::exit(64);
            };
            let Some(root) = arguments.next() else {
                process::exit(64);
            };
            if flag != "--store-root" {
                process::exit(64);
            }
            let Ok(address) = address.to_string_lossy().parse::<SocketAddr>() else {
                process::exit(64);
            };
            let Ok(current_binary) = std::env::current_exe() else {
                process::exit(70);
            };
            let Ok(binary) =
                choose_evaluator_binary(&mut arguments, current_binary, cfg!(debug_assertions))
            else {
                process::exit(64);
            };
            Supervisor::bind(WorkerConfig::new(address, PathBuf::from(root), binary))
                .and_then(|supervisor| supervisor.run())
        }
        #[cfg(unix)]
        Some("--listen-unix") => {
            let Some(socket) = arguments.next() else {
                process::exit(64);
            };
            let Some(flag) = arguments.next() else {
                process::exit(64);
            };
            let Some(root) = arguments.next() else {
                process::exit(64);
            };
            if flag != "--store-root" {
                process::exit(64);
            }
            let Ok(current_binary) = std::env::current_exe() else {
                process::exit(70);
            };
            let Ok(binary) =
                choose_evaluator_binary(&mut arguments, current_binary, cfg!(debug_assertions))
            else {
                process::exit(64);
            };
            Supervisor::bind_unix(
                WorkerConfig::new("127.0.0.1:0".parse().unwrap(), PathBuf::from(root), binary),
                PathBuf::from(socket),
            )
            .and_then(|supervisor| supervisor.run())
        }
        _ => {
            process::exit(64);
        }
    };
    if outcome.is_err() {
        process::exit(70);
    }
}

/// Production listeners always re-exec their own evaluator mode. A debug-only
/// override exists exclusively for the process-containment integration test;
/// release binaries reject it so controller configuration cannot choose guest
/// executables.
fn choose_evaluator_binary(
    arguments: &mut impl Iterator<Item = OsString>,
    current_binary: PathBuf,
    allow_test_override: bool,
) -> Result<PathBuf, ()> {
    match (arguments.next(), arguments.next(), arguments.next()) {
        (None, None, None) => Ok(current_binary),
        (Some(option), Some(binary), None)
            if allow_test_override && option == "--evaluator-binary" =>
        {
            Ok(PathBuf::from(binary))
        }
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_mode_rejects_a_configured_evaluator() {
        let mut arguments = ["--evaluator-binary", "/tmp/untrusted"]
            .into_iter()
            .map(OsString::from);
        assert!(choose_evaluator_binary(&mut arguments, PathBuf::from("self"), false).is_err());
    }

    #[test]
    fn debug_harness_can_supply_its_containment_fixture() {
        let mut arguments = ["--evaluator-binary", "/tmp/fixture"]
            .into_iter()
            .map(OsString::from);
        assert_eq!(
            choose_evaluator_binary(&mut arguments, PathBuf::from("self"), true).unwrap(),
            PathBuf::from("/tmp/fixture")
        );
    }
}
