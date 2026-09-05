# Deployment permissions

Upgrades run through privileged CI with `reakt deploy`. CI owns generation-specific Roles and RoleBindings, controller image deployment, activation and old-writer retirement. The controller receives its compiled, approved reconciliation capabilities; upgrading does not require granting it deployment or RBAC administration powers.

The deployment review's `permissions.deployment.required` field describes Kubernetes access required by the issued deployer for the proposed transition. It includes fixed control-plane objects, generation staging, conditional old-generation retirement, managed-object cleanup and approved adoption. Runtime capabilities remain separately reported. This is a required-access report, not proof of effective RBAC or admission authorization.

Each entry specifies namespace (null means cluster scope), API group, resource/subresource, verb and resource names. Empty resource names mean unrestricted names within that scope. Intended creation names are reported separately under `creationTargets`: Kubernetes cannot constrain top-level create with `resourceNames`. Pod lists and generated Pod, ReplicaSet and Node names also require broader scopes. Executor ownership and UID validation do not narrow an RBAC grant.

The `roleDelegation` entries list capabilities granted to each runtime Role. Kubernetes allows creating a Role when the caller holds its permissions or has `escalate`; binding similarly requires the permissions or `bind`. A privileged CI identity can meet these requirements directly. See the [Kubernetes RBAC authorization documentation](https://kubernetes.io/docs/reference/access-authn-authz/rbac/) for these constraints.

Bootstrap trust discovery, API discovery and credential minting use the invoking identity and are outside this issued-deployer report. Inventory database access requires separate CI credentials. Neither those credentials nor resource values are included in the report. Effective authorization, admission and live upgrade acceptance remain separate verification gates.
