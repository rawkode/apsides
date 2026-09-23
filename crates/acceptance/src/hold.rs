//! Holds: `ValidatingAdmissionPolicy` objects that stop `aps deploy` at an
//! exact point by denying the deployer's next write there.
//!
//! A registry hold denies any update of the namespace registry by the
//! deployer `ServiceAccount` while the stored registry (`oldObject`) records
//! one transition at one phase. The write that moves the transition into that
//! phase is admitted, because its old object is still at the phase before;
//! every later registry write of that run is denied, so the run cannot move
//! past the phase however it is scheduled. A reservation hold denies every
//! registry update by the deployer. A deletion hold denies the deployer's
//! DELETE of one named object.
//!
//! Admission policies take effect asynchronously. Each hold therefore also
//! matches a probe `ConfigMap` that nothing else writes, denied for everyone:
//! [`Hold::activate`] returns only once a dry-run update of the probe is
//! denied with the hold's message, and [`Hold::release`] only once it is
//! admitted again.

use crate::{Error, Result, bootstrap, objects::Objects, wait};
use deploy::Phase;
use serde_json::{Value, json};
use std::time::Duration;

/// The single policy and binding name; one hold is active at a time.
const NAME: &str = "apsides-acceptance-hold";
/// The probe `ConfigMap`, in the control namespace.
const PROBE: &str = "apsides-acceptance-hold-probe";
/// How long admission may take to pick up a policy change.
const PROPAGATION: Duration = Duration::from_secs(60);

/// An installed hold. It is not removed on drop: a scenario that fails with
/// a hold in place deletes its cluster anyway.
pub struct Hold {
    objects: Objects,
    control: String,
    message: String,
    policy: Value,
}

impl Hold {
    /// Deny every registry update by the deployer while the stored registry
    /// records `transition` at `phase`.
    #[must_use]
    pub fn registry(
        objects: &Objects,
        control: &str,
        workload: &str,
        transition: &str,
        phase: Phase,
    ) -> Self {
        let (policy, message) = registry_policy(control, workload, transition, phase);
        Self {
            objects: objects.clone(),
            control: control.to_owned(),
            message,
            policy,
        }
    }

    /// Deny every registry update by the deployer. A fresh `aps deploy`
    /// stops at its reservation, the first registry write it makes, after
    /// planning and any live preview.
    #[must_use]
    pub fn reservation(objects: &Objects, control: &str, workload: &str) -> Self {
        let (policy, message) = reservation_policy(control, workload);
        Self {
            objects: objects.clone(),
            control: control.to_owned(),
            message,
            policy,
        }
    }

    /// Deny the deployer's DELETE of `name`, a `resource` of `api_group` in
    /// `namespace`.
    #[must_use]
    pub fn deletion(objects: &Objects, control: &str, target: &Target<'_>) -> Self {
        let (policy, message) = deletion_policy(control, target);
        Self {
            objects: objects.clone(),
            control: control.to_owned(),
            message,
            policy,
        }
    }

    /// Install the hold and wait until admission enforces it.
    ///
    /// # Errors
    /// Fails when the policy is refused or does not take effect in time.
    pub async fn activate(&self) -> Result<Value> {
        self.objects.create(&self.policy).await?;
        self.objects
            .create(&json!({
                "apiVersion": "admissionregistration.k8s.io/v1",
                "kind": "ValidatingAdmissionPolicyBinding",
                "metadata": {"name": NAME},
                "spec": {"policyName": NAME, "validationActions": ["Deny"]},
            }))
            .await?;
        self.await_probe(true).await?;
        Ok(json!({"policy": NAME, "message": self.message}))
    }

    /// Remove the hold and wait until admission no longer enforces it.
    ///
    /// # Errors
    /// Fails when the objects cannot be deleted or the hold stays in effect.
    pub async fn release(&self) -> Result {
        for kind in [
            "ValidatingAdmissionPolicyBinding",
            "ValidatingAdmissionPolicy",
        ] {
            self.objects
                .remove("admissionregistration.k8s.io/v1", kind, None, NAME)
                .await?;
        }
        self.await_probe(false).await
    }

