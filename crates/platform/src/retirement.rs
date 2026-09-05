//! Bind credential retirement to the approved generation rather than arbitrary
//! objects named by a stored checkpoint.
use crate::{Result, StoredTransition, invalid};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn identities(previous: &StoredTransition) -> Result<BTreeMap<String, String>> {
    let generation = &previous.plan.request.next;
    let name = deploy::service_account_name(generation)?;
    if previous.plan.generation_reservation.service_account_name != name {
        return Err(invalid("retirement generation identity changed"));
    }
    select(
        &name,
        &generation.compiled.ir.domain.control_namespace,
        generation
            .compiled
            .capabilities
            .iter()
            .map(|c| c.namespace.as_str()),
        &previous.staged_identity_uids,
    )
}

fn select<'a>(
    name: &str,
    control: &str,
    namespaces: impl Iterator<Item = &'a str>,
    staged: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut expected: BTreeSet<String> = namespaces
        .map(|namespace| format!("rbac.authorization.k8s.io/v1/RoleBinding/{namespace}/{name}"))
        .collect();
    expected.insert(format!("v1/ServiceAccount/{control}/{name}"));
    if staged
        .keys()
        .any(|key| key.contains("/RoleBinding/") && !expected.contains(key))
    {
        return Err(invalid(
            "staged RoleBinding lies outside approved generation",
        ));
    }
    expected
        .into_iter()
        .map(|key| {
            let uid = staged
                .get(&key)
                .filter(|uid| !uid.is_empty())
                .ok_or_else(|| invalid("approved retirement identity UID missing"))?;
            Ok((key, uid.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> BTreeMap<String, String> {
        [
            ("v1/ServiceAccount/control/generation", "sa-uid"),
            (
                "rbac.authorization.k8s.io/v1/RoleBinding/workload/generation",
                "binding-uid",
            ),
            (
                "v1/ServiceAccount/control/generation-process",
                "process-uid",
            ),
        ]
        .into_iter()
        .map(|(key, uid)| (key.into(), uid.into()))
        .collect()
    }

    #[test]
    fn selects_complete_credential_set_excluding_tokenless_process() -> Result<()> {
        let selected = select(
            "generation",
            "control",
            ["workload", "workload"].into_iter(),
            &staged(),
        )?;
        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected.get("v1/ServiceAccount/control/generation"),
            Some(&"sa-uid".into())
        );
        assert!(!selected.contains_key("v1/ServiceAccount/control/generation-process"));
        Ok(())
    }

    #[test]
    fn missing_empty_and_unrelated_identities_reject() {
        for key in [
            "v1/ServiceAccount/control/generation",
            "rbac.authorization.k8s.io/v1/RoleBinding/workload/generation",
        ] {
            let mut missing = staged();
            missing.remove(key);
            assert!(select("generation", "control", ["workload"].into_iter(), &missing).is_err());
            missing.insert(key.into(), String::new());
            assert!(select("generation", "control", ["workload"].into_iter(), &missing).is_err());
        }
        for key in [
            "rbac.authorization.k8s.io/v1/RoleBinding/other/generation",
            "rbac.authorization.k8s.io/v1/RoleBinding/workload/unrelated",
            "other/v1/RoleBinding/workload/generation",
        ] {
            let mut unrelated = staged();
            unrelated.insert(key.into(), "uid".into());
            assert!(
                select(
                    "generation",
                    "control",
                    ["workload"].into_iter(),
                    &unrelated
                )
                .is_err()
            );
        }
    }
}
