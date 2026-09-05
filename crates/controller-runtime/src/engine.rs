//! Small async engine loops which connect actor cache epochs to adapter errors.

use kube_transport::{ConflictProvenanceV0, ResourceKindV0, TransportErrorV0};
use renderer_abi::RenderInputV0;

use crate::{
    FixedPointPortV0, FixedPointResultV0, RootActorV0, RuntimeErrorV0,
    observation_token_for_barrier, reconcile_fixed_point,
};

/// A terminal dependency watch restarts the full list/watch/reread barrier
/// after invalidating only observations. Durable acknowledgements are untouched.
pub async fn reconcile_with_watch_restarts<P: FixedPointPortV0>(
    actor: &mut RootActorV0,
    port: &mut P,
    mut input: RenderInputV0,
) -> Result<FixedPointResultV0, RuntimeErrorV0> {
    loop {
        port.set_barrier(actor.barrier());
        input.cache_epoch = actor.barrier().cache_epoch.to_string();
        match reconcile_fixed_point(port, input.clone()).await {
            Err(RuntimeErrorV0::Discovery(error)) if is_dependency_watch_terminal(&error) => {
                actor.watch_invalidated();
                port.watch_invalidated(actor.barrier());
                port.set_barrier(actor.barrier());
                input.cache_epoch = actor.barrier().cache_epoch.to_string();
                input.observation_token =
                    observation_token_for_barrier(&input.observation_token, actor.barrier());
            }
            result => return result,
        }
    }
}

fn is_dependency_watch_terminal(error: &TransportErrorV0) -> bool {
    matches!(
        error,
        TransportErrorV0::WatchGone | TransportErrorV0::WatchOverflow
    ) || matches!(
        error,
        TransportErrorV0::Conflict(ConflictProvenanceV0::WatchTerminal { resource, .. })
            if matches!(
                resource,
                ResourceKindV0::ConfigMap
                    | ResourceKindV0::Deployment
                    | ResourceKindV0::Service
                    | ResourceKindV0::WebApplication
                    | ResourceKindV0::OxideLoopState
                    | ResourceKindV0::AutomationRun
            )
    )
}
