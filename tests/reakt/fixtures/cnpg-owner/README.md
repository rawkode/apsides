# CNPG ownership collision subject

This controller has the CNPG example's namespace and cluster binding, but a
different domain. Its only resource is `default/api-config`, producing exactly
one overlapping identity for the ownership-reservation qualification.

Build and sign it as a separate production candidate before passing it to
`run_ownership_collision.py` as `--owner-artifact`. The test reserves its claim;
it does not deploy this controller or create its ConfigMap. Generated compilation
and image outputs belong under ignored `work/` paths.
