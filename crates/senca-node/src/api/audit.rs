//! 監査ログの読み出し（`GET /audit`）。
//!
//! - 監査ログは要求を受けたノードの `data_dir/audit.log` にある（[`crate::audit`]）。クラスターでは、各ノードへノード間の
//!   接続（合言葉の相互認証・mTLS）で問い合わせ、時刻の新しい順に合わせて返す。呼び手の資格情報はほかのノードへ送らない。
//!   テナントの絞り込みは、このノードが呼び手の権限から決めて問い合わせに入れる。
//! - ページの位置（`cursor`）はノードごとの読み出しの位置をまとめたもの（base64url の JSON。中身は決まりではない）。
//! - 1 回で読む量には上限がある。上限で止まったノードがあると、`limit` 件に満たなくても `next_cursor` を返す。

use super::{ApiResponse, ApiResult, Ctx};
use crate::audit::{Filter, Page, Pos, SCAN_BUDGET};
use base64::Engine;
use senca_control::{Problem, ProblemCode};
use senca_control::auth::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// ページの位置。`n` はノード ID → 位置（null はそのノードを読み終えた）。`t` は最後に返した行の時刻（このページの後から
/// 加わったノードは、この時刻までの行から読む）。
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct Cursor {
    #[serde(default)]
    t: Option<i64>,
    #[serde(default)]
    n: BTreeMap<String, Option<Pos>>,
}

fn encode_cursor(c: &Cursor) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(c).unwrap_or_default())
}

fn decode_cursor(s: &str) -> Result<Cursor, Problem> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| Problem::invalid("cursor の形が違います"))
}

/// 読む相手のノード。
struct Target {
    id: String,
    name: String,
    /// 自分なら None、ほかのノードなら Raft の番号。
    raft: Option<u64>,
}

/// ノードごとの読み出しの結果。
struct NodePage {
    page: Page,
    /// 返した位置まで使った件数。
    used: usize,
}

