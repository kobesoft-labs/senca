//! SSE（`GET /api/v1/events`）を型付きの非同期の列にする。切れたら `Last-Event-ID` を付けてつなぎ直す。
//!
//! ノードは再送の保持を持たないので、つなぎ直すと最初に `resync` が来ることがある（[`ServerEvent::Resync`]）。
//! 受け手は一覧を読み直して状態を合わせる。

use futures_core::Stream;
use senca_http::Method;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::client::{Call, SencaClient};
use crate::error::{ApiError, Error, Result};
use crate::models::SseEventData;

/// 購読の指定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventsOptions {
    /// この種類だけ（`stream.created`、末尾の `*` は前方一致: `stream.*`）。空なら全部。
    /// サーバーは絞り込みを持たないので、クライアントで捨てる。`resync` は常に渡す。
    pub types: Vec<String>,
    /// ここから（前に受けた最後の `id`）。
    pub last_event_id: Option<String>,
    /// 切れたらつなぎ直す（既定 true）。401・403 ではつなぎ直さない。
    pub reconnect: bool,
    /// つなぎ直しの待ちの最初と上限（倍々）。
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for EventsOptions {
    fn default() -> Self {
        EventsOptions {
            types: Vec::new(),
            last_event_id: None,
            reconnect: true,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl EventsOptions {
    pub fn types<I: IntoIterator<Item = S>, S: Into<String>>(mut self, t: I) -> Self {
        self.types = t.into_iter().map(Into::into).collect();
        self
    }

    pub fn last_event_id(mut self, id: impl Into<String>) -> Self {
        self.last_event_id = Some(id.into());
        self
    }

    fn wants(&self, ty: &str) -> bool {
        self.types.is_empty()
            || self.types.iter().any(|p| match p.strip_suffix('*') {
                Some(prefix) => ty.starts_with(prefix),
                None => p == ty,
            })
    }
}

/// 管理 API のイベント。
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    /// SSE の `id`（制御ログの位置）。`Last-Event-ID` に使う。
    pub id: Option<String>,
    /// SSE の `event`（`stream.created` など）。
    pub event: String,
    pub data: SseEventData,
}

/// 列に流れるもの。
#[derive(Debug, Clone, PartialEq)]
pub enum ServerEvent {
    Event(SseEvent),
    /// 取りこぼしがありうる。一覧を読み直して合わせること。
    Resync { id: Option<String> },
}

/// SSE の 1 件（解析しただけのもの）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawSse {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

/// text/event-stream の解析器（HTML の仕様の規則: CRLF・LF・CR、`:` のコメント、`data` の複数行）。
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
    id: Option<String>,
    /// 最後に見た `id`（イベントの境目で確定）。
    pub last_id: Option<String>,
    /// サーバーが勧めたつなぎ直しの待ち（ミリ秒）。
    pub retry_ms: Option<u64>,
    saw_cr: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// バイトを足し、まとまったイベントを返す。
    pub fn feed(&mut self, data: &[u8]) -> Vec<RawSse> {
        let mut out = Vec::new();
        for &b in data {
            if self.saw_cr {
                self.saw_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\r' => {
                    self.saw_cr = true;
                    self.line(&mut out);
                }
                b'\n' => self.line(&mut out),
                _ => self.buf.push(b),
            }
        }
        out
    }

    fn line(&mut self, out: &mut Vec<RawSse>) {
        let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
        if line.is_empty() {
            if let Some(id) = self.id.take() {
                self.last_id = Some(id);
            }
            if !self.data.is_empty() || !self.event.is_empty() {
                out.push(RawSse {
                    event: if self.event.is_empty() { "message".into() } else { std::mem::take(&mut self.event) },
                    data: self.data.join("\n"),
                    id: self.last_id.clone(),
                });
            }
            self.event.clear();
            self.data.clear();
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            "id" if !value.contains('\0') => self.id = Some(value.to_string()),
            "retry" => {
                if let Ok(n) = value.parse() {
                    self.retry_ms = Some(n);
                }
            }
            _ => {}
        }
    }
}

/// イベントの列（[`Stream`]、または `while let Some(e) = events.next().await`）。落とすと購読をやめる。
pub struct EventStream {
    rx: mpsc::Receiver<Result<ServerEvent>>,
    task: tokio::task::JoinHandle<()>,
}

