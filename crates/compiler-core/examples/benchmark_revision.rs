//! Bounded public-input reference for the performance fixture. It uses the
//! trusted evaluator rather than duplicating canonicalization in the harness.
use compiler_core::{
    InputMode, InputSnapshot, RolloutOutcome, evaluate_rollout, parse_and_validate,
};
use serde_json::json;
use std::{collections::BTreeMap, error::Error, io::Read};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 || args[1].len() > 128 || args[1].is_empty() {
        return Err("usage: benchmark_revision IR_PATH PUBLIC_REVISION_VALUE".into());
    }
    let mut encoded = String::new();
    std::fs::File::open(&args[0])?
        .take(1_048_577)
        .read_to_string(&mut encoded)?;
    if encoded.len() > 1_048_576 {
        return Err("benchmark IR exceeds 1 MiB".into());
    }
    let plan = parse_and_validate(&encoded)?;
    if plan.ir.inputs.len() != 1 || plan.ir.reactions.len() != 1 {
        return Err("benchmark reference requires exactly one public input and reaction".into());
    }
    let input = &plan.ir.inputs[0];
    if input.mode != InputMode::Observed || input.kind != "ConfigMap" || input.sensitive {
        return Err("benchmark reference accepts only an observed public ConfigMap".into());
    }
    let snapshots = BTreeMap::from([(
        input.id.clone(),
        InputSnapshot::Available {
            data: json!({"revision": args[1]}),
            authoritative_applied: false,
        },
    )]);
    let target = &plan.ir.reactions[0].target;
    let RolloutOutcome::Ready { revision } = evaluate_rollout(&plan, target, &snapshots, None)?
    else {
        return Err("benchmark reference was unexpectedly blocked".into());
    };
    println!(
        "{}",
        json!({"irDigest": plan.ir_digest, "domainId": plan.ir.domain.id,
        "namespace": plan.ir.domain.namespace, "inputId": input.id, "targetId": target,
        "inputValue": args[1], "revision": revision})
    );
    Ok(())
}