pub fn list(ctx: &Ctx) -> ApiResult {
    ctx.require(Scope::TenantAdmin)?;
    let (limit, cursor) = ctx.page()?;
    let mut f = Filter { tenant: Some(ctx.tenant().to_string()), ..Default::default() };
    let mut node_q: Option<String> = None;
    let time = |k: &str, v: &str| {
        senca_core::rfc3339::parse_millis(v).ok_or_else(|| Problem::invalid(format!("{k} は RFC 3339 の時刻にしてください")))
    };
    let text = |k: &str, v: String| if v.is_empty() { Err(Problem::invalid(format!("{k} が空です"))) } else { Ok(Some(v)) };
    for (k, v) in ctx.req.query_pairs() {
        match k.as_str() {
            "since" => f.since_ms = Some(time("since", &v)?),
            "until" => f.until_ms = Some(time("until", &v)?),
            "action" => f.action = text("action", v)?,
            "subject" => f.subject = text("subject", v)?,
            "resource" => f.resource = text("resource", v)?,
            "q" => f.q = text("q", v)?,
            "node" => node_q = text("node", v)?,
            "tenant" => {
                if v != ctx.tenant() {
                    ctx.require(Scope::NodeAdmin)?;
                }
                f.tenant = if v == "*" { None } else { text("tenant", v)? };
            }
            _ => {}
        }
    }
    let cursor = cursor.as_deref().map(decode_cursor).transpose()?;

    // 読む相手
    let me = ctx.app.node_id.clone();
    let cluster = ctx.app.cluster.get().cloned();
    let mut targets: Vec<Target> = {
        let core = ctx.control();
        let name_of = |id: &str| core.state.nodes.get(id).map(|n| n.name.clone()).unwrap_or_else(|| ctx.app.cfg.node_name.clone());
        let mut v = vec![Target { id: me.clone(), name: name_of(&me), raft: None }];
        if let Some(c) = &cluster {
            for n in core.state.nodes.values() {
                if n.id == me {
                    continue;
                }
                if let Some(r) = n.labels.get("raft_id").and_then(|r| r.parse::<u64>().ok()).filter(|r| c.peers.contains_key(r) && *r != c.me) {
                    v.push(Target { id: n.id.clone(), name: n.name.clone(), raft: Some(r) });
                }
            }
        }
        v
    };
    if let Some(nq) = &node_q {
        targets.retain(|t| &t.id == nq || &t.name == nq);
        if targets.is_empty() {
            return Err(Problem::invalid(format!("知らないノードです: {nq}")));
        }
        targets.truncate(1);
    }
    targets.sort_by(|a, b| a.id.cmp(&b.id));

    let ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
    let queries = plan(&ids, cursor.as_ref(), &f);

    // ほかのノードへ先に送ってから、自分の分を読む
    let rt = ctx.app.rt.get().cloned();
    let mut remote = Vec::new();
    if let (Some(c), Some(rt)) = (&cluster, &rt) {
        for (i, g, start) in &queries {
            if let Some(r) = targets[*i].raft {
                let c = c.clone();
                let q = crate::cluster::AuditQuery { filter: g.clone(), start: *start, limit };
                remote.push((*i, rt.spawn(async move { c.audit_query(r, &q).await })));
            }
        }
    }
    let mut pages: BTreeMap<usize, NodePage> = BTreeMap::new();
    for (i, g, start) in &queries {
        if targets[*i].raft.is_none() {
            pages.insert(*i, NodePage { page: ctx.app.audit_log.query(g, *start, limit, SCAN_BUDGET), used: 0 });
        }
    }
    let mut unavailable = Vec::new();
    for (i, h) in remote {
        let r = match &rt {
            Some(rt) => crate::util::block_on(rt, h).unwrap_or_else(|e| Err(e.to_string())),
            None => Err("no runtime".into()),
        };
        match r {
            Ok(p) => {
                pages.insert(i, NodePage { page: p, used: 0 });
            }
            Err(e) => {
                ctx.app.log(&format!("audit: node {} did not answer: {e}", targets[i].id));
                unavailable.push(targets[i].id.clone());
            }
        }
    }

    if node_q.is_some() && !unavailable.is_empty() {
        return Err(Problem::new(ProblemCode::CapacityUnavailable, format!("ノード {} から答えがありません", unavailable[0])).with_retryable(true));
    }

    let (picked, next) = merge(&ids, cursor.as_ref(), &mut pages, limit);
    let items: Vec<Value> = picked
        .into_iter()
        .map(|(i, mut e)| {
            e.insert("node".into(), Value::String(targets[i].id.clone()));
            e.insert("node_name".into(), Value::String(targets[i].name.clone()));
            Value::Object(e)
        })
        .collect();
    let next_cursor = next.map_or(Value::Null, |c| Value::String(encode_cursor(&c)));
    Ok(ApiResponse::json(200, &json!({ "items": items, "next_cursor": next_cursor, "unavailable_nodes": unavailable })))
}

/// ノードごとの問い合わせ（ノードの番号, 条件, 位置）。読み終えたノードは問い合わせない。カーソルに無いノード（後から
/// 加わった・前は答えなかった）は、カーソルの時刻までの行から読む。
fn plan(ids: &[String], cursor: Option<&Cursor>, f: &Filter) -> Vec<(usize, Filter, Option<Pos>)> {
    let mut out = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        match cursor.map(|c| c.n.get(id)) {
            Some(Some(None)) => {}
            Some(Some(Some(p))) => out.push((i, f.clone(), Some(*p))),
            Some(None) => {
                let mut g = f.clone();
                if let Some(ts) = cursor.and_then(|c| c.t) {
                    let upto = ts.saturating_add(1);
                    g.until_ms = Some(g.until_ms.map_or(upto, |u| u.min(upto)));
                }
                out.push((i, g, None));
            }
            None => out.push((i, f.clone(), None)),
        }
    }
    out
}

/// 合わせた行（ノードの番号, 行）。
type Picked = Vec<(usize, Map<String, Value>)>;