impl EventStream {
    /// 次のイベント（購読が終われば `None`）。
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> Option<Result<ServerEvent>> {
        self.rx.recv().await
    }

    /// 購読をやめる。
    pub fn close(self) {}
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Stream for EventStream {
    type Item = Result<ServerEvent>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

pub(crate) fn start(client: SencaClient, opts: EventsOptions) -> EventStream {
    let (tx, rx) = mpsc::channel(256);
    let task = tokio::spawn(run(client, opts, tx));
    EventStream { rx, task }
}

async fn run(client: SencaClient, opts: EventsOptions, tx: mpsc::Sender<Result<ServerEvent>>) {
    let mut last_id = opts.last_event_id.clone();
    let mut backoff = opts.initial_backoff;
    loop {
        let mut extra = vec![];
        if let Some(id) = &last_id {
            extra.push(("Last-Event-ID".to_string(), id.clone()));
        }
        let mut call = Call::new(Method::Get, "/events");
        call.accept = "text/event-stream";
        let mut retry_hint = None;
        match client.open(&call, &extra, client.inner.timeout).await {
            Ok(resp) if resp.status == 200 => {
                let mut body = resp.body;
                let mut parser = SseParser::new();
                loop {
                    match body.chunk().await {
                        Ok(Some(c)) => {
                            for raw in parser.feed(&c) {
                                backoff = opts.initial_backoff;
                                if raw.id.is_some() {
                                    last_id = raw.id.clone();
                                }
                                let item = if raw.event == "resync" {
                                    Some(Ok(ServerEvent::Resync { id: raw.id }))
                                } else if opts.wants(&raw.event) {
                                    Some(
                                        serde_json::from_str::<SseEventData>(&raw.data)
                                            .map(|data| ServerEvent::Event(SseEvent { id: raw.id, event: raw.event.clone(), data }))
                                            .map_err(|e| Error::Decode(format!("event {}: {e}", raw.event))),
                                    )
                                } else {
                                    None
                                };
                                if let Some(it) = item
                                    && tx.send(it).await.is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            if !opts.reconnect && tx.send(Err(e)).await.is_err() {
                                return;
                            }
                            break;
                        }
                    }
                }
                retry_hint = parser.retry_ms.map(Duration::from_millis);
            }
            Ok(resp) => {
                let status = resp.status;
                let headers = resp.headers.clone();
                let b = resp.read_all(64 * 1024).await.unwrap_or_default();
                let e = ApiError::from_response(status, &headers, &b);
                let fatal = matches!(status, 401 | 403 | 404) || !e.is_retryable();
                if tx.send(Err(e.into())).await.is_err() || fatal {
                    return;
                }
            }
            Err(e) => {
                if !opts.reconnect {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
        if !opts.reconnect || tx.is_closed() {
            return;
        }
        let wait = retry_hint.unwrap_or(backoff);
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(opts.max_backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_stream() {
        let mut p = SseParser::new();
        let mut evs = p.feed(b": keepalive\n\nevent: stream.created\nid: 4");
        assert!(evs.is_empty());
        evs.extend(p.feed(b"2\r\ndata: {\"a\":\r\ndata: 1}\r\n\r\n"));
        assert_eq!(evs, vec![RawSse { event: "stream.created".into(), data: "{\"a\":\n1}".into(), id: Some("42".into()) }]);
        let evs = p.feed(b"event: resync\rid: 50\rdata: {}\r\rretry: 1500\n\ndata:x\n\n");
        assert_eq!(evs[0], RawSse { event: "resync".into(), data: "{}".into(), id: Some("50".into()) });
        // id の無いイベントは前の id を引き継ぐ
        assert_eq!(evs[1], RawSse { event: "message".into(), data: "x".into(), id: Some("50".into()) });
        assert_eq!(p.retry_ms, Some(1500));
    }

    #[test]
    fn type_filter() {
        let o = EventsOptions::default().types(["stream.*", "node.updated"]);
        assert!(o.wants("stream.created") && o.wants("node.updated"));
        assert!(!o.wants("recording.updated"));
        assert!(EventsOptions::default().wants("anything"));
    }
}
