# Ownership owner candidate source

This digest-locked production source declares only `ConfigMap production/api-config`
under controller `ownership-owner-production`. It overlaps the ownership contender
candidate on exactly that resource identity so qualification can exercise competing
ownership without broadening either controller's scope.

The source is limited to ownership qualification. Its compiled outputs remain ignored,
and it is not a synthetic fixture promoted into a production candidate.