    /// Whether `diagnostic` is this hold's denial.
    #[must_use]
    pub fn denied(&self, diagnostic: &str) -> bool {
        diagnostic.contains(&self.message)
    }

    /// Dry-run updates of the probe until they are denied with this hold's
    /// message (`enforced`) or admitted.
    async fn await_probe(&self, enforced: bool) -> Result {
        let probe = match self
            .objects
            .get("v1", "ConfigMap", Some(&self.control), PROBE)
            .await?
        {
            Some(probe) => probe,
            None => {
                self.objects
                    .create(&json!({
                        "apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": PROBE, "namespace": self.control},
                        "data": {"purpose": "acceptance hold probe"},
                    }))
                    .await?
            }
        };
        let label = if enforced {
            "the acceptance hold to take effect"
        } else {
            "the acceptance hold to be lifted"
        };
        wait::until(label, PROPAGATION, async || {
            match self.objects.replace_dry_run(&probe).await {
                Ok(_) => Ok((!enforced).then_some(())),
                Err(Error::Kube(kube::Error::Api(status)))
                    if status.message.contains(&self.message) =>
                {
                    Ok(enforced.then_some(()))
                }
                Err(error) => Err(error),
            }
        })
        .await
    }
}

/// One object a deletion hold protects.
pub struct Target<'a> {
    pub api_group: &'a str,
    pub resource: &'a str,
    pub namespace: &'a str,
    pub name: &'a str,
}

/// The registry hold's policy and denial message.
///
/// The expression matches the phase text and the transition ID in the
/// stored registry's canonical JSON. That identifies this transition's record
/// because a domain has at most one unfinished transition and the registry
/// records a phase nowhere else; the scenario confirms each stop by reading
/// the transition back.
fn registry_policy(
    control: &str,
    workload: &str,
    transition: &str,
    phase: Phase,
) -> (Value, String) {
    let phase = format!("{phase:?}");
    let message = format!("acceptance hold: {transition} stays {phase}");
    let target = format!(
        "request.userInfo.username == '{}' && request.namespace == '{control}' && request.name == 'apsides-registry-{workload}'",
        deployer(control)
    );
    let held = format!(
        "has(oldObject.data) && 'state.json' in oldObject.data \
         && oldObject.data['state.json'].contains('\"phase\":\"{phase}\"') \
         && oldObject.data['state.json'].contains('\"transitionId\":\"{transition}\"')"
    );
    let policy = policy(
        &[rule("", "configmaps", "UPDATE")],
        &format!("{} || ({target})", probe(control)),
        &format!("!{} && !({held})", probe(control)),
        &message,
    );
    (policy, message)
}

/// The reservation hold's policy and denial message.
fn reservation_policy(control: &str, workload: &str) -> (Value, String) {
    let message = format!("acceptance hold: apsides-registry-{workload} is not updated");
    let target = format!(
        "request.userInfo.username == '{}' && request.namespace == '{control}' && request.name == 'apsides-registry-{workload}'",
        deployer(control)
    );
    let policy = policy(
        &[rule("", "configmaps", "UPDATE")],
        &format!("{} || ({target})", probe(control)),
        "false",
        &message,
    );
    (policy, message)
}

/// The deletion hold's policy and denial message.
fn deletion_policy(control: &str, target: &Target<'_>) -> (Value, String) {
    let Target {
        api_group,
        resource,
        namespace,
        name,
    } = target;
    let message = format!("acceptance hold: {resource} {namespace}/{name} is not deleted");
    let matches = format!(
        "request.userInfo.username == '{}' && request.resource.resource == '{resource}' && request.namespace == '{namespace}' && request.name == '{name}'",
        deployer(control)
    );
    let policy = policy(
        &[
            rule("", "configmaps", "UPDATE"),
            rule(api_group, resource, "DELETE"),
        ],
        &format!("{} || ({matches})", probe(control)),
        "false",
        &message,
    );
    (policy, message)
}

