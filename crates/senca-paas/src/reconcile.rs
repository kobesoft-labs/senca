//! 望む状態（出力と、記録した `provider_resource`）と、外部で見つけた資源（タグ付き）を突き合わせて、
//! 何をするか決める純粋な関数。
//!
//! 決まり（REQ-001 の 4、仕様 8.2）:
//! - タグ（`senca:owner`）を持ち、同じクラスタ（`senca:cluster`）の資源だけを扱う。タグの無い資源・
//!   別のクラスタの資源は決して消さない。
//! - 作るときはタグを付けて作り、`provider_resource` に記録する。記録が無いままタグ付きの資源が
//!   見つかれば、同じ owner のものは引き取り（Adopt。二重作成をしない）、持ち主の無いものは消す。
//! - 記録した資源が一覧に見えないときは、すぐ作り直さず `get` で確かめる（一覧の遅れで二重に作らない）。
//! - 作りかけ（MediaLive の security group だけ）は、完成したものが見つかれば引き取り、無ければ消して作り直す。
//!
//! 実行は、その stream の owner が最新の fencing token を持つときだけ行い、同じ出力の create と
//! 並行して走らせないこと（呼び出し側の責任）。

use crate::provider::TaggedResource;
use std::collections::{BTreeMap, BTreeSet};

/// 望む状態の 1 件（連携先・アカウントごとに分けて渡す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub output_id: String,
    /// 無効の出力は資源を残す（作り直さない）が、新しくは作らない。
    pub enabled: bool,
    /// 記録した `provider_resource`。
    pub provider_resource: Option<String>,
    /// 記録が作りかけか（[`crate::PaasProvider`] ごとに決まる。MediaLive の `sg=…` だけのもの）。
    pub partial: bool,
}

/// 消す理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// 持ち主の出力が無い（削除された出力の残り、記録の前に落ちた作成）。
    OutputRemoved,
    /// 持ち主の出力には別の資源が記録・引き取りされている。
    Duplicate,
    /// 作りかけ。
    Partial,
}

/// すること。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 記録どおり存在する。
    Keep { output_id: String, resource_id: String },
    /// 記録が無い（か作りかけの）出力に、タグ付きの完成した資源がある。`provider_resource` に記録する
    /// （送り先は `get` で取る）。
    Adopt { output_id: String, resource_id: String },
    /// 作る（[`crate::PaasProvider::create`]。checkpoint を記録しながら）。
    Create { output_id: String },
    /// 記録した資源が一覧に無い。`get` で確かめ、無ければ記録を消して（有効なら）作る。
    VerifyMissing { output_id: String, resource_id: String },
    /// 消す（[`crate::PaasProvider::delete`]。冪等）。`clear_record_of` があれば、成功後にその出力の
    /// `provider_resource` を消す。
    Delete { resource_id: String, owner: Option<String>, reason: DeleteReason, clear_record_of: Option<String> },
}

fn components(id: &str) -> impl Iterator<Item = &str> {
    id.split(';').filter(|c| !c.is_empty())
}

fn overlaps(a: &str, b: &str) -> bool {
    components(a).any(|x| components(b).any(|y| x == y))
}

