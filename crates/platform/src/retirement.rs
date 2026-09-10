//! Bind credential retirement to the approved generation rather than arbitrary
//! objects named by a stored checkpoint.
#[cfg(test)]
use crate::Result;
use crate::WriterCheckpoint;
#[cfg(test)]
use crate::writer_checkpoint::select_retirement_identities as select;
use std::collections::BTreeMap;

pub(crate) const fn identities(previous: &WriterCheckpoint) -> &BTreeMap<String, String> {
    &previous.retirement_identity_uids
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
