//! 監査ログの読み出し（`GET /audit`）。状態を変更する管理 API の操作の記録を、時刻の新しい順に読む。
//! クラスターではノードが全ノードの分を合わせて返す（答えなかったノードは [`AuditList::unavailable_nodes`]）。
//! 型は openapi.yaml からの生成物（[`crate::models`] の `AuditEntry`・`AuditList`）。

use crate::client::SencaClient;
use crate::error::Result;
use crate::models::{AuditEntry, AuditList};
use crate::pagination::{ListOptions, Paginator};

/// 監査ログの絞り込み。時刻は RFC 3339（`2026-11-03T00:00:00Z`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    /// この時刻以降（含む）。
    pub since: Option<String>,
    /// この時刻より前（含まない）。
    pub until: Option<String>,
    /// 操作の種類。完全一致、`.` か `*` で終われば前方一致（`stream.`）。
    pub action: Option<String>,
    /// 操作した主体（`token:{ID}`・JWT の `sub`）。
    pub subject: Option<String>,
    /// 対象の資源。
    pub resource: Option<String>,
    /// action・subject・resource・request_id のどれかに含まれる文字列（大文字・小文字を区別しない）。
    pub q: Option<String>,
    /// このノード（ID か名前）の記録だけ。
    pub node: Option<String>,
    /// 読むテナント（別のテナント・`*` は node:admin が要る）。
    pub tenant: Option<String>,
}

impl AuditQuery {
    fn apply(&self, mut o: ListOptions) -> ListOptions {
        let fields = [
            ("since", &self.since),
            ("until", &self.until),
            ("action", &self.action),
            ("subject", &self.subject),
            ("resource", &self.resource),
            ("q", &self.q),
            ("node", &self.node),
            ("tenant", &self.tenant),
        ];
        for (k, v) in fields {
            if let Some(v) = v {
                o.filters.push((k.to_string(), v.clone()));
            }
        }
        o
    }
}

/// 監査ログ（`/audit`、読み出しだけ）。
#[derive(Debug, Clone)]
pub struct Audit(pub(crate) SencaClient);

impl Audit {
    /// 一覧（`listAudit`、新しい順、cursor を辿る）。
    pub fn list(&self, q: &AuditQuery) -> Paginator<AuditEntry> {
        self.list_with(q, ListOptions::default())
    }
    pub fn list_with(&self, q: &AuditQuery, opts: ListOptions) -> Paginator<AuditEntry> {
        self.0.op_list("/audit".into(), q.apply(opts))
    }
    /// 1 頁（`unavailable_nodes` つき）。項目が `limit` より少なくても `next_cursor` があれば続きがある（ノードが読む量に上限がある）。
    pub async fn list_page(&self, q: &AuditQuery, opts: &ListOptions) -> Result<AuditList> {
        self.0.op_page_as("/audit".into(), &q.apply(opts.clone())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_become_query_parameters() {
        let q = AuditQuery { action: Some("stream.".into()), since: Some("2026-11-03T00:00:00Z".into()), ..Default::default() };
        let o = q.apply(ListOptions::default().limit(5));
        assert_eq!(o.limit, Some(5));
        assert_eq!(o.filters, vec![("since".to_string(), "2026-11-03T00:00:00Z".to_string()), ("action".to_string(), "stream.".to_string())]);
    }
}