/// 突き合わせる。`cluster` は自分のクラスタの識別子（`senca:cluster` タグ。使わないなら `None`）。
/// 結果は、出力ごとの判断（`desired` の順）の後に孤児の削除（資源 ID の順）が並ぶ。
pub fn reconcile(desired: &[Desired], observed: &[TaggedResource], cluster: Option<&str>) -> Vec<Action> {
    let mut obs: Vec<&TaggedResource> = observed.iter().filter(|o| o.cluster.as_deref() == cluster).collect();
    obs.sort_by(|a, b| a.resource_id.cmp(&b.resource_id));
    obs.dedup_by(|a, b| a.resource_id == b.resource_id);
    let mut claimed: BTreeSet<usize> = BTreeSet::new();
    let mut actions = Vec::new();
    let owners: BTreeMap<&str, &Desired> = desired.iter().map(|d| (d.output_id.as_str(), d)).collect();

    // 記録が完成している出力を先に（それらの資源を引き取り候補から外すため）。
    let mut order: Vec<&Desired> = desired.iter().collect();
    order.sort_by_key(|d| match (&d.provider_resource, d.partial) {
        (Some(_), false) => 0,
        (Some(_), true) => 1,
        (None, _) => 2,
    });
    let mut per_output: BTreeMap<&str, Vec<Action>> = BTreeMap::new();

    for d in order {
        let id = d.output_id.as_str();
        let mut out = Vec::new();
        match (&d.provider_resource, d.partial) {
            (Some(rid), false) => {
                let matches: Vec<usize> = (0..obs.len()).filter(|&i| overlaps(&obs[i].resource_id, rid)).collect();
                if matches.is_empty() {
                    out.push(Action::VerifyMissing { output_id: id.into(), resource_id: rid.clone() });
                } else {
                    claimed.extend(matches);
                    out.push(Action::Keep { output_id: id.into(), resource_id: rid.clone() });
                }
            }
            (Some(rid), true) => {
                let done = (0..obs.len())
                    .find(|&i| !claimed.contains(&i) && obs[i].complete && obs[i].owner == id && overlaps(&obs[i].resource_id, rid));
                if let Some(i) = done {
                    claimed.insert(i);
                    out.push(Action::Adopt { output_id: id.into(), resource_id: obs[i].resource_id.clone() });
                } else {
                    // 作りかけの部品（一覧に見えていてもいなくても）を消してから作り直す。
                    claimed.extend((0..obs.len()).filter(|&i| overlaps(&obs[i].resource_id, rid)));
                    out.push(Action::Delete {
                        resource_id: rid.clone(),
                        owner: Some(id.into()),
                        reason: DeleteReason::Partial,
                        clear_record_of: Some(id.into()),
                    });
                    if d.enabled {
                        out.push(Action::Create { output_id: id.into() });
                    }
                }
            }
            (None, _) => {
                let found = (0..obs.len()).find(|&i| !claimed.contains(&i) && obs[i].complete && obs[i].owner == id);
                if let Some(i) = found {
                    claimed.insert(i);
                    out.push(Action::Adopt { output_id: id.into(), resource_id: obs[i].resource_id.clone() });
                } else if d.enabled {
                    out.push(Action::Create { output_id: id.into() });
                }
            }
        }
        per_output.insert(id, out);
    }
    for d in desired {
        if let Some(a) = per_output.remove(d.output_id.as_str()) {
            actions.extend(a);
        }
    }
    for (i, o) in obs.iter().enumerate() {
        if claimed.contains(&i) {
            continue;
        }
        let reason = match owners.get(o.owner.as_str()) {
            None => DeleteReason::OutputRemoved,
            Some(_) if !o.complete => DeleteReason::Partial,
            Some(_) => DeleteReason::Duplicate,
        };
        actions.push(Action::Delete { resource_id: o.resource_id.clone(), owner: Some(o.owner.clone()), reason, clear_record_of: None });
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, rec: Option<&str>) -> Desired {
        Desired { output_id: id.into(), enabled: true, provider_resource: rec.map(Into::into), partial: false }
    }

    fn t(rid: &str, owner: &str) -> TaggedResource {
        TaggedResource { resource_id: rid.into(), owner: owner.into(), cluster: None, complete: true }
    }

    #[test]
    fn create_keep_and_verify() {
        let a = reconcile(&[d("o1", None), d("o2", Some("r2")), d("o3", Some("r3"))], &[t("r2", "o2")], None);
        assert_eq!(
            a,
            vec![
                Action::Create { output_id: "o1".into() },
                Action::Keep { output_id: "o2".into(), resource_id: "r2".into() },
                Action::VerifyMissing { output_id: "o3".into(), resource_id: "r3".into() },
            ]
        );
    }

    #[test]
    fn adopt_after_crash_instead_of_creating_twice() {
        // 作成の応答を記録する前に落ちた: タグ付きの資源が 2 つ（再試行で二重に作られた）
        let a = reconcile(&[d("o1", None)], &[t("rB", "o1"), t("rA", "o1")], None);
        assert_eq!(
            a,
            vec![
                Action::Adopt { output_id: "o1".into(), resource_id: "rA".into() },
                Action::Delete { resource_id: "rB".into(), owner: Some("o1".into()), reason: DeleteReason::Duplicate, clear_record_of: None },
            ]
        );
    }

    #[test]
    fn orphans_deleted_untagged_and_other_cluster_kept() {
        let mut other = t("rX", "gone");
        other.cluster = Some("other".into());
        // 一覧には owner タグ付きのものしか来ない（タグの無いものは list_by_tag が落とす）。別クラスタは残す。
        let a = reconcile(&[d("o1", Some("r1"))], &[t("r1", "o1"), t("r9", "deleted-output"), other], None);
        assert_eq!(
            a,
            vec![
                Action::Keep { output_id: "o1".into(), resource_id: "r1".into() },
                Action::Delete { resource_id: "r9".into(), owner: Some("deleted-output".into()), reason: DeleteReason::OutputRemoved, clear_record_of: None },
            ]
        );
        // 自分のクラスタを指定すると、クラスタ無しのものも別扱い
        let mut mine = t("r5", "zzz");
        mine.cluster = Some("me".into());
        let a = reconcile(&[], &[t("r4", "zzz"), mine], Some("me"));
        assert_eq!(a.len(), 1);
        assert!(matches!(&a[0], Action::Delete { resource_id, .. } if resource_id == "r5"));
    }

    #[test]
    fn duplicate_of_recorded_resource() {
        let a = reconcile(&[d("o1", Some("r1"))], &[t("r1", "o1"), t("r2", "o1")], None);
        assert_eq!(a[1], Action::Delete { resource_id: "r2".into(), owner: Some("o1".into()), reason: DeleteReason::Duplicate, clear_record_of: None });
    }

    #[test]
    fn partial_medialive_records() {
        let partial = Desired { output_id: "o1".into(), enabled: true, provider_resource: Some("sg=5".into()), partial: true };
        // 完成した Input が見つかれば引き取る
        let a = reconcile(std::slice::from_ref(&partial), &[t("input=7;sg=5", "o1")], None);
        assert_eq!(a, vec![Action::Adopt { output_id: "o1".into(), resource_id: "input=7;sg=5".into() }]);
        // 無ければ部品を消して作り直す
        let sg = TaggedResource { resource_id: "sg=5".into(), owner: "o1".into(), cluster: None, complete: false };
        let a = reconcile(std::slice::from_ref(&partial), std::slice::from_ref(&sg), None);
        assert_eq!(
            a,
            vec![
                Action::Delete { resource_id: "sg=5".into(), owner: Some("o1".into()), reason: DeleteReason::Partial, clear_record_of: Some("o1".into()) },
                Action::Create { output_id: "o1".into() },
            ]
        );
        // 記録の無い作りかけは孤児として消す
        let a = reconcile(&[d("o1", None)], &[sg], None);
        assert_eq!(
            a,
            vec![
                Action::Create { output_id: "o1".into() },
                Action::Delete { resource_id: "sg=5".into(), owner: Some("o1".into()), reason: DeleteReason::Partial, clear_record_of: None },
            ]
        );
        // 記録が完成していて、一覧には security group 抜きで見える
        let a = reconcile(&[d("o1", Some("input=7;sg=5"))], &[t("input=7", "o1")], None);
        assert_eq!(a, vec![Action::Keep { output_id: "o1".into(), resource_id: "input=7;sg=5".into() }]);
    }

    #[test]
    fn disabled_outputs_keep_but_do_not_create() {
        let mut x = d("o1", None);
        x.enabled = false;
        assert!(reconcile(std::slice::from_ref(&x), &[], None).is_empty());
        let a = reconcile(&[x], &[t("r1", "o1")], None);
        assert_eq!(a, vec![Action::Adopt { output_id: "o1".into(), resource_id: "r1".into() }]);
    }

    #[test]
    fn recorded_resource_not_adopted_by_other_output() {
        // o2 が記録している r1 に o1 のタグが付いていても、o1 は引き取らない
        let a = reconcile(&[d("o1", None), d("o2", Some("r1"))], &[t("r1", "o1")], None);
        assert_eq!(
            a,
            vec![Action::Create { output_id: "o1".into() }, Action::Keep { output_id: "o2".into(), resource_id: "r1".into() }]
        );
    }
}
