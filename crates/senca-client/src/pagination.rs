//! cursor の一覧（`limit`・`cursor` → `items`・`next_cursor`）を 1 件ずつの非同期の列にする。

use futures_core::Stream;
use senca_http::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::client::{Call, SencaClient};
use crate::error::Result;

/// 一覧の 1 頁。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// 次の頁の cursor（最後の頁なら `None`）。
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// 一覧の指定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// 1 頁の件数（1〜1000、既定はサーバーの 100）。
    pub limit: Option<u32>,
    /// ここから（前の頁の `next_cursor`）。
    pub cursor: Option<String>,
    /// 絞り込み（`stream` など、操作が受け付けるもの）。
    pub filters: Vec<(String, String)>,
}

impl ListOptions {
    pub fn limit(mut self, n: u32) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn cursor(mut self, c: impl Into<String>) -> Self {
        self.cursor = Some(c.into());
        self
    }

    /// `stream` で絞る（録画・予約）。
    pub fn stream(mut self, id: impl Into<String>) -> Self {
        self.filters.push(("stream".into(), id.into()));
        self
    }

    /// `room` で絞る（メディアパイプライン）。
    pub fn room(mut self, id: impl Into<String>) -> Self {
        self.filters.push(("room".into(), id.into()));
        self
    }

    pub(crate) fn apply(&self, mut c: Call) -> Call {
        if let Some(l) = self.limit {
            c = c.query("limit", l);
        }
        if let Some(cur) = &self.cursor {
            c = c.query("cursor", cur);
        }
        for (k, v) in &self.filters {
            c = c.query(k, v);
        }
        c
    }
}

type PageFuture<T> = Pin<Box<dyn Future<Output = Result<Page<T>>> + Send>>;

/// 頁を必要なだけ読みながら 1 件ずつ返す列。[`Stream`] を実装する（`futures::StreamExt` で使える）ほか、
/// 追加の依存なしに `while let Some(x) = pager.next().await` でも読める。
///
/// 途中で誤りが出たらそれを 1 回返して終わる。
pub struct Paginator<T> {
    client: SencaClient,
    path: String,
    opts: ListOptions,
    buf: VecDeque<T>,
    next: Option<String>,
    started: bool,
    done: bool,
    fut: Option<PageFuture<T>>,
}

impl<T: DeserializeOwned + Send + 'static> Paginator<T> {
    pub(crate) fn new(client: SencaClient, path: String, opts: ListOptions) -> Self {
        let next = opts.cursor.clone();
        Paginator { client, path, opts, buf: VecDeque::new(), next, started: false, done: false, fut: None }
    }

    /// 次の 1 件（終われば `None`）。
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> Option<Result<T>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

    /// 残りをすべて集める。
    pub async fn collect_all(mut self) -> Result<Vec<T>> {
        let mut out = Vec::new();
        while let Some(x) = self.next().await {
            out.push(x?);
        }
        Ok(out)
    }

    fn fetch(&mut self) -> PageFuture<T> {
        let client = self.client.clone();
        let mut o = self.opts.clone();
        o.cursor = self.next.clone();
        let call = o.apply(Call::new(Method::Get, self.path.clone()));
        Box::pin(async move { client.json::<Page<T>>(call).await.map(|v| v.value) })
    }
}

impl<T> Unpin for Paginator<T> {}

impl<T: DeserializeOwned + Send + 'static> Stream for Paginator<T> {
    type Item = Result<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<T>>> {
        loop {
            if let Some(x) = self.buf.pop_front() {
                return Poll::Ready(Some(Ok(x)));
            }
            if self.done {
                return Poll::Ready(None);
            }
            if self.fut.is_none() {
                if self.started && self.next.is_none() {
                    self.done = true;
                    return Poll::Ready(None);
                }
                self.started = true;
                let f = self.fetch();
                self.fut = Some(f);
            }
            let Some(f) = self.fut.as_mut() else { continue };
            match f.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(r) => {
                    self.fut = None;
                    match r {
                        Ok(p) => {
                            // cursor が進まないなら終える（無限に回らない）。項目が空でも cursor が進めば続ける
                            // （`GET /audit` は、ノードが読む量の上限に達すると空の頁と続きの cursor を返すことがある）
                            if p.next_cursor == self.next {
                                self.next = None;
                            } else {
                                self.next = p.next_cursor;
                            }
                            self.buf.extend(p.items);
                        }
                        Err(e) => {
                            self.done = true;
                            return Poll::Ready(Some(Err(e)));
                        }
                    }
                }
            }
        }
    }
}
