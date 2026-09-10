//! Review documents are derived from validated IR, never from source callbacks.
use crate::CompiledPlan;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Produce deterministic review documents for a compiled domain.
#[must_use]
pub fn review_documents(plan: &CompiledPlan) -> BTreeMap<&'static str, Value> {
    let ir = &plan.ir;
    let resources: Vec<_> = ir
        .resources
        .iter()
        .map(|r| {
            let mut value = json!({
                "id":r.id,"apiVersion":r.api_version,"kind":r.kind,"namespace":r.namespace,
                "name":r.name,"source":r.source,"lifecycle":r.lifecycle,
            });
            if r.force {
                value["force"] = json!(true);
            }
            value
        })
        .collect();
    let inputs: Vec<_> = ir.inputs.iter().map(|i| json!({
        "id":i.id,"apiVersion":i.api_version,"kind":i.kind,"namespace":i.namespace,
        "name":i.name,"mode":i.mode,"sensitive":i.sensitive,"projection":i.projection,"source":i.source,
    })).collect();
    let edges: Vec<_> = ir
        .reactions
        .iter()
        .flat_map(|r| {
            r.inputs.iter().map(move |input| json!({
        "input":input,"reaction":r.id,"target":r.target,"primitive":r.op,"source":r.source,
    }))
        })
        .collect();
    let ownership: Vec<_> = ir
        .resources
        .iter()
        .map(|r| {
            json!({
                "domain":ir.domain.id,"resource":r.id,"deletionPolicy":r.lifecycle.deletion_policy,
                "fieldManager":format!("apsides-{}",ir.domain.id),
            })
        })
        .collect();
    let apis: BTreeSet<_> = plan
        .capabilities
        .iter()
        .map(|c| {
            (
                c.api_group.clone(),
                c.resource.clone(),
                c.subresource.clone(),
                c.verb.clone(),
            )
        })
        .collect();
    BTreeMap::from([
        (
            "graphs.json",
            json!({"schemaVersion":"apsides.dev/graphs/v1","irDigest":plan.ir_digest,
            "resources":resources,"inputs":inputs,"ownership":ownership,"dataflow":edges,"reactions":ir.reactions}),
        ),
        (
            "capabilities.json",
            json!({"schemaVersion":"apsides.dev/capability-report/v1","irDigest":plan.ir_digest,
            "effects":plan.effects,"requests":plan.request_plan,"capabilities":plan.capabilities,
            "enforcement":ir.domain.enforcement,"enforcementGaps":plan.enforcement_gaps,"indirectRisks":plan.indirect_risks}),
        ),
        (
            "compatibility.json",
            json!({"schemaVersion":"apsides.dev/compatibility/v1","irDigest":plan.ir_digest,
            "schemaLibrary":"k8s-openapi/0.28.0","schemaKubernetesMinor":"1.36","requiredApis":apis,
            "qualifiedCreatePreconditionServers":["v1.34.3"],"fieldValidation":"Strict",
            "transitionPlanSchema":"apsides.dev/transition-plan/v4","runtimeReplicas":1}),
        ),
        (
            "ir-source-map.json",
            json!({"schemaVersion":"apsides.dev/ir-source-map/v1","irDigest":plan.ir_digest,
            "resources":ir.resources.iter().map(|r| (&r.id,&r.source)).collect::<BTreeMap<_,_>>(),
            "inputs":ir.inputs.iter().map(|i| (&i.id,&i.source)).collect::<BTreeMap<_,_>>(),
            "reactions":ir.reactions.iter().map(|r| (&r.id,&r.source)).collect::<BTreeMap<_,_>>()}),
        ),
    ])
}
