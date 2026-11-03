//! 制御状態と、それを変えるコマンド。
//!
//! 規則:
//! - 状態を変えるのは [`ControlState::apply`] だけ。時刻・乱数・ID はコマンドに入れて渡す（決定的に保つ）。
//! - `apply` は不変条件（存在・世代・上限・最後の管理者）を自分でも検査する。提案した時点の状態が古くても、
//!   適用の時点で誤りにならないようにするため（分散で同じコマンドが複数のノードで適用される）。
//! - 失敗したコマンドは状態を変えない。

use crate::auth::Scope;
use crate::model::*;
use crate::problem::{Problem, ProblemCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_STREAMS_PER_TENANT: usize = 1000;
pub const MAX_TOKENS_PER_TENANT: usize = 100;
pub const MAX_SCHEDULES_PER_TENANT: usize = 1000;
pub const MAX_ROOMS_PER_TENANT: usize = 10_000;
pub const IDEMPOTENCY_TTL_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlState {
    #[serde(default)]
    pub pipelines: BTreeMap<String, Pipeline>,
    #[serde(default)]
    pub streams: BTreeMap<String, Stream>,
    #[serde(default)]
    pub tokens: BTreeMap<String, ApiToken>,
    #[serde(default)]
    pub recordings: BTreeMap<String, Recording>,
    #[serde(default)]
    pub schedules: BTreeMap<String, Schedule>,
    /// キーは `tenant\u{0}key`。
    #[serde(default)]
    pub idempotency: BTreeMap<String, IdempotencyRecord>,
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeInfo>,
    #[serde(default)]
    pub inputs: BTreeMap<String, Input>,
    #[serde(default)]
    pub outputs: BTreeMap<String, Output>,
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretEntry>,
    /// stream → 所有者（送り手を受けているノード）。
    #[serde(default)]
    pub owners: BTreeMap<String, Ownership>,
    /// 署名付き再生 URL の鍵（16 進）。作り直すと発行済みの URL がすべて無効になる。
    #[serde(default)]
    pub playback_key: Option<String>,
    /// 適用したコマンドの数（ログの位置）。
    #[serde(default)]
    pub applied_index: u64,
    /// 所有権の世代（fencing token）を払い出す通し番号（仕様 8.2）。
    #[serde(default)]
    pub fencing_counter: u64,
    /// グリッド（ノードの資源・仕事・割り当て。ADR-0013）。仕事は入力・出力・パイプライン・部屋から導く。
    #[serde(default)]
    pub grid: senca_grid::GridState,
    /// リアルタイムの部屋（ADR-0015）。
    #[serde(default)]
    pub rooms: BTreeMap<String, Room>,
    /// 部屋のメディアパイプライン（ADR-0016）。
    #[serde(default)]
    pub media_pipelines: BTreeMap<String, MediaPipeline>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    CreateStream { stream: Stream },
    /// 世代 `expect_generation` のときだけ置き換える（If-Match）。新しい世代は +1。
    UpdateStream { stream: Stream, expect_generation: u64 },
    DeleteStream { tenant: String, id: String, expect_generation: Option<u64> },
    CreateToken { token: ApiToken },
    DeleteToken { tenant: String, id: String },
    PutRecording { recording: Recording },
    DeleteRecording { tenant: String, id: String },
    CreateSchedule { schedule: Schedule },
    UpdateSchedule { schedule: Schedule, expect_generation: u64 },
    DeleteSchedule { tenant: String, id: String, expect_generation: Option<u64> },
    /// 予約の実行結果（状態と run の追記・更新）。世代は変えない（利用者の更新と競合させない）。
    RecordScheduleRun { tenant: String, id: String, state: ScheduleState, run: ScheduleRun },
    PutIdempotency { record: IdempotencyRecord },
    ExpireIdempotency { before_ms: i64 },
    SetPlaybackKey { key_hex: String },
    PutNode { node: NodeInfo },
    PutInput { input: Input },
    DeleteInput { tenant: String, id: String },
    PutOutput { output: Output },
    DeleteOutput { tenant: String, id: String },
    PutSecret { secret: SecretEntry },
    PutPipeline { pipeline: Pipeline, expect_generation: Option<u64> },
    DeletePipeline { tenant: String, id: String },
    /// stream の所有権を取る（新しい fencing token を払い出す。古い所有者は送り手を切る）。
    ClaimStream { stream: String, node: String, now_ms: i64 },
    /// 所有権を手放す（token が今のものと同じときだけ）。
    ReleaseStream { stream: String, token: u64 },
    DeleteSecret { tenant: String, id: String },
    /// 新しい fencing token を払い出す（結果は `fencing_counter`）。
    NextFencingToken,
    /// グリッドのコマンド（ノードの登録・停止・装置の故障・配置係の決定）。資源から導く仕事は
    /// ここでは受け付けない（入力・出力・パイプラインのコマンドで変わる）。
    Grid { cmd: senca_grid::GridCommand },
    /// 部屋を作る・置き換える（`expect_generation` があれば If-Match）。
    PutRoom { room: Room, expect_generation: Option<u64> },
    DeleteRoom { tenant: String, id: String },
    PutMediaPipeline { pipeline: MediaPipeline, expect_generation: Option<u64> },
    DeleteMediaPipeline { tenant: String, id: String },
}

fn conflict(t: &str) -> Problem {
    Problem::new(ProblemCode::Conflict, t)
}

fn stale() -> Problem {
    Problem::new(ProblemCode::PreconditionFailed, "資源が更新されています（If-Match が古い）")
}

pub fn idem_key(tenant: &str, key: &str) -> String {
    format!("{tenant}\u{0}{key}")
}

impl ControlState {
    pub fn apply(&mut self, cmd: Command) -> Result<(), Problem> {
        self.apply_inner(cmd)?;
        self.applied_index += 1;
        Ok(())
    }

    fn apply_inner(&mut self, cmd: Command) -> Result<(), Problem> {
        let derives = matches!(
            cmd,
            Command::DeleteStream { .. }
                | Command::PutInput { .. }
                | Command::DeleteInput { .. }
                | Command::PutOutput { .. }
                | Command::DeleteOutput { .. }
                | Command::PutPipeline { .. }
                | Command::DeletePipeline { .. }
                | Command::PutRoom { .. }
                | Command::DeleteRoom { .. }
                | Command::PutMediaPipeline { .. }
                | Command::DeleteMediaPipeline { .. }
        );
        self.apply_resource(cmd)?;
        if derives {
            self.sync_workloads();
        }
        Ok(())
    }

    /// 資源から導いた仕事を `grid.workloads` に反映する（差だけ。決定的）。
    fn sync_workloads(&mut self) {
        let want = crate::workloads::derive(self);
        let stale: Vec<String> =
            self.grid.workloads.keys().filter(|id| crate::workloads::is_derived(id) && !want.contains_key(*id)).cloned().collect();
        for id in stale {
            let _ = self.grid.apply(senca_grid::GridCommand::DeleteWorkload { id });
        }
        for (id, w) in want {
            if self.grid.workloads.get(&id) != Some(&w) {
                // 導いた仕事は検査を通る形で作る（テナント・ID は空でない、段の名前は API で検査済み）
                let _ = self.grid.apply(senca_grid::GridCommand::PutWorkload { workload: w });
            }
        }
    }

    fn apply_resource(&mut self, cmd: Command) -> Result<(), Problem> {
        match cmd {
            Command::CreateStream { stream } => {
                if self.streams.contains_key(&stream.id) {
                    return Err(conflict("同じ ID の stream があります"));
                }
                if self.streams.values().filter(|s| s.tenant == stream.tenant).count() >= MAX_STREAMS_PER_TENANT {
                    return Err(Problem::new(ProblemCode::QuotaExceeded, "stream の数が上限です"));
                }
                if self.streams.values().any(|s| s.tenant == stream.tenant && s.name == stream.name) {
                    return Err(conflict("同じ名前の stream があります"));
                }
                self.streams.insert(stream.id.clone(), stream);
            }
            Command::UpdateStream { mut stream, expect_generation } => {
                let cur = self.tenant_stream(&stream.tenant, &stream.id)?;
                if cur.generation != expect_generation {
                    return Err(stale());
                }
                if cur.name != stream.name
                    && self.streams.values().any(|s| s.tenant == stream.tenant && s.name == stream.name && s.id != stream.id)
                {
                    return Err(conflict("同じ名前の stream があります"));
                }
                stream.generation = expect_generation + 1;
                stream.created_at_ms = cur.created_at_ms;
                self.streams.insert(stream.id.clone(), stream);
            }
            Command::DeleteStream { tenant, id, expect_generation } => {
                let cur = self.tenant_stream(&tenant, &id)?;
                if let Some(g) = expect_generation
                    && cur.generation != g
                {
                    return Err(stale());
                }
                self.streams.remove(&id);
                // 予約・入力・出力は stream と一緒に消す（録画は消さない。仕様 10.2）。
                self.schedules.retain(|_, s| s.stream != id);
                self.inputs.retain(|_, i| i.stream != id);
                self.outputs.retain(|_, o| o.stream != id);
                self.owners.remove(&id);
            }
            Command::CreateToken { token } => {
                if self.tokens.contains_key(&token.id) {
                    return Err(conflict("同じ ID のトークンがあります"));
                }
                if self.tokens.values().filter(|t| t.tenant == token.tenant).count() >= MAX_TOKENS_PER_TENANT {
                    return Err(Problem::new(ProblemCode::QuotaExceeded, "トークンの数が上限です"));
                }
                self.tokens.insert(token.id.clone(), token);
            }
            Command::DeleteToken { tenant, id } => {
                let t = self.tokens.get(&id).filter(|t| t.tenant == tenant).ok_or_else(Problem::not_found)?;
                if t.scopes.contains(&Scope::TenantAdmin) {
                    let admins = self
                        .tokens
                        .values()
                        .filter(|x| x.tenant == tenant && x.scopes.contains(&Scope::TenantAdmin))
                        .count();
                    if admins <= 1 {
                        return Err(conflict("テナントの最後の管理者トークンは消せません"));
                    }
                }
                self.tokens.remove(&id);
            }
            Command::PutRecording { recording } => {
                if let Some(cur) = self.recordings.get(&recording.id)
                    && cur.tenant != recording.tenant
                {
                    return Err(conflict("録画のテナントが違います"));
                }
                self.recordings.insert(recording.id.clone(), recording);
            }
            Command::DeleteRecording { tenant, id } => {
                let r = self.recordings.get(&id).filter(|r| r.tenant == tenant).ok_or_else(Problem::not_found)?;
                if r.legal_hold {
                    return Err(conflict("法的保全中の録画は消せません"));
                }
                self.recordings.remove(&id);
            }
            Command::CreateSchedule { schedule } => {
                if self.schedules.contains_key(&schedule.id) {
                    return Err(conflict("同じ ID の予約があります"));
                }
                self.tenant_stream(&schedule.tenant, &schedule.stream)?;
                if self.schedules.values().filter(|s| s.tenant == schedule.tenant).count() >= MAX_SCHEDULES_PER_TENANT {
                    return Err(Problem::new(ProblemCode::QuotaExceeded, "予約の数が上限です"));
                }
                self.schedules.insert(schedule.id.clone(), schedule);
            }
            Command::UpdateSchedule { mut schedule, expect_generation } => {
                let cur = self
                    .schedules
                    .get(&schedule.id)
                    .filter(|s| s.tenant == schedule.tenant)
                    .ok_or_else(Problem::not_found)?;
                if cur.generation != expect_generation {
                    return Err(stale());
                }
                schedule.generation = expect_generation + 1;
                schedule.created_at_ms = cur.created_at_ms;
                schedule.runs = cur.runs.clone();
                self.schedules.insert(schedule.id.clone(), schedule);
            }
            Command::DeleteSchedule { tenant, id, expect_generation } => {
                let cur = self.schedules.get(&id).filter(|s| s.tenant == tenant).ok_or_else(Problem::not_found)?;
                if let Some(g) = expect_generation
                    && cur.generation != g
                {
                    return Err(stale());
                }
                self.schedules.remove(&id);
            }
            Command::RecordScheduleRun { tenant, id, state, run } => {
                let s = self
                    .schedules
                    .get_mut(&id)
                    .filter(|s| s.tenant == tenant)
                    .ok_or_else(Problem::not_found)?;
                s.state = state;
                match s.runs.iter_mut().find(|r| r.occurrence == run.occurrence) {
                    Some(r) => *r = run,
                    None => {
                        s.runs.push(run);
                        // 履歴は直近 100 件まで
                        let n = s.runs.len();
                        if n > 100 {
                            s.runs.drain(..n - 100);
                        }
                    }
                }
            }
            Command::PutIdempotency { record } => {
                let k = idem_key(&record.tenant, &record.key);
                if let Some(cur) = self.idempotency.get(&k)
                    && cur.fingerprint != record.fingerprint
                {
                    return Err(Problem::new(ProblemCode::IdempotencyConflict, "同じ Idempotency-Key で別の要求です"));
                }
                self.idempotency.insert(k, record);
            }
            Command::ExpireIdempotency { before_ms } => {
                self.idempotency.retain(|_, r| r.created_at_ms >= before_ms);
            }
            Command::SetPlaybackKey { key_hex } => {
                if key_hex.len() != 64 || !key_hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Problem::invalid("鍵の形が違います"));
                }
                self.playback_key = Some(key_hex);
            }
            Command::PutNode { node } => {
                self.nodes.insert(node.id.clone(), node);
            }
            Command::PutInput { input } => {
                self.tenant_stream(&input.tenant, &input.stream)?;
                if let Some(c) = self.inputs.get(&input.id)
                    && c.tenant != input.tenant
                {
                    return Err(conflict("入力のテナントが違います"));
                }
                if !self.inputs.contains_key(&input.id) && self.inputs.values().filter(|i| i.stream == input.stream).count() >= 4 {
                    return Err(Problem::new(ProblemCode::QuotaExceeded, "stream の入力は 4 つまで"));
                }
                self.inputs.insert(input.id.clone(), input);
            }
            Command::DeleteInput { tenant, id } => {
                self.inputs.get(&id).filter(|i| i.tenant == tenant).ok_or_else(Problem::not_found)?;
                self.inputs.remove(&id);
            }
            Command::PutOutput { output } => {
                self.tenant_stream(&output.tenant, &output.stream)?;
                if let Some(c) = self.outputs.get(&output.id)
                    && c.tenant != output.tenant
                {
                    return Err(conflict("出力のテナントが違います"));
                }
                if !self.outputs.contains_key(&output.id) && self.outputs.values().filter(|o| o.stream == output.stream).count() >= 16 {
                    return Err(Problem::new(ProblemCode::QuotaExceeded, "stream の出力は 16 個まで"));
                }
                self.outputs.insert(output.id.clone(), output);
            }
            Command::DeleteOutput { tenant, id } => {
                self.outputs.get(&id).filter(|o| o.tenant == tenant).ok_or_else(Problem::not_found)?;
                self.outputs.remove(&id);
            }
            Command::PutSecret { secret } => {
                if let Some(c) = self.secrets.get(&secret.id)
                    && c.tenant != secret.tenant
                {
                    return Err(conflict("秘密値のテナントが違います"));
                }
                if !self.secrets.contains_key(&secret.id) {
                    if self.secrets.values().any(|s| s.tenant == secret.tenant && s.name == secret.name) {
                        return Err(conflict("同じ名前の秘密値があります"));
                    }
                    if self.secrets.values().filter(|s| s.tenant == secret.tenant).count() >= 500 {
                        return Err(Problem::new(ProblemCode::QuotaExceeded, "秘密値の数が上限です"));
                    }
                }
                self.secrets.insert(secret.id.clone(), secret);
            }
            Command::DeleteSecret { tenant, id } => {
                self.secrets.get(&id).filter(|s| s.tenant == tenant).ok_or_else(Problem::not_found)?;
                self.secrets.remove(&id);
            }
            Command::PutPipeline { mut pipeline, expect_generation } => {
                match self.pipelines.get(&pipeline.id) {
                    Some(cur) => {
                        if cur.tenant != pipeline.tenant {
                            return Err(Problem::not_found());
                        }
                        if expect_generation.is_some_and(|g| g != cur.generation) {
                            return Err(stale());
                        }
                        if cur.name != pipeline.name
                            && self.pipelines.values().any(|p| p.tenant == pipeline.tenant && p.name == pipeline.name && p.id != pipeline.id)
                        {
                            return Err(conflict("同じ名前のパイプラインがあります"));
                        }
                        pipeline.generation = cur.generation + 1;
                        pipeline.created_at_ms = cur.created_at_ms;
                    }
                    None => {
                        if self.pipelines.values().any(|p| p.tenant == pipeline.tenant && p.name == pipeline.name) {
                            return Err(conflict("同じ名前のパイプラインがあります"));
                        }
                        if self.pipelines.values().filter(|p| p.tenant == pipeline.tenant).count() >= 100 {
                            return Err(Problem::new(ProblemCode::QuotaExceeded, "パイプラインの数が上限です"));
                        }
                        pipeline.generation = 1;
                    }
                }
                self.pipelines.insert(pipeline.id.clone(), pipeline);
            }
            Command::DeletePipeline { tenant, id } => {
                self.pipelines.get(&id).filter(|p| p.tenant == tenant).ok_or_else(Problem::not_found)?;
                self.pipelines.remove(&id);
            }
            Command::ClaimStream { stream, node, now_ms } => {
                if !self.streams.contains_key(&stream) {
                    return Err(Problem::not_found());
                }
                self.fencing_counter += 1;
                self.owners.insert(stream, Ownership { node, token: self.fencing_counter, since_ms: now_ms });
            }
            Command::ReleaseStream { stream, token } => {
                if self.owners.get(&stream).is_some_and(|o| o.token == token) {
                    self.owners.remove(&stream);
                }
            }
            Command::NextFencingToken => {
                self.fencing_counter += 1;
            }
            Command::PutRoom { mut room, expect_generation } => {
                match self.rooms.get(&room.id) {
                    Some(cur) => {
                        // 別のテナントの部屋は「ある」とだけ分かる（ID はテナントをまたいで一意）
                        if cur.tenant != room.tenant {
                            return Err(conflict("その ID の部屋は使われています"));
                        }
                        if expect_generation.is_some_and(|g| g != cur.generation) {
                            return Err(stale());
                        }
                        room.generation = cur.generation + 1;
                        room.created_at_ms = cur.created_at_ms;
                    }
                    None => {
                        if expect_generation.is_some() {
                            return Err(Problem::not_found());
                        }
                        if self.rooms.values().filter(|r| r.tenant == room.tenant).count() >= MAX_ROOMS_PER_TENANT {
                            return Err(Problem::new(ProblemCode::QuotaExceeded, "部屋の数が上限です"));
                        }
                        room.generation = 1;
                    }
                }
                self.rooms.insert(room.id.clone(), room);
            }
            Command::DeleteRoom { tenant, id } => {
                self.rooms.get(&id).filter(|r| r.tenant == tenant).ok_or_else(Problem::not_found)?;
                self.rooms.remove(&id);
                // 部屋のメディアパイプラインも消す（書き出した stream・録画は残る）
                self.media_pipelines.retain(|_, m| m.room != id);
            }
            Command::PutMediaPipeline { mut pipeline, expect_generation } => {
                self.rooms.get(&pipeline.room).filter(|r| r.tenant == pipeline.tenant).ok_or_else(Problem::not_found)?;
                match self.media_pipelines.get(&pipeline.id) {
                    Some(cur) => {
                        if cur.tenant != pipeline.tenant {
                            return Err(Problem::not_found());
                        }
                        if expect_generation.is_some_and(|g| g != cur.generation) {
                            return Err(stale());
                        }
                        pipeline.generation = cur.generation + 1;
                        pipeline.created_at_ms = cur.created_at_ms;
                    }
                    None => {
                        if self.media_pipelines.values().any(|m| m.tenant == pipeline.tenant && m.name == pipeline.name) {
                            return Err(conflict("同じ名前のメディアパイプラインがあります"));
                        }
                        if self.media_pipelines.values().filter(|m| m.room == pipeline.room).count() >= 8 {
                            return Err(Problem::new(ProblemCode::QuotaExceeded, "部屋のメディアパイプラインは 8 つまで"));
                        }
                        pipeline.generation = 1;
                    }
                }
                self.media_pipelines.insert(pipeline.id.clone(), pipeline);
            }
            Command::DeleteMediaPipeline { tenant, id } => {
                self.media_pipelines.get(&id).filter(|m| m.tenant == tenant).ok_or_else(Problem::not_found)?;
                self.media_pipelines.remove(&id);
            }
            Command::Grid { cmd } => {
                if let senca_grid::GridCommand::PutWorkload { workload } = &cmd
                    && crate::workloads::is_derived(&workload.id)
                {
                    return Err(Problem::invalid("資源から導く仕事は直接変えられません"));
                }
                if let senca_grid::GridCommand::DeleteWorkload { id } = &cmd
                    && crate::workloads::is_derived(id)
                {
                    return Err(Problem::invalid("資源から導く仕事は直接変えられません"));
                }
                self.grid.apply(cmd).map_err(|e| match e {
                    senca_grid::GridError::StalePlan { .. } | senca_grid::GridError::InvalidDecision { .. } => {
                        Problem::new(ProblemCode::Conflict, e.to_string())
                    }
                    senca_grid::GridError::UnknownNode { .. }
                    | senca_grid::GridError::UnknownDevice { .. }
                    | senca_grid::GridError::UnknownWorkload { .. } => Problem::not_found(),
                    senca_grid::GridError::Invalid { .. } => Problem::invalid(e.to_string()),
                })?;
            }
        }
        Ok(())
    }

    /// テナントの stream。別テナントのものは「無い」と同じ（仕様 10.1: 404 で存在を漏らさない）。
    pub fn tenant_stream(&self, tenant: &str, id: &str) -> Result<&Stream, Problem> {
        self.streams.get(id).filter(|s| s.tenant == tenant).ok_or_else(Problem::not_found)
    }

    pub fn tenant_recording(&self, tenant: &str, id: &str) -> Result<&Recording, Problem> {
        self.recordings.get(id).filter(|r| r.tenant == tenant).ok_or_else(Problem::not_found)
    }

    pub fn tenant_schedule(&self, tenant: &str, id: &str) -> Result<&Schedule, Problem> {
        self.schedules.get(id).filter(|r| r.tenant == tenant).ok_or_else(Problem::not_found)
    }

    pub fn idempotency(&self, tenant: &str, key: &str) -> Option<&IdempotencyRecord> {
        self.idempotency.get(&idem_key(tenant, key))
    }

    pub fn stream_by_name(&self, tenant: &str, name: &str) -> Option<&Stream> {
        self.streams.values().find(|s| s.tenant == tenant && s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn stream(id: &str, tenant: &str, name: &str) -> Stream {
        Stream {
            id: id.into(),
            tenant: tenant.into(),
            name: name.into(),
            generation: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            key_hash: "h".into(),
            latency_profile: Default::default(),
            codec_profile: Default::default(),
            recording: Default::default(),
            snapshots: None,
            hls: Default::default(),
            limits: Default::default(),
            metadata: Default::default(),
            desired_state: "running".into(),
            captions: None,
        }
    }

    fn token(id: &str, tenant: &str, admin: bool) -> ApiToken {
        let mut scopes = BTreeSet::new();
        scopes.insert(if admin { Scope::TenantAdmin } else { Scope::StreamRead });
        ApiToken { id: id.into(), tenant: tenant.into(), name: id.into(), hash: id.into(), scopes, created_at_ms: 0, expires_at_ms: None }
    }

    #[test]
    fn stream_lifecycle_and_generation() {
        let mut st = ControlState::default();
        st.apply(Command::CreateStream { stream: stream("a", "t1", "cam") }).unwrap();
        assert_eq!(st.apply(Command::CreateStream { stream: stream("b", "t1", "cam") }).unwrap_err().status, 409);
        st.apply(Command::CreateStream { stream: stream("b", "t2", "cam") }).unwrap();
        // 別テナントからは見えない
        assert_eq!(st.tenant_stream("t2", "a").unwrap_err().status, 404);
        let mut s = st.streams["a"].clone();
        s.name = "cam2".into();
        assert_eq!(st.apply(Command::UpdateStream { stream: s.clone(), expect_generation: 7 }).unwrap_err().status, 412);
        st.apply(Command::UpdateStream { stream: s, expect_generation: 1 }).unwrap();
        assert_eq!(st.streams["a"].generation, 2);
        assert_eq!(st.applied_index, 3);
        st.apply(Command::DeleteStream { tenant: "t1".into(), id: "a".into(), expect_generation: Some(2) }).unwrap();
        assert!(!st.streams.contains_key("a"));
    }

    #[test]
    fn last_admin_cannot_be_deleted() {
        let mut st = ControlState::default();
        st.apply(Command::CreateToken { token: token("x", "t", true) }).unwrap();
        st.apply(Command::CreateToken { token: token("y", "t", false) }).unwrap();
        assert_eq!(st.apply(Command::DeleteToken { tenant: "t".into(), id: "x".into() }).unwrap_err().status, 409);
        st.apply(Command::CreateToken { token: token("z", "t", true) }).unwrap();
        st.apply(Command::DeleteToken { tenant: "t".into(), id: "x".into() }).unwrap();
    }

    #[test]
    fn idempotency_conflict() {
        let mut st = ControlState::default();
        let r = IdempotencyRecord {
            tenant: "t".into(),
            key: "k".into(),
            fingerprint: "f1".into(),
            status: 201,
            body: "{}".into(),
            location: None,
            etag: None,
            subject: None,
            content_type: None,
            created_at_ms: 10,
        };
        st.apply(Command::PutIdempotency { record: r.clone() }).unwrap();
        let mut r2 = r.clone();
        r2.fingerprint = "f2".into();
        assert_eq!(st.apply(Command::PutIdempotency { record: r2 }).unwrap_err().status, 409);
        st.apply(Command::ExpireIdempotency { before_ms: 11 }).unwrap();
        assert!(st.idempotency("t", "k").is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let mut st = ControlState::default();
        st.apply(Command::CreateStream { stream: stream("a", "t1", "cam") }).unwrap();
        let j = serde_json::to_string(&st).unwrap();
        let back: ControlState = serde_json::from_str(&j).unwrap();
        assert_eq!(back, st);
        let c = Command::NextFencingToken;
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"op":"next_fencing_token"}"#);
    }
}
