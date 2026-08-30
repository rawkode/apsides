# Renderer worker trust boundary

The worker binds only `127.0.0.1`; its loopback listener is not an identity
boundary between arbitrary processes in a shared namespace. The deployment
must run a single trusted controller/worker pair in a pod or otherwise isolate
the network namespace, and one global coordinator owns worker configuration,
startup, restart, and endpoint selection.

The worker treats its evaluator executable path and digest-addressed bundle
store as trusted immutable image/mount inputs. Rust's spawned child receives
only explicit stdin, stdout, and stderr descriptors; production images must
preserve close-on-exec semantics for all other sensitive descriptors and must
not provide the evaluator a Kubernetes token or controller environment. The
anonymous P0 pipes are parent-created capabilities, but they do not provide
network isolation. Network isolation, cgroups, and further OS containment are
deployment/runtime responsibilities.

If the worker cannot confirm that an evaluator direct child is reaped and both
P0 pipe workers are complete before its fixed containment deadline, it exits
with a containment-uncertain failure instead of attempting another request.
The container runtime must terminate remaining processes in that container
namespace before replacing the worker; this is deliberately not treated as a
native worker network-isolation guarantee.
