use coordination::{AuditRef, AuditStore, TransitionAuditV1};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default)]
pub struct MemoryAuditStore {
    rows: Mutex<BTreeMap<String, TransitionAuditV1>>,
}

fn key(reference: &AuditRef) -> coordination::Result<String> {
    reference.validate()?;
    compiler_core::digest(&reference.scope)
        .map_err(|_| coordination::Error::Invalid("test audit scope"))
}

#[async_trait::async_trait]
impl AuditStore for MemoryAuditStore {
    async fn put_audit(&self, audit: &TransitionAuditV1) -> coordination::Result<AuditRef> {
        let reference = audit.reference()?;
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test audit lock"))?;
        let entry = rows
            .entry(key(&reference)?)
            .or_insert_with(|| audit.clone());
        if entry != audit {
            return Err(coordination::Error::Invalid(
                "test audit immutable conflict",
            ));
        }
        Ok(reference)
    }

    async fn read_audit(
        &self,
        reference: &AuditRef,
    ) -> coordination::Result<Option<TransitionAuditV1>> {
        let rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test audit lock"))?;
        let Some(audit) = rows.get(&key(reference)?) else {
            return Ok(None);
        };
        if audit.reference()? != *reference {
            return Err(coordination::Error::Invalid(
                "test audit reference mismatch",
            ));
        }
        Ok(Some(audit.clone()))
    }
}