/// ノードごとの結果を時刻の新しい順に合わせ、`limit` 件まで取る。(ノードの番号, 行) と次のカーソル（終わりなら None）。
///
/// いちばん新しい先頭を取っていく。読む量の上限で止まったノード（または `limit` 件を返したノード）を使い切ったら、
/// そのノードのまだ読んでいない行より古いものは出さない（順が崩れないように。残りは次のページ）。
fn merge(ids: &[String], cursor: Option<&Cursor>, pages: &mut BTreeMap<usize, NodePage>, limit: usize) -> (Picked, Option<Cursor>) {
    let mut items = Vec::new();
    let mut last_t = cursor.and_then(|c| c.t);
    while items.len() < limit {
        let bound = pages
            .values()
            .filter(|np| np.used == np.page.hits.len() && np.page.next.is_some())
            .map(|np| if np.page.budget_hit { np.page.frontier_ms.unwrap_or(i64::MAX) } else { np.page.hits.last().map_or(i64::MAX, |h| h.ts_ms) })
            .max();
        let best = pages
            .iter()
            .filter(|(_, np)| np.used < np.page.hits.len())
            .max_by(|(ia, a), (ib, b)| a.page.hits[a.used].ts_ms.cmp(&b.page.hits[b.used].ts_ms).then(ib.cmp(ia)))
            .map(|(i, np)| (*i, np.page.hits[np.used].ts_ms));
        let Some((i, ts)) = best else { break };
        if bound.is_some_and(|b| ts < b) {
            break;
        }
        let np = pages.get_mut(&i).expect("page");
        items.push((i, np.page.hits[np.used].entry.clone()));
        np.used += 1;
        last_t = Some(ts);
    }

    let mut next = Cursor { t: last_t, n: BTreeMap::new() };
    let mut more = false;
    for (i, id) in ids.iter().enumerate() {
        let prev = cursor.and_then(|c| c.n.get(id).copied());
        let pos = match pages.get(&i) {
            Some(np) if np.used == np.page.hits.len() => np.page.next,
            Some(np) if np.used > 0 => Some(np.page.hits[np.used - 1].after),
            // 1 件も使わなかった: 前の位置のまま（初めてなら、カーソルに載せずに次もカーソルの時刻から）
            Some(_) => match prev {
                Some(p) => p,
                None => {
                    more = true;
                    continue;
                }
            },
            // 読み終えていた・答えが無かった: 前のまま
            None => match prev {
                Some(p) => p,
                None => continue,
            },
        };
        more |= pos.is_some() && pages.contains_key(&i);
        next.n.insert(id.clone(), pos);
    }
    (items, more.then_some(next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip() {
        let mut c = Cursor { t: Some(5), n: BTreeMap::new() };
        c.n.insert("a".into(), Some(Pos { file: 3, off: 99 }));
        c.n.insert("b".into(), None);
        let s = encode_cursor(&c);
        assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(decode_cursor(&s).unwrap(), c);
        assert!(decode_cursor("!!").is_err());
        assert!(decode_cursor(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"\"x\"")).is_err());
    }

    /// 複数のノードの監査ログを、ハンドラーと同じ手順（plan → 各ノードの query → merge）でページをたどって読む。
    fn read_all(logs: &[&crate::audit::AuditLog], f: &Filter, limit: usize, budget: u64) -> Vec<(usize, String, i64)> {
        let ids: Vec<String> = (0..logs.len()).map(|i| format!("n{i}")).collect();
        let mut cursor: Option<Cursor> = None;
        let mut out = Vec::new();
        for _ in 0..100_000 {
            let mut pages = BTreeMap::new();
            for (i, g, start) in plan(&ids, cursor.as_ref(), f) {
                pages.insert(i, NodePage { page: logs[i].query(&g, start, limit, budget), used: 0 });
            }
            let (items, next) = merge(&ids, cursor.as_ref(), &mut pages, limit);
            assert!(items.len() <= limit);
            for (i, e) in items {
                let ts = senca_core::rfc3339::parse_millis(e["ts"].as_str().unwrap()).unwrap();
                out.push((i, e["request_id"].as_str().unwrap().to_string(), ts));
            }
            match next {
                // 外へ出すときと同じく文字列を通す
                Some(c) => cursor = Some(decode_cursor(&encode_cursor(&c)).unwrap()),
                None => return out,
            }
        }
        panic!("終わらない");
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("senca-audit-merge-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(log: &crate::audit::AuditLog, ts: i64, id: &str, tenant: &str) {
        let l = json!({"action": "stream.create", "request_id": id, "resource": "r", "subject": "token:a", "tenant": tenant, "ts": crate::util::rfc3339(ts)});
        log.append(&l.to_string()).unwrap();
    }

    #[test]
    fn cluster_merge_is_ordered_and_complete() {
        let dirs: Vec<_> = (0..3).map(|i| tmp(&format!("m{i}"))).collect();
        // 1 つ目は小さく回転する
        let logs: Vec<crate::audit::AuditLog> =
            dirs.iter().enumerate().map(|(i, d)| crate::audit::AuditLog::new(d, if i == 0 { 8192 } else { 0 }, 1000)).collect();
        // 時刻をずらして 3 つのノードに書く（ノード 2 はまばら、同じ時刻もある）
        let mut want = Vec::new();
        for k in 0..900i64 {
            let n = match k % 7 {
                0 | 3 | 5 => 0,
                1 | 4 => 1,
                2 => 2,
                _ => {
                    if k % 2 == 0 {
                        0
                    } else {
                        1
                    }
                }
            };
            let ts = 1_700_000_000_000 + k * 10 - (k % 3);
            let tenant = if k % 11 == 0 { "other" } else { "default" };
            write(&logs[n], ts, &format!("r{k}"), tenant);
            if tenant == "default" {
                want.push((n, format!("r{k}"), ts));
            }
        }
        assert!(dirs[0].join("audit.log.1").exists());
        let refs: Vec<&crate::audit::AuditLog> = logs.iter().collect();
        let f = Filter { tenant: Some("default".into()), ..Default::default() };
        for (limit, budget) in [(1000, crate::audit::SCAN_BUDGET), (7, crate::audit::SCAN_BUDGET), (13, 2000), (1, 500)] {
            let got = read_all(&refs, &f, limit, budget);
            // 重複・取りこぼしが無い
            let mut a: Vec<_> = got.iter().map(|x| x.1.clone()).collect();
            let mut b: Vec<_> = want.iter().map(|x| x.1.clone()).collect();
            a.sort();
            b.sort();
            assert_eq!(a, b, "limit={limit} budget={budget}");
            // 新しい順
            assert!(got.windows(2).all(|w| w[0].2 >= w[1].2), "limit={limit} budget={budget}");
            // 行がどのノードのものか
            assert!(got.iter().all(|g| want.iter().any(|w| w.0 == g.0 && w.1 == g.1)));
        }
        // 1 つのノードだけ
        let got = read_all(&refs[2..], &f, 5, crate::audit::SCAN_BUDGET);
        assert_eq!(got.len(), want.iter().filter(|w| w.0 == 2).count());
        for d in dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_node_that_joins_later_starts_at_the_cursor_time() {
        let dirs: Vec<_> = (0..2).map(|i| tmp(&format!("j{i}"))).collect();
        let logs: Vec<crate::audit::AuditLog> = dirs.iter().map(|d| crate::audit::AuditLog::new(d, 0, 1)).collect();
        for k in 0..10i64 {
            write(&logs[(k % 2) as usize], 1_000_000 + k * 1000, &format!("r{k}"), "default");
        }
        let f = Filter::default();
        // 最初のページは n0 だけ（n1 は答えなかった）
        let ids = vec!["n0".to_string()];
        let mut pages = BTreeMap::new();
        for (i, g, start) in plan(&ids, None, &f) {
            pages.insert(i, NodePage { page: logs[i].query(&g, start, 2, crate::audit::SCAN_BUDGET), used: 0 });
        }
        let (items, next) = merge(&ids, None, &mut pages, 2);
        assert_eq!(items.iter().map(|x| x.1["request_id"].as_str().unwrap()).collect::<Vec<_>>(), ["r8", "r6"]);
        let c = next.unwrap();
        // 次のページで n1 が加わる: カーソルの時刻（r6）より新しい r9・r7 は出さない
        let ids = vec!["n0".to_string(), "n1".to_string()];
        let q = plan(&ids, Some(&c), &f);
        assert_eq!(q[1].1.until_ms, Some(1_000_000 + 6000 + 1));
        let mut pages = BTreeMap::new();
        for (i, g, start) in q {
            pages.insert(i, NodePage { page: logs[i].query(&g, start, 100, crate::audit::SCAN_BUDGET), used: 0 });
        }
        let (items, next) = merge(&ids, Some(&c), &mut pages, 100);
        assert_eq!(items.iter().map(|x| x.1["request_id"].as_str().unwrap()).collect::<Vec<_>>(), ["r5", "r4", "r3", "r2", "r1", "r0"]);
        assert!(next.is_none());
        for d in dirs {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
