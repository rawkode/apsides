//! The expected Pod-template revision, computed independently of the
//! generated controller.
//!
//! This follows the versioned `apsides.dev/rollout/v1` contract directly —
//! canonical JSON of the domain, target, contributing reactions and their
//! input projections, hashed with SHA-256, or HMAC-SHA-256 under the fixture
//! rollout key when any contributing input is sensitive — rather than calling
//! the compiler's rollout evaluator the controller itself runs. Omitting one
//! reaction's contribution must change the result; that is the mutation check.

use crate::{
    Result,
    contract::{ROLLOUT_KEY, ROLLOUT_KEY_ID},
    infrastructure,
};
use compiler_core::{CompiledPlan, InputMode};
use hmac::{Hmac, Mac as _};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// The revision the controller must stamp on `target`, given the live data of
/// its observed inputs. `omitted` drops one reaction's contribution.
pub fn expected_revision(
    plan: &CompiledPlan,
    target: &str,
    omitted: Option<&str>,
    observed: &BTreeMap<String, Value>,
) -> Result<String> {
    let mut reactions = plan
        .ir
        .reactions
        .iter()
        .filter(|reaction| reaction.target == target && Some(reaction.id.as_str()) != omitted)
        .collect::<Vec<_>>();
    reactions.sort_by(|left, right| left.id.cmp(&right.id));
    let input_ids = reactions
        .iter()
        .flat_map(|reaction| reaction.inputs.iter())
        .collect::<BTreeSet<_>>();
    let mut projections = Map::new();
    let mut sensitive = false;
    for id in input_ids {
        let Some(input) = plan.ir.inputs.iter().find(|input| &input.id == id) else {
            return infrastructure(format!("reaction input {id} is not declared"));
        };
        sensitive |= input.sensitive;
        let data = match input.mode {
            InputMode::Managed => plan
                .ir
                .resources
                .iter()
                .find(|resource| input.managed_resource_id.as_ref() == Some(&resource.id))
                .map_or(Value::Null, |resource| resource.desired["data"].clone()),
            InputMode::Observed => observed.get(id).cloned().unwrap_or(Value::Null),
        };
        let data = if data.is_null() { json!({}) } else { data };
        projections.insert(id.clone(), data);
    }
    let mut payload = json!({
        "contract": "apsides.dev/rollout/v1",
        "domain": plan.ir.domain.id,
        "target": target,
        "reactions": reactions.iter().map(|reaction| &reaction.id).collect::<Vec<_>>(),
        "inputs": projections,
    });
    if sensitive {
        payload["keyId"] = json!(ROLLOUT_KEY_ID);
        let mut mac = Hmac::<Sha256>::new_from_slice(ROLLOUT_KEY.as_bytes())
            .map_err(|error| crate::Error::Infrastructure(error.to_string()))?;
        mac.update(&canonical(&payload)?);
        return Ok(format!(
            "hmac-sha256:{ROLLOUT_KEY_ID}:{:x}",
            mac.finalize().into_bytes()
        ));
    }
    Ok(format!("sha256:{:x}", Sha256::digest(canonical(&payload)?)))
}

/// Compact JSON with every object's keys in byte order.
pub fn canonical(value: &Value) -> Result<Vec<u8>> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let ordered: BTreeMap<&String, Value> =
                    map.iter().map(|(key, item)| (key, sorted(item))).collect();
                Value::Object(
                    ordered
                        .into_iter()
                        .map(|(key, item)| (key.clone(), item))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    Ok(serde_json::to_vec(&sorted(value))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn observed() -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("observed-external".to_owned(), json!({"value": "a"})),
            ("observed-credentials".to_owned(), json!({"value": "Yg=="})),
        ])
    }

    #[test]
    fn a_sensitive_contribution_signs_the_revision_with_the_fixture_key() -> Result<()> {
        let plan = testing::plan()?;
        let revision = expected_revision(&plan, "deployment-api", None, &observed())?;
        assert!(
            revision.starts_with("hmac-sha256:fixture-v1:"),
            "{revision}"
        );
        let unsigned = expected_revision(
            &plan,
            "deployment-api",
            Some("rollout-credentials"),
            &observed(),
        )?;
        assert!(unsigned.starts_with("sha256:"), "{unsigned}");
        Ok(())
    }

    #[test]
    fn the_revision_tracks_projected_data_and_every_contribution() -> Result<()> {
        let plan = testing::plan()?;
        let baseline = expected_revision(&plan, "deployment-api", None, &observed())?;
        assert_eq!(
            baseline,
            expected_revision(&plan, "deployment-api", None, &observed())?
        );
        let mut changed = observed();
        changed.insert("observed-external".into(), json!({"value": "b"}));
        assert_ne!(
            baseline,
            expected_revision(&plan, "deployment-api", None, &changed)?
        );
        for reaction in &plan.ir.reactions {
            assert_ne!(
                baseline,
                expected_revision(&plan, "deployment-api", Some(&reaction.id), &observed())?,
                "omitting {} must change the revision",
                reaction.id
            );
        }
        Ok(())
    }

    /// The oracle is separate code, so this pins it to the same contract the
    /// controller's evaluator implements.
    #[test]
    fn the_oracle_agrees_with_the_rollout_contract() -> Result<()> {
        use compiler_core::{InputSnapshot, RolloutKey, RolloutOutcome};
        let plan = testing::plan()?;
        let mut snapshots = observed()
            .into_iter()
            .map(|(id, data)| {
                (
                    id,
                    InputSnapshot::Available {
                        data,
                        authoritative_applied: true,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        snapshots.insert(
            "managed-settings".into(),
            InputSnapshot::Available {
                data: json!({"LEVEL": "info"}),
                authoritative_applied: true,
            },
        );
        let key = RolloutKey {
            id: ROLLOUT_KEY_ID.into(),
            bytes: ROLLOUT_KEY.as_bytes().to_vec(),
        };
        let RolloutOutcome::Ready { revision } =
            compiler_core::evaluate_rollout(&plan, "deployment-api", &snapshots, Some(&key))?
        else {
            return Err(crate::Error::Case("rollout blocked".into()));
        };
        assert_eq!(
            revision,
            expected_revision(&plan, "deployment-api", None, &observed())?
        );
        Ok(())
    }

    #[test]
    fn canonical_json_sorts_nested_keys() -> Result<()> {
        let value = json!({"b": 1, "a": {"d": [2, {"f": 1, "e": 0}], "c": "x"}});
        assert_eq!(
            canonical(&value)?,
            br#"{"a":{"c":"x","d":[2,{"e":0,"f":1}]},"b":1}"#
        );
        Ok(())
    }
}
