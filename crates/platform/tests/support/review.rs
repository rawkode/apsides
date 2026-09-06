use coordination::{ReviewMetadataV1, ReviewRef, ReviewStore};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default)]
pub struct MemoryReviewStore {
    rows: Mutex<BTreeMap<String, ReviewMetadataV1>>,
}

impl MemoryReviewStore {
    pub fn seed(&self, metadata: ReviewMetadataV1) -> coordination::Result<ReviewRef> {
        let reference = metadata.reference()?;
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test review lock"))?;
        if let Some(existing) = rows.get(&reference.digest) {
            if existing != &metadata {
                return Err(coordination::Error::Invalid(
                    "test review immutable conflict",
                ));
            }
        } else {
            rows.insert(reference.digest.clone(), metadata);
        }
        Ok(reference)
    }

    pub fn read(&self, reference: &ReviewRef) -> coordination::Result<Option<ReviewMetadataV1>> {
        reference.validate()?;
        let rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test review lock"))?;
        let Some(metadata) = rows.get(&reference.digest) else {
            return Ok(None);
        };
        if metadata.reference()? != *reference {
            return Err(coordination::Error::Invalid(
                "test review reference mismatch",
            ));
        }
        Ok(Some(metadata.clone()))
    }
}

#[async_trait::async_trait]
impl ReviewStore for MemoryReviewStore {
    async fn put_review(&self, metadata: &ReviewMetadataV1) -> coordination::Result<ReviewRef> {
        self.seed(metadata.clone())
    }

    async fn read_review(
        &self,
        reference: &ReviewRef,
    ) -> coordination::Result<Option<ReviewMetadataV1>> {
        self.read(reference)
    }
}
