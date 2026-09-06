# Permission upgrade predecessor

This source declares the same domain and ConfigMap identity as the production
example, with a smaller runtime permission set and a distinguishable value.
A qualification runner must build and verify its real controller, install it,
and derive the successor request from its committed state before claiming a
permission-changing upgrade. The source alone does not establish that evidence.

The successor remains the exact production example artifact; do not rewrite its
IR, scope, image, or generated permission plan for the experiment.

`cuenv task permissionUpgradeStep` runs one explicit driver action from the
absolute settings path in `APS_PERMISSION_UPGRADE_STEP`. The `verify` action
requires `initialArtifact`, `successorArtifact`, and `trustPolicy` absolute paths
and verifies both signed candidates without Kubernetes access. Other actions
require the owned-cluster marker, dedicated credentials, and local coordination
and inventory stores supplied by the qualification runner. They do not verify
the complete release or publish production evidence.
