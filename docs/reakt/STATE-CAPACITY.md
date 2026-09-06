# State capacity admission

Protected state has explicit admission limits. A domain that cannot fit its
remaining lifecycle evidence is rejected; state is never silently truncated.
Live maximal-profile completion and production release qualification remain
unverified.

## Current contract

Each pending transition carries a protected `CapacityAdmission` outside its
frozen planning inputs. It reserves canonical serialized bytes for the domain's
remaining lifecycle, including the immutable executor credential binding, review
references, retained predecessor state, staged metadata, termination
observations, receipts, and successor checkpoint. Registry loads and
publications account for every pending domain's reservation, so one domain
cannot consume another's reserved space.

Reservation calculates the complete candidate before claim publication.
Registry and individual control objects are limited to 512 KiB. Capacity
admission grants no ownership, deletion, or deployment authority.

Typed evidence profiles bound captured runtime Pods to 64, constrain regular,
init, and ephemeral container totals, bound encoded identity strings, and cap
complete drain evidence at 128 KiB. Before reserving an upgrade, CI verifies the
exact active predecessor and captures its Pod topology twice through Pod,
ReplicaSet, and Deployment ownership. Quiescence captures exact current
identities before freezing the old writer. A profile is a size allowance, not
evidence that a process terminated.

Pending histories allow at most eight recovery resumes and forty receipts.
Lifecycle entry points preserve enough receipt slots for the successful path
and one failure. The pure reducer retains its separate 256-event validation
bound. Exhausted records and irreducible profile growth require explicit
operator recovery; there is no automatic cancellation.

Activation and runtime inventory use separate completion ceilings covering both
predecessor and successor generations. Controller image references are limited
to 4096 ASCII graphic bytes. Activation UIDs and resource versions use a fixed
128-byte canonical JSON string limit, enforced by both deployment and runtime.
An out-of-profile Kubernetes response stops dependent writes.

The owning CI session can monotonically enlarge an untouched Planned or Staged
upgrade. The transition must have no durable drain, and the exact predecessor
must remain Active before and after topology capture. The conditional update
changes only capacity. It does not change claims, allocation, planning inputs,
or evidence. The old session becomes invalid and the verified release must be
bound again before lifecycle work resumes.

Use the same verified artifact and executor for the supported resize path:

```sh
reakt deploy --transition TRANSITION_ID --capacity resize
```

Resize cannot create a transition, change a committed deployment, cancel an
upgrade, or transfer executor ownership.

## Verification boundary

Automated checks cover canonical accounting across registry maps, escaped keys,
maximum counter widths, competing pending domains, forged small ceilings,
receipt trajectories, topology substitution, container drift, retirement UID
tiers, resize session invalidation, Active-state races, conditional-update
conflicts, and no-op resize.

Aggregate overflow is rejected before claim-store publication or Kubernetes
writes. Oversized activation metadata is rejected before inventory creation.
Secret commitments and unexpected staged keys are rejected without writes.

These checks do not establish full maximal-profile end-to-end completion, every
upgrade cleanup payload, live Pod upgrades, persistent-volume reattachment, or
a remote Turso primary. The metadata store remains metadata-only: capacity
records add no manifests, desired values, source, credentials, or hashes of
private contents.
