use crate::{Result, reject};
use compiler_core::digest;
use serde_json::Value;

/// Validate that a compiler fixture preserves the production contract and that
/// its overlay binds both canonical IR documents.
///
/// # Errors
/// Rejects invalid modes, overlay identities, or changes outside the permitted
/// fixture substitution; propagates canonical serialization failures.
pub fn validate_fixture_projection(
    production: &Value,
    fixture: &Value,
    overlay: &Value,
) -> Result<()> {
    if production.get("testOnly") != Some(&Value::Bool(false))
        || fixture.get("testOnly") != Some(&Value::Bool(true))
        || overlay["schemaVersion"] != "apsides.dev/fixture-overlay/v1"
        || overlay["testOnly"] != true
        || overlay["productionReady"] != false
    {
        return reject("fixture projection requires explicit production and fixture modes");
    }
    let production_digest = digest(production)?;
    let fixture_digest = digest(fixture)?;
    if overlay["productionIrSha256"].as_str() != production_digest.strip_prefix("sha256:")
        || overlay["fixtureIrSha256"].as_str() != fixture_digest.strip_prefix("sha256:")
    {
        return reject("fixture overlay does not bind the production and fixture IR");
    }
    validate_fixture_substitution(production, fixture)
}

fn validate_fixture_substitution(production: &Value, fixture: &Value) -> Result<()> {
    let mut expected = production.clone();
    expected
        .as_object_mut()
        .ok_or_else(|| crate::Error::Rejected("production IR is not an object".into()))?
        .insert("testOnly".into(), Value::Bool(true));
    for resource in expected
        .get_mut("resources")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| crate::Error::Rejected("fixture resources missing".into()))?
    {
        if resource["kind"] == "Deployment" {
            let containers = resource
                .pointer_mut("/desired/spec/template/spec/containers")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| crate::Error::Rejected("fixture containers missing".into()))?;
            for container in containers {
                let container = container.as_object_mut().ok_or_else(|| {
                    crate::Error::Rejected("fixture container is not an object".into())
                })?;
                container.insert(
                    "image".into(),
                    Value::String("apsides-fixture:local".into()),
                );
                container.insert("imagePullPolicy".into(), Value::String("Never".into()));
            }
        }
    }
    if &expected != fixture {
        return reject("fixture substitution changes the approved reaction contract");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_fixture_projection, validate_fixture_substitution};
    use compiler_core::digest;
    use serde_json::json;

    fn projection() -> crate::Result<(serde_json::Value, serde_json::Value, serde_json::Value)> {
        let production = json!({"testOnly":false,"resources":[{
            "kind":"Deployment","desired":{"spec":{"template":{"spec":{
                "containers":[{"name":"api","image":"registry.example/api@sha256:abc"}]
            }}}}
        }]});
        let mut fixture = production.clone();
        fixture["testOnly"] = json!(true);
        fixture["resources"][0]["desired"]["spec"]["template"]["spec"]["containers"][0]["image"] =
            json!("apsides-fixture:local");
        fixture["resources"][0]["desired"]["spec"]["template"]["spec"]["containers"][0]["imagePullPolicy"] =
            json!("Never");
        let overlay = json!({
            "schemaVersion":"apsides.dev/fixture-overlay/v1",
            "testOnly":true,"productionReady":false,
            "productionIrSha256":digest(&production)?.strip_prefix("sha256:"),
            "fixtureIrSha256":digest(&fixture)?.strip_prefix("sha256:")
        });
        Ok((production, fixture, overlay))
    }

    #[test]
    fn projection_requires_exact_modes_digests_and_substitution() -> crate::Result<()> {
        let (production, fixture, overlay) = projection()?;
        validate_fixture_projection(&production, &fixture, &overlay)?;
        for field in [
            "productionIrSha256",
            "fixtureIrSha256",
            "schemaVersion",
            "testOnly",
            "productionReady",
        ] {
            let mut changed = overlay.clone();
            changed[field] = json!(null);
            assert!(validate_fixture_projection(&production, &fixture, &changed).is_err());
        }
        let mut changed = production.clone();
        changed["testOnly"] = json!(null);
        assert!(validate_fixture_projection(&changed, &fixture, &overlay).is_err());
        let mut changed = fixture.clone();
        changed["resources"][0]["desired"]["spec"]["replicas"] = json!(99);
        let mut rebound = overlay.clone();
        rebound["fixtureIrSha256"] = json!(digest(&changed)?.strip_prefix("sha256:"));
        assert!(validate_fixture_projection(&production, &changed, &rebound).is_err());
        assert!(validate_fixture_projection(&fixture, &fixture, &overlay).is_err());
        Ok(())
    }

    #[test]
    fn malformed_fixture_structures_return_errors() {
        for production in [
            json!(null),
            json!([]),
            json!({"resources":{}}),
            json!({"resources":[{"kind":"Deployment","desired":{"spec":[]}}]}),
            json!({"resources":[{"kind":"Deployment","desired":{"spec":{"template":{"spec":{"containers":[null]}}}}}]}),
        ] {
            assert!(validate_fixture_substitution(&production, &json!({})).is_err());
        }
    }
}
