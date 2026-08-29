use std::{path::Path, time::Duration};

use renderer_host::{ProbeKind, SupervisedProbe, supervise_probe};

fn probe_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_renderer-host-probe"))
}

#[test]
fn probe_supervision_distinguishes_recoverable_and_process_fatal() {
    let recoverable = supervise_probe(
        probe_binary(),
        ProbeKind::Recoverable,
        Duration::from_secs(2),
    )
    .unwrap();
    assert!(
        matches!(recoverable, SupervisedProbe::Exited { success: true, ref stdout } if stdout.starts_with("recoverable:"))
    );

    let fatal = supervise_probe(probe_binary(), ProbeKind::Fatal, Duration::from_secs(2)).unwrap();
    assert!(matches!(
        fatal,
        SupervisedProbe::Exited { success: false, .. }
    ));
}

#[test]
fn probe_supervision_could_kill_a_non_cooperative_child() {
    let result = supervise_probe(
        probe_binary(),
        ProbeKind::Infinite,
        Duration::from_millis(25),
    )
    .unwrap();
    assert!(matches!(result, SupervisedProbe::SupervisorTimedOut));
}
