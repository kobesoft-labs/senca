//! 配置の指定（`placement`）の検査と、配置の結果の表示（ADR-0013）。
//!
//! 入力・出力・パイプラインは `placement` で、ノード・GPU の手動の割り当て、ラベル、反親和性、
//! 優先度、置き方、（パイプラインだけ）縮退の段を指定できる。結果は資源の `placement_status` に出る。

use super::Ctx;
use senca_control::model::PlacementSpec;
use senca_control::{Problem, ProblemCode};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_LABELS: usize = 16;
const MAX_LEVELS: usize = 8;

fn bad(t: impl Into<String>) -> Problem {
    Problem::new(ProblemCode::InvalidConfig, t)
}

fn short(s: &str, what: &str) -> Result<(), Problem> {
    if s.is_empty() || s.len() > 128 || s.chars().any(|c| c.is_control()) {
        return Err(bad(format!("{what} は 1〜128 文字で、制御文字を含めないでください")));
    }
    Ok(())
}

/// 検査し、ノードの名前を ID に直したものを返す。`graph` はパイプラインの図（縮退の段はパイプラインだけ）。
pub fn check(ctx: &Ctx, p: Option<PlacementSpec>, graph: Option<&Value>) -> Result<Option<PlacementSpec>, Problem> {
    let Some(mut p) = p else { return Ok(None) };
    if let Some(n) = &p.node {
        short(n, "placement.node")?;
        let core = ctx.control();
        let id = core
            .state
            .nodes
            .values()
            .find(|x| &x.id == n || &x.name == n)
            .map(|x| x.id.clone())
            .ok_or_else(|| bad(format!("ノード {n} がありません")))?;
        p.node = Some(id);
    }
    if let Some(d) = &p.device {
        short(d, "placement.device")?;
    }
    if let Some(a) = &p.anti_affinity {
        short(a, "placement.anti_affinity")?;
    }
    if p.labels.len() > MAX_LABELS {
        return Err(bad(format!("placement.labels は {MAX_LABELS} 個まで")));
    }
    for l in &p.labels {
        let (k, vals): (&str, Vec<&str>) = match l {
            senca_grid::LabelSelector::Eq { key, value } => (key, vec![value]),
            senca_grid::LabelSelector::In { key, values } | senca_grid::LabelSelector::NotIn { key, values } => {
                if values.is_empty() || values.len() > 32 {
                    return Err(bad("placement.labels の values は 1〜32 個"));
                }
                (key, values.iter().map(String::as_str).collect())
            }
        };
        short(k, "ラベルのキー")?;
        for v in vals {
            short(v, "ラベルの値")?;
        }
    }
    if !p.degrade.is_empty() {
        let Some(graph) = graph else { return Err(bad("placement.degrade はパイプラインだけで使えます")) };
        if p.degrade.len() > MAX_LEVELS {
            return Err(bad(format!("placement.degrade は {MAX_LEVELS} 段まで")));
        }
        let ids: BTreeSet<&str> =
            graph.get("nodes").and_then(|n| n.as_array()).into_iter().flatten().filter_map(|n| n.get("id").and_then(|i| i.as_str())).collect();
        let mut labels = BTreeSet::new();
        for l in &p.degrade {
            short(&l.label, "縮退の段の名前")?;
            if l.label == "full" || !labels.insert(l.label.as_str()) {
                return Err(bad(format!("縮退の段の名前 {} は使えないか重複しています（full は段 0 の名前）", l.label)));
            }
            if l.disable.is_empty() {
                return Err(bad(format!("縮退の段 {} の disable が空です", l.label)));
            }
            for d in &l.disable {
                if !ids.contains(d.as_str()) {
                    return Err(bad(format!("縮退の段 {} の disable にある節点 {d} が図にありません", l.label)));
                }
            }
        }
    }
    Ok(Some(p))
}

/// 配置の結果（`placement_status`）。
pub fn status(ctx: &Ctx, workload: &str) -> Value {
    let core = ctx.control();
    let g = &core.state.grid;
    let node_name = |id: &str| core.state.nodes.get(id).map(|n| n.name.clone());
    match (g.workloads.get(workload), g.assignments.get(workload)) {
        (None, _) => json!({ "state": "not_scheduled" }),
        (Some(w), Some(a)) => json!({
            "state": if a.level > 0 { "degraded" } else { "running" },
            "node": a.node,
            "node_name": node_name(&a.node),
            "device": a.device,
            "level": a.level,
            "level_label": w.level_label(a.level),
            "since": crate::util::rfc3339(a.since_ms),
            "reason": a.reason,
        }),
        (Some(_), None) => json!({
            "state": if g.unschedulable.contains_key(workload) { "unschedulable" } else { "pending" },
            "reasons": g.unschedulable.get(workload),
        }),
    }
}
