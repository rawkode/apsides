# CNPG ownership collision owner

This controller has the CNPG example's namespace and cluster binding, but a
different domain. Its only resource is `default/api-config`, the one identity it
shares with `examples/cnpg/initial`.

`acceptance-prepare` builds and signs it. The ownership collision scenario
deploys it with `aps deploy`, then requires `aps deploy` of the CNPG initial
release to be refused for the claim collision without changing any state (see
docs/reference/ACCEPTANCE.md).
