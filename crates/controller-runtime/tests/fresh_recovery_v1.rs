//! Black-box runtime traces for a fresh retained-watch recovery.
//!
//! The runtime deliberately keeps its observability recorder and command
//! disposition private.  This integration test therefore checks the public
//! result/barrier/mode contract: a terminal watch error must restart the
//! discovery epoch and converge, without surfacing a stale command or a
//! runtime error to the caller.

use async_trait::async_trait;
use oxideloop_controller_api::{ProgramBundleReference, WebApplicationSpec, WebApplicationStatus};
use oxideloop_controller_core::{MachineStateV0, RootSnapshotV0};
use oxideloop_controller_runtime::{
    BarrierV0, FixedPointPortV0, ModeV0, RootActorV0, RuntimeErrorV0,
    observation_token_for_barrier, reconcile_with_watch_restarts,
};
use oxideloop_kube_transport::TransportErrorV0;
use oxideloop_renderer_abi::RenderInputV0;
use oxideloop_renderer_abi::model::{
    ABI_VERSION_V0, Dependency, RenderDisposition, RenderOutputV0, RootProjection,
};

#[derive(Default)]
struct TracePort {
    render_calls: usize,
    observed_calls: usize,
    invalidations: Vec<BarrierV0>,
    barriers: Vec<BarrierV0>,
}

#[async_trait]
impl FixedPointPortV0 for TracePort {
    async fn render(&mut self, _input: &RenderInputV0) -> Result<RenderOutputV0, RuntimeErrorV0> {
        self.render_calls += 1;
        if self.render_calls == 1 {
            return Err(RuntimeErrorV0::Discovery(TransportErrorV0::WatchGone));
        }
        Ok(RenderOutputV0 {
            abi_version: ABI_VERSION_V0.into(),
            disposition: RenderDisposition::ExplicitEmpty,
            resources: vec![],
            dependencies: Vec::<Dependency>::new(),
            automations: vec![],
            diagnostics: vec![],
        })
    }

    async fn observe_after_barrier(
        &mut self,
        _dependencies: &[Dependency],
    ) -> Result<Vec<oxideloop_renderer_abi::model::Observation>, RuntimeErrorV0> {
        self.observed_calls += 1;
        Ok(vec![])
    }

    fn set_barrier(&mut self, barrier: BarrierV0) {
        self.barriers.push(barrier);
    }

    fn watch_invalidated(&mut self, barrier: BarrierV0) {
        self.invalidations.push(barrier);
    }
}

fn actor() -> RootActorV0 {
    RootActorV0::new(
        MachineStateV0::bootstrap(RootSnapshotV0 {
            uid: "root-uid".into(),
            namespace: "demo".into(),
            name: "demo".into(),
            generation: 1,
            spec: WebApplicationSpec {
                message: "hello".into(),
                expose: false,
                program: ProgramBundleReference {
                    config_map_name: "bundle".into(),
                    key: "program.mjs".into(),
                    digest: "a".repeat(64),
                },
            },
            bundle_digest: "a".repeat(64),
            status: WebApplicationStatus::default(),
            state_name: String::new(),
            state_uid: String::new(),
            state_resource_version: String::new(),
            root_resource_version: "1".into(),
            root_finalizer_present: false,
            state_finalizer_present: false,
            deleting: false,
        })
        .expect("valid bootstrap root"),
    )
}

fn input() -> RenderInputV0 {
    RenderInputV0 {
        abi_version: ABI_VERSION_V0.into(),
        root: RootProjection {
            api_version: "demo.oxideloop.dev/v1alpha1".into(),
            kind: "WebApplication".into(),
            namespace: "demo".into(),
            name: "demo".into(),
            uid: "root-uid".into(),
            generation: 1,
            spec: serde_json::to_value(WebApplicationSpec {
                message: "hello".into(),
                expose: false,
                program: ProgramBundleReference {
                    config_map_name: "bundle".into(),
                    key: "program.mjs".into(),
                    digest: "a".repeat(64),
                },
            })
            .expect("root spec"),
        },
        bundle_digest: "a".repeat(64),
        cache_epoch: "0".into(),
        observation_token: "initial-observation".into(),
        observations: vec![],
    }
}

#[tokio::test]
async fn terminal_watch_restarts_on_fresh_epoch_and_converges_without_stale_quiescence() {
    let mut actor = actor();
    let mut port = TracePort::default();
    let original = input();
    let result = reconcile_with_watch_restarts(&mut actor, &mut port, original.clone())
        .await
        .expect("watch terminal should restart and converge");

    assert_eq!(actor.mode(), &ModeV0::Running);
    assert_eq!(
        actor.barrier(),
        BarrierV0 {
            generation: 1,
            cache_epoch: 1
        }
    );
    assert_eq!(result.input().cache_epoch, "1");
    assert_eq!(
        result.input().observation_token,
        observation_token_for_barrier(
            &original.observation_token,
            BarrierV0 {
                generation: 1,
                cache_epoch: 1
            },
        )
    );
    assert_eq!(port.invalidations, vec![actor.barrier()]);
    assert_eq!(
        port.barriers,
        vec![
            BarrierV0 {
                generation: 0,
                cache_epoch: 0
            },
            actor.barrier(),
            actor.barrier()
        ]
    );
    assert_eq!(port.render_calls, 3, "failed pass plus two stable renders");
    assert_eq!(
        port.observed_calls, 1,
        "the restarted fixed point establishes one fresh observation"
    );
}
