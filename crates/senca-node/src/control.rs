//! 制御状態の持ち主。変更は `propose` だけを通す: 写しに適用 → ログに書く → 入れ替え。
//! 書けなければ何も変わらない（500、retryable）。分散（M9）では `propose` を合意に置き換える。

use crate::store::Store;
use senca_control::Problem;
use senca_control::jwt::JwtVerifier;
use senca_control::state::{Command, ControlState};
use std::sync::Arc;
use tokio::sync::broadcast;

/// 適用済みのコマンドの通知（SSE のイベントと、ノードの各部の反映に使う）。
#[derive(Debug, Clone)]
pub struct Applied {
    pub index: u64,
    pub cmd: Arc<Command>,
}

pub struct ControlCore {
    pub state: ControlState,
    store: Option<Store>,
    pub jwt: Option<JwtVerifier>,
    pub applied_tx: broadcast::Sender<Applied>,
}

impl ControlCore {
    pub fn new(state: ControlState, store: Option<Store>, jwt: Option<JwtVerifier>) -> Self {
        let (applied_tx, _) = broadcast::channel(1024);
        ControlCore { state, store, jwt, applied_tx }
    }

    pub fn propose(&mut self, cmd: Command) -> Result<(), Problem> {
        let mut next = self.state.clone();
        next.apply(cmd.clone())?;
        if let Some(store) = self.store.as_mut()
            && let Err(e) = store.append(&cmd, &next)
        {
            eprintln!("control: failed to persist command: {e}");
            return Err(Problem::internal().with_retryable(true));
        }
        self.state = next;
        let _ = self.applied_tx.send(Applied { index: self.state.applied_index, cmd: Arc::new(cmd) });
        Ok(())
    }
}
