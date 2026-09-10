use crate::{CompiledPlan, Error, InputMode, Result, canonical_bytes, digest, validate};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{collections::BTreeMap, fmt::Write};

/// Projections are runtime-only values. Do not serialize these into plans,
/// inventory, logging fields, or failure diagnostics.
pub enum InputSnapshot {
    Available {
        data: Value,
        authoritative_applied: bool,
    },
    Missing,
    Unauthorized,
    Unavailable,
    Invalid,
}

/// Provisioned independently in protected control storage. Its contents are
/// intentionally not Debug/Serialize, and never included in an error.
pub struct RolloutKey {
    pub id: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockReason {
    Missing,
    Unauthorized,
    Unavailable,
    Invalid,
    ManagedSourceNotApplied,
    KeyUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RolloutOutcome {
    Ready {
        revision: String,
    },
    Blocked {
        input_id: String,
        reason: BlockReason,
    },
}

/// One revision for all declared rollout inputs of the target. This function
/// has no Kubernetes or mutation capability. Runtime must retain existing
/// targets whenever it returns Blocked.
///
/// # Errors
/// Rejects forged plans, missing target reactions, or invalid serialization.
pub fn evaluate_rollout(
    plan: &CompiledPlan,
    target: &str,
    snapshots: &BTreeMap<String, InputSnapshot>,
    key: Option<&RolloutKey>,
) -> Result<RolloutOutcome> {
    if validate(&plan.ir)? != *plan {
        return Err(Error::Invalid(
            "rollout received a plan inconsistent with trusted derivation".into(),
        ));
    }
    evaluate_validated_rollout(plan, target, snapshots, key)
}

pub(crate) fn evaluate_validated_rollout(
    plan: &CompiledPlan,
    target: &str,
    snapshots: &BTreeMap<String, InputSnapshot>,
    key: Option<&RolloutKey>,
) -> Result<RolloutOutcome> {
    let mut reactions: Vec<_> = plan
        .ir
        .reactions
        .iter()
        .filter(|r| r.target == target)
        .collect();
    if reactions.is_empty() {
        return Err(Error::Invalid(
            "target has no declared rollout reaction".into(),
        ));
    }
    reactions.sort_by_key(|reaction| &reaction.id);
    let mut projections = BTreeMap::new();
    let mut sensitive = false;
    for reaction in &reactions {
        for id in &reaction.inputs {
            if projections.contains_key(id) {
                continue;
            }
            let input = plan
                .ir
                .inputs
                .iter()
                .find(|input| input.id == *id)
                .ok_or_else(|| Error::Invalid("unresolved rollout input".into()))?;
            sensitive |= input.sensitive;
            let blocked = |reason| {
                Ok(RolloutOutcome::Blocked {
                    input_id: id.clone(),
                    reason,
                })
            };
            let snapshot = snapshots.get(id).unwrap_or(&InputSnapshot::Unavailable);
            let data = match snapshot {
                InputSnapshot::Available {
                    data,
                    authoritative_applied,
                } => {
                    if input.mode == InputMode::Managed {
                        if !authoritative_applied {
                            return blocked(BlockReason::ManagedSourceNotApplied);
                        }
                        let resource = plan
                            .ir
                            .resources
                            .iter()
                            .find(|r| Some(&r.id) == input.managed_resource_id.as_ref())
                            .ok_or_else(|| {
                                Error::Invalid("unresolved managed rollout input".into())
                            })?;
                        resource
                            .desired
                            .get("data")
                            .cloned()
                            .unwrap_or_else(|| json!({}))
                    } else {
                        data.clone()
                    }
                }
                InputSnapshot::Missing => return blocked(BlockReason::Missing),
                InputSnapshot::Unauthorized => return blocked(BlockReason::Unauthorized),
                InputSnapshot::Unavailable => return blocked(BlockReason::Unavailable),
                InputSnapshot::Invalid => return blocked(BlockReason::Invalid),
            };
            if crate::schema::string_map(&data, "runtime input projection").is_err()
                || canonical_bytes(&data)?.len() > plan.ir.domain.budgets.max_object_bytes
            {
                return blocked(BlockReason::Invalid);
            }
            if input.kind == "Secret"
                && data
                    .as_object()
                    .ok_or_else(|| {
                        crate::Error::Invalid("input projection must be an object".into())
                    })?
                    .values()
                    .any(|value| {
                        value.as_str().is_none_or(|value| {
                            base64::engine::general_purpose::STANDARD
                                .decode(value)
                                .is_err()
                        })
                    })
            {
                return blocked(BlockReason::Invalid);
            }
            projections.insert(id.clone(), data);
        }
    }
    let reaction_ids: Vec<_> = reactions.iter().map(|reaction| &reaction.id).collect();
    let payload = json!({"contract":"apsides.dev/rollout/v1","domain":plan.ir.domain.id,"target":target,"reactions":reaction_ids,"inputs":projections});
    revision(payload, sensitive, key)
}

fn revision(
    mut payload: Value,
    sensitive: bool,
    key: Option<&RolloutKey>,
) -> Result<RolloutOutcome> {
    if sensitive {
        let Some(key) = key else {
            return Ok(RolloutOutcome::Blocked {
                input_id: "rollout-key".into(),
                reason: BlockReason::KeyUnavailable,
            });
        };
        if key.bytes.len() < 32
            || key.bytes.len() > 4096
            || key.id.is_empty()
            || key.id.len() > 128
            || !key
                .id
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_' | b'.'))
        {
            return Ok(RolloutOutcome::Blocked {
                input_id: "rollout-key".into(),
                reason: BlockReason::KeyUnavailable,
            });
        }
        payload
            .as_object_mut()
            .ok_or_else(|| Error::Invalid("rollout payload is not an object".into()))?
            .insert("keyId".into(), Value::String(key.id.clone()));
        let mut mac = Hmac::<Sha256>::new_from_slice(&key.bytes)
            .map_err(|_| Error::Invalid("rollout key is invalid".into()))?;
        mac.update(&canonical_bytes(&payload)?);
        let bytes = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(64);
        for byte in bytes {
            write!(&mut hex, "{byte:02x}")
                .map_err(|_| Error::Invalid("revision formatting failed".into()))?;
        }
        Ok(RolloutOutcome::Ready {
            revision: format!("hmac-sha256:{}:{hex}", key.id),
        })
    } else {
        Ok(RolloutOutcome::Ready {
            revision: digest(&payload)?,
        })
    }
}
