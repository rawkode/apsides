//! What a scenario compares around a step that must change nothing.
//!
//! The protected control objects, every object a deployment could create,
//! and the rows of the metadata database `aps` is given.

use crate::{
    Result,
    objects::{Objects, text},
    pair::{CONTROL_NAMESPACE, WORKLOAD_NAMESPACE},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

/// The namespace registry `ConfigMap`'s UID and resourceVersion.
///
/// # Errors
/// Fails when the registry cannot be read.
pub async fn registry_metadata(objects: &Objects) -> Result<Value> {
    let registry = objects
        .require(
            "v1",
            "ConfigMap",
            Some(CONTROL_NAMESPACE),
            &format!("apsides-registry-{WORKLOAD_NAMESPACE}"),
        )
        .await?;
    Ok(json!({
        "uid": registry["metadata"]["uid"],
        "resourceVersion": registry["metadata"]["resourceVersion"],
    }))
}

/// `kind/namespace/name/uid` of every object a deployment could create in
/// the control and workload namespaces.
///
/// # Errors
/// Fails when a kind cannot be listed.
pub async fn object_inventory(objects: &Objects) -> Result<BTreeSet<String>> {
    let mut entries = BTreeSet::new();
    for namespace in [CONTROL_NAMESPACE, WORKLOAD_NAMESPACE] {
        for (api_version, kind) in [
            ("v1", "ServiceAccount"),
            ("v1", "ConfigMap"),
            ("v1", "Secret"),
            ("v1", "Service"),
            ("apps/v1", "Deployment"),
            ("rbac.authorization.k8s.io/v1", "Role"),
            ("rbac.authorization.k8s.io/v1", "RoleBinding"),
        ] {
            for item in objects
                .list(api_version, kind, Some(namespace), None)
                .await?
            {
                entries.insert(format!(
                    "{kind}/{namespace}/{}/{}",
                    text(&item, "/metadata/name"),
                    text(&item, "/metadata/uid")
                ));
            }
        }
    }
    Ok(entries)
}

/// Each named control-namespace `ConfigMap` in full, keyed by name, with
/// `null` for one that does not exist.
///
/// # Errors
/// Fails when a `ConfigMap` cannot be read.
pub async fn control_objects(objects: &Objects, names: &[String]) -> Result<Value> {
    let mut observed = serde_json::Map::new();
    for name in names {
        let object = objects
            .get("v1", "ConfigMap", Some(CONTROL_NAMESPACE), name)
            .await?;
        observed.insert(
            name.clone(),
            object.map_or(Value::Null, |object| {
                json!({
                    "uid": object["metadata"]["uid"],
                    "resourceVersion": object["metadata"]["resourceVersion"],
                    "digest": crate::digest(object["data"].to_string().as_bytes()),
                })
            }),
        );
    }
    Ok(Value::Object(observed))
}

/// The digest of every table's rows in the metadata database at `path`,
/// by table name. Rows are rendered column by column and sorted, so the
/// digest names the logical content, not the file layout.
///
/// # Errors
/// Fails when the database cannot be opened or read.
pub fn metadata_rows(path: &Path) -> Result<Value> {
    use rusqlite::{Connection, OpenFlags, types::ValueRef};
    let map = |error: rusqlite::Error| {
        crate::Error::Infrastructure(format!("metadata database: {error}"))
    };
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(map)?;
    let tables = {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .map_err(map)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map)?
    };
    let mut observed = serde_json::Map::new();
    for table in tables {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .map_err(map)?;
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| {
                        Ok(match row.get_ref(index)? {
                            ValueRef::Null => "null".to_owned(),
                            ValueRef::Integer(value) => value.to_string(),
                            ValueRef::Real(value) => value.to_string(),
                            ValueRef::Text(bytes) | ValueRef::Blob(bytes) => crate::digest(bytes),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(map)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(map)?;
        rows.sort();
        observed.insert(
            table,
            json!({"rows": rows.len(), "digest": crate::digest(serde_json::to_string(&rows)?.as_bytes())}),
        );
    }
    Ok(Value::Object(observed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_rows_ignore_insertion_order() -> Result {
        let directory = tempfile::tempdir()?;
        let open = |name: &str, rows: &[(i64, &str)]| -> Result<Value> {
            let path = directory.path().join(name);
            let connection = rusqlite::Connection::open(&path)
                .map_err(|error| crate::Error::Infrastructure(error.to_string()))?;
            connection
                .execute_batch("CREATE TABLE claims (id INTEGER, body TEXT)")
                .map_err(|error| crate::Error::Infrastructure(error.to_string()))?;
            for (id, body) in rows {
                connection
                    .execute(
                        "INSERT INTO claims VALUES (?1, ?2)",
                        rusqlite::params![id, body],
                    )
                    .map_err(|error| crate::Error::Infrastructure(error.to_string()))?;
            }
            drop(connection);
            metadata_rows(&path)
        };
        let first = open("a.db", &[(1, "x"), (2, "y")])?;
        assert_eq!(first, open("b.db", &[(2, "y"), (1, "x")])?);
        assert_ne!(first, open("c.db", &[(1, "x"), (2, "z")])?);
        assert_eq!(first["claims"]["rows"], 2);
        Ok(())
    }
}
