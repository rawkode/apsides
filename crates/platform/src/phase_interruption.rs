//! Test instrumentation only: pause after an actual, fully validated registry CAS.
use crate::{NamespaceRegistry, Result, Versioned, invalid};
use deploy::Phase;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PauseAfterPhase {
    pub registry_uid: String,
    pub phase: Phase,
}

pub(crate) struct PhasePause {
    target: PauseAfterPhase,
    transition: String,
    fired: AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Checkpoint {
    registry_uid: String,
    registry_resource_version: String,
    transition: String,
    phase: Phase,
}

impl PhasePause {
    pub(crate) fn new(target: PauseAfterPhase, transition: &str) -> Result<Self> {
        if target.registry_uid.is_empty()
            || target.registry_uid.len() > 1024
            || target.registry_uid.chars().any(char::is_control)
            || !crate::valid_name(transition)
            || target.phase == Phase::Blocked
        {
            return Err(invalid("invalid disposable phase checkpoint target"));
        }
        Ok(Self {
            target,
            transition: transition.into(),
            fired: AtomicBool::new(false),
        })
    }

    pub(crate) fn was_triggered(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    fn checkpoint(
        &self,
        previous: &Versioned<NamespaceRegistry>,
        written: &Versioned<NamespaceRegistry>,
    ) -> Option<Checkpoint> {
        if previous.uid != self.target.registry_uid
            || written.uid != self.target.registry_uid
            || written.resource_version.is_empty()
            || written.resource_version.len() > 1024
            || written.resource_version.chars().any(char::is_control)
        {
            return None;
        }
        let stored = written.value.transitions.get(&self.transition)?;
        let old_phase = previous
            .value
            .transitions
            .get(&self.transition)
            .map(|entry| entry.record.phase);
        if stored.record.phase != self.target.phase
            || old_phase == Some(self.target.phase)
            || (old_phase.is_none() && self.target.phase != Phase::Planned)
            || (old_phase.is_some() && self.target.phase == Phase::Planned)
            || self.fired.swap(true, Ordering::SeqCst)
        {
            return None;
        }
        Some(Checkpoint {
            registry_uid: written.uid.clone(),
            registry_resource_version: written.resource_version.clone(),
            transition: self.transition.clone(),
            phase: self.target.phase,
        })
    }

    pub(crate) async fn after_persistence(
        &self,
        previous: &Versioned<NamespaceRegistry>,
        written: &Versioned<NamespaceRegistry>,
    ) {
        if let Some(checkpoint) = self.checkpoint(previous, written) {
            // A failed handshake must not synthesize an Error/Blocked event after
            // the successful CAS. The harness fails on a missing marker and reaps
            // its owned child. This hook never returns after it has fired.
            let _ = emit(&checkpoint);
            std::future::pending::<()>().await;
        }
    }
}

fn emit(checkpoint: &Checkpoint) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(checkpoint).map_err(std::io::Error::other)?;
    if bytes.len() > 8192 {
        return Err(std::io::Error::other(
            "checkpoint metadata exceeds its bound",
        ));
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(b"APSIDES_TEST_CHECKPOINT=")?;
    stdout.write_all(&bytes)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg(test)]
#[path = "phase_interruption_tests.rs"]
mod tests;