/// The deployer `ServiceAccount`'s user name.
fn deployer(control: &str) -> String {
    format!("system:serviceaccount:{control}:{}", bootstrap::DEPLOYER)
}

/// The CEL condition selecting the probe.
fn probe(control: &str) -> String {
    format!("(request.namespace == '{control}' && request.name == '{PROBE}')")
}

fn rule(group: &str, resource: &str, operation: &str) -> Value {
    json!({
        "apiGroups": [group], "apiVersions": ["v1"],
        "operations": [operation], "resources": [resource],
    })
}

fn policy(rules: &[Value], matches: &str, admitted: &str, message: &str) -> Value {
    json!({
        "apiVersion": "admissionregistration.k8s.io/v1",
        "kind": "ValidatingAdmissionPolicy",
        "metadata": {"name": NAME},
        "spec": {
            "failurePolicy": "Fail",
            "matchConstraints": {"resourceRules": rules},
            "matchConditions": [{"name": "hold-target", "expression": matches}],
            "validations": [{"expression": admitted, "message": message}],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::text;

    #[test]
    fn registry_holds_key_on_the_stored_phase_of_one_transition() {
        let (policy, message) =
            registry_policy("apsides-system", "default", "t-2", Phase::Quiesced);
        let spec = &policy["spec"];
        let admitted = text(spec, "/validations/0/expression");
        assert!(
            admitted.contains(r#"oldObject.data['state.json'].contains('"phase":"Quiesced"')"#)
        );
        assert!(admitted.contains(r#"contains('"transitionId":"t-2"')"#));
        let matches = text(spec, "/matchConditions/0/expression");
        assert!(matches.contains("system:serviceaccount:apsides-system:ceiling-deployer"));
        assert!(matches.contains("request.name == 'apsides-registry-default'"));
        assert_eq!(
            spec["matchConstraints"]["resourceRules"][0]["operations"],
            json!(["UPDATE"])
        );
        assert_eq!(message, "acceptance hold: t-2 stays Quiesced");
    }

    #[test]
    fn reservation_holds_deny_every_deployer_registry_update() {
        let (policy, message) = reservation_policy("apsides-system", "default");
        let spec = &policy["spec"];
        assert_eq!(text(spec, "/validations/0/expression"), "false");
        let matches = text(spec, "/matchConditions/0/expression");
        assert!(matches.contains("system:serviceaccount:apsides-system:ceiling-deployer"));
        assert!(matches.contains("request.name == 'apsides-registry-default'"));
        assert!(matches.contains(PROBE));
        assert_eq!(
            message,
            "acceptance hold: apsides-registry-default is not updated"
        );
    }

    #[test]
    fn deletion_holds_deny_only_the_named_object_and_the_probe() {
        let (policy, message) = deletion_policy(
            "apsides-system",
            &Target {
                api_group: "postgresql.cnpg.io",
                resource: "backups",
                namespace: "default",
                name: "cleanup-backup",
            },
        );
        let spec = &policy["spec"];
        assert_eq!(text(spec, "/validations/0/expression"), "false");
        assert_eq!(
            spec["matchConstraints"]["resourceRules"][1],
            json!({"apiGroups": ["postgresql.cnpg.io"], "apiVersions": ["v1"], "operations": ["DELETE"], "resources": ["backups"]})
        );
        let matches = text(spec, "/matchConditions/0/expression");
        assert!(matches.contains("request.name == 'cleanup-backup'"));
        assert!(matches.contains(PROBE));
        assert_eq!(
            message,
            "acceptance hold: backups default/cleanup-backup is not deleted"
        );
    }
}
