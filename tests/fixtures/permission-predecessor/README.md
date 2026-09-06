# Permission upgrade predecessor

This source declares the same domain and ConfigMap identity as the production
example, with a smaller runtime permission set and a distinguishable value.
A qualification runner must build and verify its real controller, install it,
and derive the successor request from its committed state before claiming a
permission-changing upgrade. The source alone does not establish that evidence.

The successor remains the exact production example artifact; do not rewrite its
IR, scope, image, or generated permission plan for the experiment.
