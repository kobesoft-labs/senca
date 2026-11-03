// 生成物: apps/sdk-js/scripts/gen_types.py が docs/api/openapi.yaml から作る。手で直さない。
// 管理 API v1 の components.schemas（132 個）。

export type Capabilities = {
  api_version: "v1";
  captions?: {
    asr?: string[];
    translate?: string[];
  };
  cluster?: boolean;
  codec_profiles: CodecProfile[];
  codecs: {
    passthrough: string[];
    /** コーデック worker が報告する変換能力（未接続なら空） */
    transcode: TranscodeCapability[];
  };
  ingest: {
    rtmp: boolean;
    rtmps: boolean;
    rtsp_pull: boolean;
    srt: boolean;
    whip: boolean;
  };
  /** 使える入力の種類 */
  inputs?: string[];
  /** このノードの ID */
  node: string;
  output: {
    hls: {
      containers: Array<"ts" | "fmp4">;
      ll_hls: boolean;
    };
    /** HTTP/3（`--quic`） */
    http3?: boolean;
    /** MoQ（draft-22）の購読と送出（`--quic`） */
    moq?: boolean;
    rtmp_play: boolean;
    whep: boolean;
  };
  /** 使える出力の種類 */
  outputs?: string[];
  paas_providers?: string[];
  /** パイプライン（codec worker が動いているとき） */
  pipelines?: boolean;
  recordings: {
    export: string[];
    max_concurrent_per_stream: number;
  };
  rtsp_play?: boolean;
  schedules: {
    actions: ScheduleAction[];
    recurrence: Array<"daily" | "weekly">;
  };
};

/**
 * 字幕と翻訳（R6-4）。配信中の音声を `asr_endpoint` に送って文字にし、`targets` の言語へ翻訳して、ライブ HLS に WebVTT の字幕を足す。
 * 検査: `asr_provider`・`translate_provider` の値、`asr_endpoint`・`translate_endpoint` が URL として読めること、言語は `language` と `targets` 合わせて英数字と `-` の 1〜8 文字（`targets` は 8 個まで）。違えば 400（`INVALID_CONFIG`）。
 * `asr_credential_ref`・`translate_credential_ref` は `secret:{name}` の形で、テナントに秘密値があること（無ければ 400）。参照している秘密値は消せない（409）。
 */
export type CaptionSettings = {
  asr_credential_ref?: CredentialRef;
  /** 送り先の URL（例: `https://api.openai.com/v1`、社内の whisper サーバー） */
  asr_endpoint: string;
  asr_model?: string | null;
  /** openai = OpenAI 互換の `/audio/transcriptions`、whisper_cpp = whisper.cpp のサーバー */
  asr_provider: "openai" | "whisper_cpp";
  /** 話している言語（ISO 639-1） */
  language?: string;
  /** 翻訳先の言語 */
  targets?: string[];
  translate_credential_ref?: CredentialRef;
  translate_endpoint?: string | null;
  translate_model?: string | null;
  /** 翻訳（省けば翻訳しない）。書くなら translate_endpoint が要る */
  translate_provider?: "anthropic" | "deepl" | "openai" | null;
};

/** royalty_free は H.264・H.265・AAC を理由付きで拒否する（REQ-007）。royalty_free では hls.container は fmp4 にする。 */
export type CodecProfile = "any" | "royalty_free";

/** `secret:{name}`（`POST /secrets` で登録した秘密値の名前）。無い秘密値・形の違いは 400。 */
export type CredentialRef = string | null;

export type DegradeLevel = {
  /** この段で止めるグラフの部品の id（グラフに無ければ 400）。下流は入力がすべて止まれば止まる。段は積み重なる（段 2 は段 1 の分も止める） */
  disable: PipelineNodeId[];
  /** 段の名前（例: `no_translation`・`audio_only`）。`full`（段 0 の名前）と重複は不可 */
  label: string;
};

export type DeviceHealth = {
  device: string;
  /** disable は false、enable は true */
  enabled: boolean;
  /** ノードの ID（名前で指定しても ID で返す） */
  node: string;
};

export type DeviceStatus = {
  providers: Array<{
    available: boolean;
    /** 使えない理由、または読み込んだライブラリの説明 */
    detail: string;
    /** コーデックのプロバイダー（例: nvenc、dav1d） */
    provider: string;
  }>;
  /** worker のサンドボックスの種類（worker がまだ情報を返していなければ null） */
  sandbox: string | null;
};

export type Devices = {
  nodes: Array<{
    /**
     * このノードのキャプチャ機器（REQ-004、隔離した `senca-capture-worker` に問い合わせる。入力の `protocol: device` で使う）。
     * worker が無い・失敗したときは `{available: false, reason}`、ランタイムが無ければ null。
     */
    capture: {
      /** バックエンド（V4L2・ALSA・DeckLink・合成）ごとの可否 */
      backends: Array<Record<string, unknown>>;
      /** 機器（`id` を入力の `url: device:{id}` に書く） */
      devices: Array<Record<string, unknown>>;
      version: number;
    } | {
      available: false;
      reason: string;
    } | null;
    /** コーデック worker のプールが動いていなければ `providers` は空、`sandbox` は null。 */
    devices: DeviceStatus;
    node: string;
  }>;
};

/** 転送フィルター（`docs/realtime/signaling.md` の 8 章）。`rules` は「AND の組」の OR。`block` は一致したものを配らない、`allow` は一致したものだけ配る。 */
export type ForwardingFilter = {
  action: "block" | "allow";
  rules: ForwardingRule[][];
};

export type ForwardingRule = {
  /** 送り手の属性: `connection_id`・`client_id`・`bundle_id`・`kind`・`metadata.<key>`（key は 1〜64 文字。値が文字列・数・真偽のとき） */
  field: string;
  operator: "is_in" | "is_not_in";
  /** `kind` なら `audio`・`video` */
  values: string[];
};

export type Gap = {
  at: Timestamp;
  dropped: number;
  /** publisher_reconnect など */
  reason: string;
};

export type Grid = {
  nodes: Array<{
    /** 仕事に貸せる分（`--capacity-cpu-millis`・`--capacity-memory-mb`・`--net-mbps`、既定は実機の 90%・85%・10000） */
    capacity: Resources;
    capture: Array<{
      /** 運用者が手で止めた（止めたときだけ現れる） */
      disabled?: boolean;
      healthy: boolean;
      /** 機器の ID（`GET /devices` の `capture.devices[].id`、例: `synthetic:0`） */
      id: string;
      /** 機器の種類（機器の一覧の `backend`、例: `synthetic`・`v4l2`） */
      kind: string;
    }>;
    draining: boolean;
    gpus: Array<{
      decode_reserved: number;
      decode_sessions: number;
      /** 運用者が手で止めていない（`disable`・`enable`）。仕事を置くのは healthy かつ enabled の装置だけ */
      enabled: boolean;
      encode_reserved: number;
      encode_sessions: number;
      /** 測った健康（心拍で変わる） */
      healthy: boolean;
      /** `{provider}:{装置の番号}`（例: `nvidia:0`） */
      id: string;
      /** `nvidia`・`intel`・`amd`・`vaapi` など */
      vendor: string;
    }>;
    id: string;
    /** `--label` の値と、予約のラベル `senca.io/name`（ノードの名前） */
    labels: { [key: string]: string };
    /** 最後の心拍（配置係のノードだけが知っている。違えば null） */
    last_heartbeat: Timestamp | null;
    /** ノードの名前（`GET /nodes` に無ければ null） */
    name: string | null;
    /** 割り当てた仕事の要求（今の段）の和 */
    reserved: Resources;
    /** control = Raft の投票者、media = 仕事を動かす（仕事は media にだけ置く） */
    roles: Array<"control" | "media">;
    /** 心拍が届いている（5 秒途絶えると false） */
    up: boolean;
    /** このノードに割り当てた仕事の数 */
    workloads: number;
  }>;
  /** 応答したノードが配置係（リーダー）ならその ID、違えば null */
  scheduler: string | null;
  workloads: Array<{
    device: string | null;
    /** 資源から導いた仕事の ID（`input:{id}`・`output:{id}`・`pipeline:{id}`・`room:{id}`） */
    id: string;
    /** 入力は `ingest`（`device` は `capture`）、出力は `egress`、パイプラインは `pipeline`、部屋は `room` */
    kind: "ingest" | "room" | "pipeline" | "recording" | "transcode" | "capture" | "relay" | "media_pipeline" | "egress";
    /** 今の縮退の段（0 = full） */
    level: number | null;
    level_label: string | null;
    node: string | null;
    /** `placement.node`・`placement.device` で手動で割り当てた */
    pinned: boolean;
    priority: PlacementPriority;
    reason: GridReason | null;
    state: "running" | "degraded" | "unschedulable" | "pending";
    unschedulable: RejectCount[] | null;
  }>;
};

/** 割り当て・決定の理由（`code` で種類を選ぶ。`node_down` は `node`、`device_down` は `device`、`preempted` は `by` を持つ）。 */
export type GridReason = {
  /** 追い出した仕事の ID */
  by?: string;
  /**
   * placed = 普通に置いた、no_capacity = 資源が足りず縮退した段で置いた・縮退した、node_down = ノードが止まった・消えた、device_down = 装置が故障した（手で止めたときも）、
   * draining = ノードの排出で移した、constraint_changed = 条件・ラベルが変わって合わなくなった、preempted = 高い優先度の仕事のために縮退・追い出しされた、
   * overcommitted = ノードの容量が減って収まらなくなった、capacity_returned = 資源が戻ったので元の段へ戻した、pin_returned = 手動の割り当ての対象が戻ったので戻した、
   * pin_target_down = 手動の割り当て（strict）の対象が止まっている。
   */
  code: "placed" | "no_capacity" | "node_down" | "device_down" | "draining" | "constraint_changed" | "preempted" | "overcommitted" | "capacity_returned" | "pin_returned" | "pin_target_down";
  device?: string;
  node?: string;
};

export type HlsSettings = {
  container?: "ts" | "fmp4";
  segment_ms?: number;
  window?: number;
};

export type Ingest = {
  /** RTMP の publish 先（`{public-rtmp-url}/live`）。stream name に stream_key を使う。 */
  rtmp_url: string;
  srt_streamid_format?: "publish:{stream_key}";
  /** `--public-srt-url` を設定したとき */
  srt_url?: string;
  /** publish 鍵。作成・作り直しの応答でだけ値が入る（それ以外は null）。 */
  stream_key: string | null;
  /** Idempotency-Key の再送で鍵を伏せたとき */
  stream_key_redacted?: true;
  /** `--public-http-url` を設定したとき */
  whip_url?: string;
};

export type Input = {
  created_at: Timestamp;
  credential_ref: CredentialRef;
  enabled: boolean;
  generation: number;
  id: string;
  options: { [key: string]: string };
  /** 配置の指定。指定が無ければ null */
  placement: PlacementSpecView | null;
  placement_status: PlacementStatus;
  protocol: "rtmp_pull" | "rtsp_pull" | "srt_pull" | "hls_pull" | "device";
  /** このノードでの実行の様子。`enabled: false`、実行をまだ始めていない、またはこのノードに割り当てられていなければ null（分散のときは、割り当てたノード `placement_status.node` に問い合わせる）。作成・PATCH の応答では、実行の作り直しが非同期なので前の実行のもの（または null）のことがある。 */
  status: IoStatus | null;
  stream: string;
  url: string;
};

export type InputCreate = {
  credential_ref?: CredentialRef;
  enabled?: boolean | null;
  options?: IoOptions;
  /** 配置の指定（`degrade` は使えない） */
  placement?: PlacementSpec | null;
  protocol: "rtmp_pull" | "rtsp_pull" | "srt_pull" | "hls_pull" | "device";
  /**
   * 取りに行く URL。スキームは protocol で決まる（rtmp_pull: `rtmp`・`rtmps`、rtsp_pull: `rtsp`・`rtsps`、srt_pull: `srt`、hls_pull: `http`・`https`、device: `device:{機器 ID}`（300 文字まで））。
   * ホストが要る。空白・制御文字は不可。資格情報（`user:pass@`）は入れられない（`credential_ref` を使う）。
   */
  url: string;
};

export type InputList = {
  items: Input[];
  next_cursor: string | null;
};

/** プロトコルごとの設定（例: RTSP の `transport: tcp|udp`、device の `width`・`height`・`fps`・`codec`・`bitrate_bps`・`audio_device`・`audio_codec`）。16 個まで、キー 64 文字・値 256 文字まで。 */
export type IoOptions = { [key: string]: string };

/** 書いたフィールドだけ置き換える。`credential_ref: null` で参照を外す、`placement: null` で配置の指定を外す（配置係に任せる）。他の null は「変えない」。 */
export type IoPatch = {
  credential_ref?: CredentialRef;
  enabled?: boolean | null;
  options?: IoOptions | null;
  /** 配置の指定を丸ごと置き換える（部分の変更は無い。`degrade` は使えない）。null で外す */
  placement?: PlacementSpec | null;
  url?: string | null;
};

/** このノードでの実行の様子（制御状態には保存しない）。 */
export type IoStatus = {
  /** 送った・受けたバイト数（目安） */
  bytes: number;
  /** 接続に成功した回数 */
  connects: number;
  /** 最後の失敗の理由（宛先の拒否は「… は許されていない宛先です」）。成功しても消えない */
  last_error: string | null;
  /** 今の state になった時刻 */
  since: string | null;
  /**
   * starting = 実行を始めた、provisioning = PaaS の資源を用意している、waiting_input = 出力で配信を待っている、connecting = 接続中、
   * active = 送受信している、backoff = 失敗の後の待ち（1〜60 秒の指数バックオフ）、error = 失敗した（すぐ backoff になる）、ended = VOD の HLS を読み終えた（`hls_pull` だけ）。
   */
  state: "starting" | "provisioning" | "waiting_input" | "connecting" | "active" | "backoff" | "error" | "ended";
};

/** ノードのラベル（`--label`、予約の `senca.io/name`）の条件。`op` で種類を選ぶ。キー・値は 1〜128 文字で制御文字を含めない。 */
export type LabelSelector = {
  key: string;
  op: "eq";
  value: string;
} | {
  key: string;
  op: "in" | "not_in";
  values: string[];
};

/** standard = HLS、low = LL-HLS、interactive = WebRTC・MoQ */
export type LatencyProfile = "standard" | "low" | "interactive";

export type Limits = {
  max_ingress_bps?: number | null;
  max_viewers?: number | null;
};

export type LiveInfo = {
  bitrate_bps: number;
  /** 再接続ごとに増える */
  epoch: number;
  started_at: Timestamp;
  tracks: Array<Record<string, unknown>>;
  /** rtmp・srt など */
  transport: string;
  viewers_hint: number;
};

export type LivePlayback = {
  expires_at: Timestamp;
  playback: {
    /** `{http}/s/{exp}/{sig}/live/{id}/index.m3u8` */
    hls: string;
    /** MoQ（draft-22）の WebTransport の URL `https://{host}:{quic port}/moq?exp=&sig=`。namespace は `senca/live/{id}`（`--quic` を設定したときだけ） */
    moq?: string;
    /** `{rtmp}/play/{id}?exp=&sig=` */
    rtmp: string;
    /** `{public-rtsp-url}/live/{id}?exp=&sig=`（`--public-rtsp-url` を設定したときだけ） */
    rtsp?: string;
    /** `{http}/whep/{id}?exp=&sig=`（`--webrtc-udp` を設定しないノードでは null） */
    whep: string | null;
  };
};

export type MediaPipeline = {
  artifacts: MediaPipelineArtifacts;
  /** 資格情報は `secret:{name}` の参照で、値は返さない */
  captions: CaptionSettings | null;
  composite: MediaPipelineComposite | null;
  created_at: Timestamp;
  desired_state: MediaPipelineState;
  egress: MediaPipelineEgress[];
  export_participants: boolean;
  generation: number;
  id: string;
  name: MediaPipelineName;
  participants: MediaPipelineParticipants;
  recording: MediaPipelineRecording | null;
  room: string;
  /**
   * このノードでの実行の様子。部屋をこのノードに置いていない・`stopped`・係がまだ見回っていない（作った直後）なら null
   * （分散のときは部屋を置いたノードに問い合わせる）。止めた・消した直後の 1 秒ほどは前の様子が残ることがある。
   */
  status: MediaPipelineStatus | null;
  updated_at: Timestamp;
};

/** 子の資源 */
export type MediaPipelineArtifacts = {
  /** `composite` が無ければ null */
  composite: MediaPipelineCompositeArtifacts | null;
  /** 書き出した参加者ごとの stream（`metadata.media_pipeline` がこの ID の stream。今いない参加者のものも残る）。ID の昇順 */
  participant_streams: MediaPipelineParticipantStream[];
};

/**
 * 合成（会議の 1 本の映像・音声）。書かなかったフィールドは既定値（応答ではすべて入る）。映像のある参加者を並べ、音声はすべて混ぜて Opus 96 kbit/s にする。
 * `grid` は格子（列数は人数の平方根の切り上げ）、`speaker` は最初に書き出した参加者を上に大きく、残りを下の 1/4 の帯に並べる（話者の検出はまだ合成に届かない）。
 * H.264・H.265 は GPU が要る。GOP は `fps` の 2 倍。
 */
export type MediaPipelineComposite = {
  bitrate_kbps?: number;
  fps?: number;
  height?: number;
  layout?: "grid" | "speaker";
  video_codec?: "av1" | "vp9" | "vp8" | "h264" | "h265";
  width?: number;
};

/** 合成の子の資源の ID（メディアパイプラインの ID から決まる。まだ作っていなくても ID は出る） */
export type MediaPipelineCompositeArtifacts = {
  /** `egress` の順の送り出しの出力の ID（`GET /outputs/{id}`） */
  outputs: string[];
  /** 合成のパイプラインの ID（`GET /pipelines/{id}`。映像のある参加者が流れ始めるまでは無いことがある） */
  pipeline: string;
  /** 合成の stream の ID（`GET /streams/{id}`） */
  stream: string;
};

export type MediaPipelineCreate = {
  /** 文字起こし・翻訳（AWS の media insights pipeline 相当）。参加者ごとの stream の `captions` になる（省略・null は付けない） */
  captions?: CaptionSettings | null;
  /** 合成（省略・null は合成しない） */
  composite?: MediaPipelineComposite | null;
  /** 省略・null は running */
  desired_state?: MediaPipelineState | null;
  /** 送り出し（`composite` が要る） */
  egress?: MediaPipelineEgress[];
  /** 参加者ごとの stream を残す（AWS の media stream pipeline 相当）。false でも合成のために書き出す（`recording.participants`・`captions` は使えない） */
  export_participants?: boolean | null;
  name: MediaPipelineName;
  participants?: MediaPipelineParticipants;
  /** 録画（省略・null は録らない） */
  recording?: MediaPipelineRecording | null;
  /** 部屋の ID（テナントの部屋。無ければ 400） */
  room: string;
};

/**
 * 送り出し（AWS の media live connector pipeline 相当）。合成の stream の出力（`Output`）として作る。形と検査は `OutputCreate` と同じ
 * （`paas` には `credential_ref` が要る）。応答では null・空のフィールドは欄が無い。
 */
export type MediaPipelineEgress = {
  credential_ref?: CredentialRef;
  options?: IoOptions;
  protocol: "rtmp_push" | "srt_push" | "paas";
  /** `paas` で必須 */
  provider?: "cloudflare_stream" | "aws_ivs" | "aws_medialive" | null;
  /** `paas` 以外で必須。rtmp_push: `rtmp`・`rtmps`、srt_push: `srt`。資格情報は入れられない。 */
  url?: string | null;
};

export type MediaPipelineList = {
  items: MediaPipeline[];
  next_cursor: string | null;
};

/** テナント内で一意（作成のとき）。1〜40 文字の英数字と `-_.`（`.` で始まらない）。子の stream・パイプラインの名前の頭に使う */
export type MediaPipelineName = string;

export type MediaPipelineParticipantStream = {
  /** stream の名前（`{name}-{参加者}-{ID の頭 6 文字}`） */
  name: string;
  /** stream の ID */
  stream: string;
};

/** 書き出す参加者の `client_id`（参加トークンの `client_id`）。空なら全員（`client_id` の無い参加者は `connection_id` で区別する） */
export type MediaPipelineParticipants = string[];

/** 書いたフィールドだけ置き換える。`composite`・`recording`・`captions` の null は外す、他の null は「変えない」。`egress` は列ごと置き換える。 */
export type MediaPipelinePatch = {
  captions?: CaptionSettings | null;
  composite?: MediaPipelineComposite | null;
  desired_state?: MediaPipelineState | null;
  egress?: MediaPipelineEgress[] | null;
  export_participants?: boolean | null;
  name?: MediaPipelineName | null;
  participants?: MediaPipelineParticipants | null;
  recording?: MediaPipelineRecording | null;
};

/**
 * 録画（AWS の media capture pipeline 相当）。書き出した stream の自動録画（`RecordingPolicy`）を有効にする。録画は普通の録画の API（`GET /recordings?stream=...`）で読む。
 * 録画の設定は stream を作るときに決まる（後から `recording` を変えても、作った stream には効かない）。
 */
export type MediaPipelineRecording = {
  /** 合成の stream を録る（`composite` が要る） */
  composite?: boolean;
  /** 参加者ごとの stream を録る（`export_participants: true` が要る） */
  participants?: boolean;
  /** 保持日数（省けば録画の既定。応答では省いたとき欄が無い） */
  retention_days?: number | null;
};

/** running = 部屋を置いたノードの係が動かす、stopped = 止める（子の合成・送り出しを消す。書き出した stream・録画は残る） */
export type MediaPipelineState = "running" | "stopped";

/** このノードでの実行の様子（制御状態には保存しない）。 */
export type MediaPipelineStatus = {
  /** 合成の図に入っている参加者の数（`composite` が無い、または映像のある参加者が流れ始めていなければ null） */
  composite_members: number | null;
  /** 今書き出している参加者 */
  participants: Array<{
    /** `client_id`（無ければ `connection_id`） */
    participant: string;
    /** 書き出し先の stream の ID */
    stream: string;
    /** 映像を送っているか */
    video: boolean;
  }>;
};

export type Metadata = { [key: string]: string };

export type Node = {
  draining: boolean;
  id: string;
  labels: { [key: string]: string };
  /** 配信中の stream 数（他ノードは今は 0） */
  live_streams: number;
  name: string;
  registered_at: Timestamp;
  /** ノード間の中継（ADR-0014）の様子。応答したノード自身（`self: true`）で、クラスタを組んでいるときだけ値がある（単一ノード・他ノードは null）。 */
  relay: RelayStatus | null;
  /** 応答したノード自身か */
  self: boolean;
};

export type NodeList = {
  items: Node[];
};

export type Output = {
  created_at: Timestamp;
  credential_ref: CredentialRef;
  enabled: boolean;
  generation: number;
  id: string;
  options: { [key: string]: string };
  /** 配置の指定。指定が無ければ null */
  placement: PlacementSpecView | null;
  placement_status: PlacementStatus;
  protocol: "rtmp_push" | "srt_push" | "paas";
  provider: "cloudflare_stream" | "aws_ivs" | "aws_medialive" | null;
  /** SENCA が PaaS に作った資源（後片付けに使う） */
  provider_resource: string | null;
  /** このノードでの実行の様子。`enabled: false`、実行をまだ始めていない、またはこのノードに割り当てられていなければ null（分散のときは、割り当てたノード `placement_status.node` に問い合わせる）。作成・PATCH の応答では、実行の作り直しが非同期なので前の実行のもの（または null）のことがある。 */
  status: IoStatus | null;
  stream: string;
  url: string | null;
};

export type OutputCreate = {
  credential_ref?: CredentialRef;
  enabled?: boolean | null;
  options?: IoOptions;
  /** 配置の指定（`degrade` は使えない）。指定が無ければ、配信を受けたノードを好む */
  placement?: PlacementSpec | null;
  protocol: "rtmp_push" | "srt_push" | "paas";
  /** `paas` で必須（`credential_ref` も必須） */
  provider?: "cloudflare_stream" | "aws_ivs" | "aws_medialive" | null;
  /** `paas` 以外で必須。rtmp_push: `rtmp`・`rtmps`、srt_push: `srt`。資格情報は入れられない。 */
  url?: string | null;
};

export type OutputList = {
  items: Output[];
  next_cursor: string | null;
};

export type Participant = {
  bundle_id: string;
  /** 無ければ省く */
  client_id?: string;
  connection_id: string;
  joined_at?: Timestamp;
  metadata: RoomMetadata;
  /** 他のノードにいる参加者（カスケード）のノード */
  node?: string;
  /** WebRTC（ICE・DTLS）がつながっているか（他のノードの参加者は true） */
  peer_connected: boolean;
  role: "sendrecv" | "sendonly" | "recvonly";
  /** シグナリングの経路（remote は他のノードの参加者） */
  signaling: "websocket" | "datachannel" | "remote";
  /** 受信の枠（他のノードの参加者は空） */
  subscriptions: RoomSlot[];
  /** 送っているトラック */
  tracks: RoomTrack[];
};

export type ParticipantList = {
  items: Participant[];
  /** 続きを読む `cursor`（続きがあるときだけ値、無ければ null） */
  next_cursor?: string | null;
};

export type ParticipantPatch = {
  metadata: RoomMetadata;
};

export type Pipeline = {
  created_at: Timestamp;
  desired_state: "running" | "stopped";
  generation: number;
  graph: PipelineGraph;
  id: string;
  /** 直近の失敗（例: `codec worker が動いていません`）。動き出すと null に戻る */
  last_error: string | null;
  /** 実行していなければ null、実行中は `audio_mix` の部品の id → 直近のメーター（ミキサーが無い・まだ通知が無ければ `{}`）。 */
  meters: PipelineMeters | null;
  name: string;
  /**
   * このノードでの実行の様子。starting = セッションを開いている（`desired_state: running` でまだ実行を始めていないときも）、running = 動いている、
   * error = 失敗して作り直しを待っている（`last_error`、1〜32 秒のバックオフ）、stopped = 止まっている。
   */
  observed_state: "starting" | "running" | "error" | "stopped";
  /** 配置の指定。指定が無ければ null */
  placement: PlacementSpecView | null;
  placement_status: PlacementStatus;
  /** 実行中の計画（worker が開いたセッションのもの）。動いていなければ null（検証の計画は `POST /pipelines/{id}/validate`）。 */
  plan: PipelinePlan | null;
  /** 直近の状態の通知。まだ無ければ null。 */
  status: PipelineStatus | null;
  updated_at: Timestamp;
};

/**
 * 操作（senca-pipeline の `Control`、`op` で種類を選ぶ。知らない項目は 400）。`node` は対象の部品の id（`compose` または `audio_mix`）。
 * `prepare`・`commit`・`rollback` は型としては読めるが、この API では 400（グラフの変更は `PATCH /pipelines/{id}`）。
 */
export type PipelineControl = {
  node: string;
  op: "switch_program";
  scene: string;
  transition?: PipelineTransition;
} | {
  node: string;
  op: "set_preview";
  scene: string;
} | {
  node: string;
  op: "take";
  transition?: PipelineTransition;
} | {
  layer: number;
  node: string;
  op: "update_layer";
  patch: {
    height?: number;
    opacity?: number;
    source?: string;
    visible?: boolean;
    width?: number;
    x?: number;
    y?: number;
    z?: number;
  };
  scene: string;
} | {
  layers: PipelineLayer[];
  node: string;
  op: "set_scene";
  scene: string;
} | {
  delay_ms?: number;
  gain_db?: number;
  input: string;
  mute?: boolean;
  node: string;
  op: "set_audio";
  pan?: number;
  solo?: boolean;
} | {
  gain_db: number;
  node: string;
  op: "set_master";
} | {
  enabled: boolean;
  node: string;
  op: "snapshot";
} | {
  node?: string | null;
  op: "request_keyframe";
} | {
  op: "prepare" | "commit" | "rollback";
};

export type PipelineControlResult = {
  ok: true;
  /** 操作の返事（今の操作では空のオブジェクト） */
  reply: Record<string, unknown>;
};

export type PipelineCreate = {
  desired_state?: "running" | "stopped" | null;
  graph: PipelineGraph;
  name: StreamName;
  /** 配置の指定と縮退の段 */
  placement?: PlacementSpec | null;
};

/**
 * パイプラインの定義（senca-pipeline の `Graph`、`crates/senca-pipeline/src/model.rs`）。有向非巡回グラフで、知らない項目は拒否する。大きさは 16 MiB まで（ただし HTTP 層の本文の上限 1 MiB が先にかかる）。
 * 部品（`nodes`）は `type` で種類を選ぶ。入力の参照は `"部品"` または `"部品.ポート"`（`source` のポートは `video`・`audio` で省けない、`compose` は `program`（既定）・`preview`、他は `out`）。
 * 部品ごとの設定のすべては crate の文書（`cargo doc -p senca-pipeline`）を見る。ここでは種類と必須の項目だけを書く。値の範囲は検証（`POST /pipelines/{id}/validate`）で調べる。
 */
export type PipelineGraph = {
  nodes: PipelineNode[];
  options?: PipelineOptions;
  version: 1;
};

/** 検証の失敗（400・503 の `details.issues` の要素）。 */
export type PipelineIssue = {
  code: "MALFORMED" | "INVALID_PARAM" | "DUPLICATE_ID" | "UNKNOWN_REFERENCE" | "TYPE_MISMATCH" | "CYCLE" | "UNCONSUMED_OUTPUT" | "POLICY_REJECTED" | "UNSUPPORTED_CODEC" | "CAPACITY_UNAVAILABLE" | "LIMIT_EXCEEDED";
  message: string;
  /** 問題の部品の id */
  node?: string;
};

/** シーンのレイヤー（senca-pipeline の `LayerSpec`: source・image・color のどれか、x・y・width・height、crop・fit・z・opacity・corner_radius・border・visible・filter） */
export type PipelineLayer = Record<string, unknown>;

export type PipelineList = {
  items: Pipeline[];
  next_cursor: string | null;
};

/** `audio_mix` の部品の id → 直近のメーター（`Event::Meters`: `inputs`・`master` の peak/rms dBFS、`limiter_reduction_db`）。 */
export type PipelineMeters = { [key: string]: {
  event: "meters";
} };

/** 部品。`type` ごとの必須の項目だけを書く（他の項目は crate の文書）。`id` は英数字・`_`・`-` の 1〜64 文字でグラフ内で一意。 */
export type PipelineNode = {
  audio?: PipelineSourceTrack;
  id: PipelineNodeId;
  /** 入力の stream の ID（テナントのもの） */
  stream: string;
  type: "source";
  video?: PipelineSourceTrack;
} | {
  id: PipelineNodeId;
  input: string;
  type: "decode";
} | {
  id: PipelineNodeId;
  input: string;
  /** 板書の補正（mode・corners・auto_detect・output_width・output_height・enhance など） */
  params?: Record<string, unknown>;
  type: "board";
} | {
  filter?: "nearest" | "bilinear" | "bicubic" | "lanczos3" | "area";
  height: number;
  id: PipelineNodeId;
  input: string;
  type: "scale";
  width: number;
} | {
  id: PipelineNodeId;
  input: string;
  type: "crop";
} | {
  canvas: Record<string, unknown>;
  id: PipelineNodeId;
  /** シーンから参照する名前 → 上流の参照 */
  inputs: { [key: string]: string };
  preview?: string | null;
  program: string;
  scenes: { [key: string]: Record<string, unknown> };
  type: "compose";
} | {
  id: PipelineNodeId;
  inputs: { [key: string]: Record<string, unknown> };
  type: "audio_mix";
} | {
  bitrate_kbps: number;
  /** av1・vp9・vp8・opus・h264・h265 など（使えるものはコーデック worker の能力と方針で決まる） */
  codec: string;
  id: PipelineNodeId;
  input: string;
  type: "encode";
} | {
  id: PipelineNodeId;
  input: string;
  type: "snapshot_on_change";
} | {
  id: PipelineNodeId;
  inputs: string[];
  /** 出力先の stream の ID（テナントのもの。パイプラインが送り手になる） */
  stream: string;
  type: "sink";
};

export type PipelineNodeId = string;

export type PipelineNodeType = "source" | "decode" | "board" | "scale" | "crop" | "compose" | "audio_mix" | "encode" | "snapshot_on_change" | "sink";

export type PipelineOptions = {
  block_timeout_ms?: number;
  clock?: "realtime" | "input";
  commit_timeout_ms?: number;
  frame_queue?: number;
  input_queue_aus?: number;
  latency_ms?: number;
  missing_timeout_ms?: number;
  overflow?: "drop" | "block";
  /** 状態・メーターの更新の間隔（0 で出さない） */
  status_interval_ms?: number;
};

/** 書いたフィールドだけ置き換える（null は「変えない」。ただし `placement: null` は配置の指定を外す）。 */
export type PipelinePatch = {
  desired_state?: "running" | "stopped" | null;
  graph?: PipelineGraph | null;
  name?: StreamName | null;
  /** 配置の指定を丸ごと置き換える（部分の変更は無い）。null で外す。書かなければ今の指定を新しい `graph` で検査し直す（縮退の段の `disable` が新しいグラフに無ければ 400） */
  placement?: PlacementSpec | null;
};

/** 実行の計画（処理の形・必要な資源・追加の遅延）。 */
export type PipelinePlan = {
  nodes: Array<{
    added_latency_ms: number;
    codec?: string;
    cpu_codec_sessions: number;
    gpu_codec_sessions: number;
    hardware?: boolean;
    id: string;
    mode: "passthrough" | "transcode" | "process" | "terminal";
    notes?: string[];
    /** 出力の型の説明 */
    outputs?: string[];
    pixels_per_sec: number;
    provider?: string;
    type: PipelineNodeType;
  }>;
  /** 実行の順（トポロジカル順） */
  order: string[];
  sinks: Array<{
    id: string;
    stream: string;
    tracks: Array<{
      codec: string;
      from: string;
      kind: "video" | "audio";
      latency_ms: number;
      mode: "passthrough" | "transcode" | "process" | "terminal";
      track: number;
    }>;
  }>;
  totals: {
    cpu_codec_sessions: number;
    gpu_codec_sessions: number;
    max_latency_ms: number;
    pixels_per_sec: number;
    threads: number;
  };
};

export type PipelineSourceTrack = {
  channels?: number | null;
  codec: string;
  fps?: number | null;
  height?: number | null;
  sample_rate?: number | null;
  track?: number | null;
  width?: number | null;
};

/**
 * パイプラインの直近の通知（senca-pipeline の `Event`、`event` で種類を選ぶ）。
 * `event: status`（`status_interval_ms` ごと）は `sources`（入力ごとの state `waiting`・`live`・`stalled`・`ended`、AU 数、タリー `on_program`/`on_preview`）、
 * `compose`（シーン・トランジション中か・スレートを出している入力）、`encoders`（フレーム数・誤り）を持つ。
 * メーター（`event: meters`）は `meters` に入り、ここにはそれ以外の直近の通知が残る（世代・誤りの通知のこともある）。
 */
export type PipelineStatus = {
  compose: Array<Record<string, unknown>>;
  encoders: Array<Record<string, unknown>>;
  event: "status";
  /** 実行中のグラフの世代（worker の中の番号。パイプラインの generation とは別） */
  generation: number;
  session_us: number;
  sources: Array<{
    state: "waiting" | "live" | "stalled" | "ended";
  }>;
} | {
  event: "input_dropped" | "error" | "generation_prepared" | "generation_committed" | "generation_failed" | "generation_rolled_back";
};

export type PipelineTransition = {
  duration_ms?: number;
  kind?: "cut" | "fade" | "wipe_left" | "wipe_right";
};

export type PipelineValidation = {
  ok: true;
  plan: PipelinePlan;
};

/** 優先度（low < normal < high < critical）。資源が足りないとき、低い仕事から縮退・追い出しする。 */
export type PlacementPriority = "low" | "normal" | "high" | "critical";

/**
 * 配置の指定（ADR-0013）。省略・null なら配置係に任せる（優先度 normal、spread）。検査の失敗は 400（`INVALID_CONFIG`、未知のフィールド・型の違いは `INVALID_REQUEST`）:
 * `node` が無い、文字列が空・128 文字超・制御文字を含む、`labels` が 17 個以上、`values` が空・33 個以上、`degrade` を入力・出力に書いた、
 * `degrade` が 9 段以上・段の名前が `full` か重複・`disable` が空・グラフに無い部品。
 */
export type PlacementSpec = {
  /** 組の名前。同じ組の仕事は同じノードに置かない */
  anti_affinity?: string | null;
  /** 縮退の段（**パイプラインだけ**、よい順）。資源が足りないとき、段を下げて（部品を止めて）動かし続ける。資源が戻れば元の段へ戻す */
  degrade?: DegradeLevel[];
  /** 手動の割り当て。装置の ID（GPU・キャプチャ機器、`GET /grid`）。`node` が無ければ、その装置を持つノードのどれか */
  device?: string | null;
  /** すべてに合うノードにだけ置く */
  labels?: LabelSelector[];
  /** 手動の割り当て。ノードの ID か名前（`GET /nodes`）。保存するときは ID に直す（応答は ID） */
  node?: string | null;
  /** 手動の割り当て先が止まったとき。prefer = 他へ移し、戻ったら（冷却の後に）戻す、strict = 止めたまま（`placement_status.state: unschedulable`） */
  pin_policy?: "strict" | "prefer";
  priority?: PlacementPriority;
  /** spread = 余裕の多いノードへ、binpack = 詰める */
  strategy?: "spread" | "binpack";
};

/** 保存した配置の指定（`node` は ID に直したもの）。無い項目・空の配列は省く。`pin_policy`・`priority`・`strategy` は既定値でも出る。 */
export type PlacementSpecView = {
  anti_affinity?: string;
  degrade?: DegradeLevel[];
  device?: string;
  labels?: LabelSelector[];
  node?: string;
  pin_policy: "strict" | "prefer";
  priority: PlacementPriority;
  strategy: "spread" | "binpack";
};

/**
 * 配置の結果（グリッドの割り当て）。`state` で形が変わる:
 * - `not_scheduled`: 仕事になっていない（入力・出力の `enabled: false`、パイプラインの `desired_state: stopped`）。
 * - `running`・`degraded`: 割り当てた（`degraded` は縮退の段 `level` が 1 以上）。割り当てたノードだけが動かす（実際の実行の様子は `status`・`observed_state`）。
 * - `pending`: 置き場所を探している（作成・変更の直後。配置係は約 1 秒ごと、資源の変更のたびに計画する）。
 * - `unschedulable`: 置けるノードが無い（`reasons` に理由ごとのノード）。
 */
export type PlacementStatus = {
  state: "not_scheduled";
} | {
  /** 使う GPU の ID */
  device: string | null;
  /** 今の縮退の段（0 = full、i は `placement.degrade[i-1]`） */
  level: number;
  /** 段の名前（段 0 は `full`） */
  level_label: string;
  /** 割り当てたノードの ID */
  node: string;
  /** そのノードの名前 */
  node_name: string | null;
  reason: GridReason;
  /** このノードに置いた時刻 */
  since: Timestamp;
  state: "running" | "degraded";
} | {
  /** 置けない理由（`unschedulable` のとき。`pending` は null） */
  reasons: RejectCount[] | null;
  state: "pending" | "unschedulable";
};

export type Plan = {
  outputs: PlanOutput[];
  reason?: string;
  state: "provisional" | "ready";
};

export type PlanOutput = {
  added_latency_ms?: number;
  code?: "UNSUPPORTED_CODEC";
  container?: "ts" | "fmp4";
  mode: "passthrough" | "remux" | "repacketize" | "transcode" | "unsupported";
  output: "hls" | "webrtc" | "recording" | "rtmp";
  reason?: string;
  to?: string;
  track: "video" | "audio" | "data";
};

export type PlaybackRequest = {
  /** 既定 300、上限は `--max-playback-ttl` */
  ttl_secs?: number | null;
};

/** RFC 9457 の problem details（仕様 10.4）。内部の詳細や秘密は入れない。 */
export type Problem = {
  code: ProblemCode;
  /** code ごとの補足（例: 本文の解析エラーは `{reason}`、予約の衝突は `{limit, overlaps}`） */
  details?: unknown;
  /** `X-Request-Id` と同じ（Idempotency-Key の再送では、再送の要求の ID）。 */
  request_id?: string;
  retryable: boolean;
  status: number;
  /** 日本語の説明 */
  title: string;
  type: string;
};

export type ProblemCode = "INVALID_CONFIG" | "INVALID_REQUEST" | "UNSUPPORTED_PROTOCOL_VERSION" | "UNSUPPORTED_CODEC" | "UNSUPPORTED_MEDIA_TYPE" | "AUTH_FAILED" | "FORBIDDEN" | "NOT_FOUND" | "METHOD_NOT_ALLOWED" | "CONFLICT" | "IDEMPOTENCY_CONFLICT" | "PRECONDITION_FAILED" | "PRECONDITION_REQUIRED" | "QUOTA_EXCEEDED" | "RATE_LIMITED" | "CAPACITY_UNAVAILABLE" | "INPUT_TIMEOUT" | "OWNERSHIP_LOST" | "PAYLOAD_TOO_LARGE" | "INTERNAL_ERROR";

export type Recording = {
  bytes: number | null;
  duration_ms: number | null;
  ended_at: string | null;
  gaps: Gap[] | null;
  generation: number;
  id: string;
  legal_hold: boolean;
  name: string | null;
  retention_until: string | null;
  /** 予約で作った録画なら予約の ID */
  schedule: string | null;
  segments: number | null;
  started_at: Timestamp;
  /** 削除中（deleting）の録画は API から見えない（404）。 */
  state: "recording" | "finalized" | "interrupted";
  /** 元の stream の ID（stream を消しても残る） */
  stream: string;
  /** `GET /recordings/{id}/thumbnail` があるか */
  thumbnail: boolean;
};

export type RecordingDeleting = {
  id: string;
  state: "deleting";
};

export type RecordingList = {
  items: Recording[];
  next_cursor: string | null;
};

export type RecordingPatch = {
  /** 解除には recording:delete が要る */
  legal_hold?: boolean;
  name?: string;
  /** RFC 3339。null で無期限。 */
  retention_until?: string | null;
};

export type RecordingPolicy = {
  /** publish が始まったら自動で録画する */
  enabled?: boolean;
  /** 保持日数（null は無期限） */
  retention_days?: number | null;
};

export type RecordingStart = {
  name?: string | null;
  retention_days?: number | null;
};

export type Recurrence = {
  count?: number | null;
  freq: "daily" | "weekly";
  /** IANA 名（例: Asia/Tokyo）。壁時計の時刻で繰り返す */
  timezone: string;
  /** 繰り返しの終わり（含まない、UTC ミリ秒） */
  until_ms?: number | null;
  /** weekly の曜日（0 = 日曜） */
  weekdays?: number[];
};

export type RejectCount = {
  /** そのノードの ID（8 個まで） */
  examples: string[];
  /** この理由で受けられないノードの数 */
  nodes: number;
  /** ノードが仕事を受けられない理由（`contended` は単独なら収まるが、優先度の同じか高い仕事で埋まっている） */
  reason: "node_down" | "not_media" | "draining" | "pinned_elsewhere" | "pin_target_down" | "labels" | "anti_affinity" | "no_device" | "capture" | "cpu" | "memory" | "net_in" | "net_out" | "gpu_encode" | "gpu_decode" | "gpu_memory" | "contended";
};

export type RelayStatus = {
  /** 中継で受けている写しの stream の ID */
  mirrors: string[];
  /** 中継の接続（相手のノードごと） */
  peers: Array<{
    /** この接続で送っている stream の数 */
    downstreams: number;
    /** どちらかが接続を閉じようとしている */
    going_away: boolean;
    /** 相手のクラスタのノード番号（クラスタ設定の `peers[].id`。API のノード ID とは別） */
    node: number;
    /** 送った AU 1 つ当たりの見出しのバイト数（枠の見出し・制御の枠を含む） */
    overhead_per_au: number;
    /** RTT（平滑化、ミリ秒）。まだ測っていなければ null */
    rtt_ms: number | null;
    /** 受けた AU の数 */
    rx_aus: number;
    /** 直近の受けの速さ（bit/s） */
    rx_bps: number;
    rx_bytes: number;
    /** 送った AU の数 */
    tx_aus: number;
    /** 直近の送りの速さ（bit/s） */
    tx_bps: number;
    tx_bytes: number;
    /** この接続で受けている stream の数 */
    upstreams: number;
  }>;
};

export type Resources = {
  /** CPU（ミリコア） */
  cpu_millis: number;
  memory_mb: number;
  net_in_mbps: number;
  net_out_mbps: number;
};

export type Room = {
  /** 参加トークンの発行で自動に作った部屋（誰もいないまま `empty_timeout_secs` 経つと消える） */
  auto: boolean;
  created_at: Timestamp;
  generation: number;
  id: RoomId;
  metadata: RoomMetadata;
  /** 配置の指定。指定が無ければ null */
  placement: PlacementSpecView | null;
  placement_status: PlacementStatus;
  settings: RoomSettings;
  /** つなぐ先（`GET /signaling` の WebSocket）。部屋を別のノードに置いたらそのノードの URL。このノードの `--public-http-url` が無く、別のノードにも置いていなければ null */
  signaling_url: string | null;
  updated_at: Timestamp;
};

export type RoomCreate = {
  /** 省けば作る（24 文字の 16 進） */
  id?: RoomId;
  metadata?: RoomMetadata;
  /** 配置の指定（`degrade` は使えない） */
  placement?: PlacementSpec | null;
  /** 部屋の設定（省略・null は既定値） */
  settings?: RoomSettings | null;
};

/** 部屋の ID（1〜128 文字の英数字と `-_.:@`、テナントをまたいで一意） */
export type RoomId = string;

export type RoomLayerStats = {
  bitrate_bps: number | null;
  /** 層（層の無いトラックは null） */
  rid: string | null;
};

export type RoomList = {
  items: Room[];
  next_cursor: string | null;
};

export type RoomMessage = {
  /** 送るもの（JSON で 64 KiB まで）。`label` があれば文字列 */
  data: unknown;
  /** DataChannel のラベル（`#` で始まる）。無ければシグナリングの `push` */
  label?: string | null;
  /** 送る相手の connection_id（空なら全員） */
  to?: string[];
};

export type RoomMessageResult = {
  /** 送った相手の数 */
  delivered: number;
};

/** 部屋・参加者の metadata（任意の JSON、JSON にして 8 KiB まで）。参加者の metadata は他の参加者に見える */
export type RoomMetadata = unknown;

export type RoomParticipantStats = {
  /** 受け手への推定帯域 */
  bandwidth_bps: number;
  /** クライアントが送った getStats の要約（無ければ省く） */
  client?: Record<string, unknown>;
  connection_id: string;
  forwarded_bytes: number;
  forwarded_frames: number;
  nack_requested: number;
  published: Array<{
    kind: "audio" | "video";
    layers: RoomLayerStats[];
    track_id: string;
  }>;
  received_bytes: number;
  retransmitted_packets: number;
  rtt_ms: number;
  /** この参加者へ送っている量 */
  send_bitrate_bps: number;
  subscriptions: Array<{
    mid: string;
    /** 今配っている層（層の無いトラック・まだなら null） */
    rid: string | null;
    state: "empty" | "active" | "paused";
    track_id: string | null;
  }>;
};

/** 書いたフィールドだけ変える。`settings` は全体を置き換える。`settings`・`metadata` の null は「変えない」、`placement: null` は配置の指定を外す。 */
export type RoomPatch = {
  metadata?: RoomMetadata;
  /** 配置の指定を丸ごと置き換える（`degrade` は使えない）。null で外す */
  placement?: PlacementSpec | null;
  settings?: RoomSettings | null;
};

/** 参加者に許すこと（省けばすべて）。 */
export type RoomPermissions = {
  /** 使ってよい DataChannel のラベルの型（`#*` で全部） */
  data?: string[];
  /** 送ってよいトラックの種類 */
  publish?: Array<"audio" | "video">;
  /** 他の参加者のトラックを受けてよいか */
  subscribe?: boolean;
};

/**
 * 部屋の設定。応答では既定値を埋めた全体を返す（`max_video_bitrate_kbps`・`spotlight`・`forwarding_filter` は無ければ省く）。
 * API で検査するのは形と `max_participants` だけ（範囲の他の制約は部屋の核が部屋を開くときに検査する。`POST /rooms` の説明）。
 */
export type RoomSettings = {
  audio_codec?: "opus";
  /** 使ってよい DataChannel のラベルの型（`#` で始まる。末尾の `*` は前方一致） */
  data_labels?: string[];
  /** E2EE（SFrame）の部屋。SFU は中身を見ずに配り、合成・合成の録画はできない */
  e2ee?: boolean;
  /** 誰もいなくなってから消すまでの秒数（参加トークンで自動に作った部屋だけ） */
  empty_timeout_secs?: number;
  /** 部屋全体の転送フィルター（購読者ごとのフィルターと両方を満たすものだけ配る） */
  forwarding_filter?: ForwardingFilter | null;
  /** 参加者の上限（ノードをまたぐ参加者も数える） */
  max_participants?: number;
  /** 送り手に勧める映像の上限（kbit/s） */
  max_video_bitrate_kbps?: number | null;
  /** 録画の方針（`composite` は `e2ee` と併用できない） */
  recording?: "none" | "per_participant" | "composite";
  spotlight?: RoomSpotlight | null;
  /** 部屋の映像のコーデック（SFU は変換しないので全員が同じ） */
  video_codec?: "vp8" | "vp9" | "h264" | "av1";
};

/** 受信の枠（クライアントの受ける m 行 1 つ）と、そこに今流しているトラック。 */
export type RoomSlot = {
  client_id?: string;
  connection_id?: string;
  kind: "audio" | "video";
  mid: string;
  /** active = 配っている、paused = 割り当てているが配っていない、empty = 空き */
  state: "empty" | "active" | "paused";
  track_id?: string;
};

/** スポットライト（話している人を注目し、注目の外の映像を下げる）。 */
export type RoomSpotlight = {
  /** 注目した送り手の層（無ければ帯域で自動） */
  focus_rid?: string;
  /** 注目する話者の数（受け手は自分を除いてこの人数） */
  number: number;
  /** lowest = 注目の外は一番低い層、off = 配らない（枠は `paused`） */
  unfocused?: "lowest" | false;
};

export type RoomStats = {
  at: Timestamp;
  /** 部屋を置いたノードにつないだ参加者 */
  participants: RoomParticipantStats[];
  room_id: RoomId;
};

export type RoomToken = {
  expires_at: Timestamp;
  room_id: RoomId;
  /** `Room.signaling_url` と同じ */
  signaling_url: string | null;
  /** 参加トークン（JWT）。Idempotency-Key の再送では null */
  token: string | null;
  /** Idempotency-Key の再送で値を伏せたとき */
  token_redacted?: true;
};

export type RoomTokenRequest = {
  /** 同じ値の接続どうしは互いのトラックを受けない */
  bundle_id?: string | null;
  /** あれば `connect.client_id` はこれと同じでなければならない（転送フィルターの条件になる） */
  client_id?: string | null;
  /** 参加者の metadata（他の参加者に見える） */
  metadata?: RoomMetadata;
  permissions?: RoomPermissions | null;
  /** 許す役割の上限（`sendrecv` のトークンはどの役割でも参加できる） */
  role: "sendrecv" | "sendonly" | "recvonly";
  /** 有効期間（秒） */
  ttl_secs?: number | null;
};

export type RoomTrack = {
  /** 送り手の接続 */
  connection_id: string;
  kind: "audio" | "video";
  /** サイマルキャストの層（低い順）。サイマルキャストでなければ省く */
  rids?: string[];
  track_id: string;
};

export type Schedule = {
  action: ScheduleAction;
  created_at: Timestamp;
  end_at: Timestamp;
  generation: number;
  grace_ms: number;
  id: string;
  last_run: ScheduleRun | null;
  name: string | null;
  post_roll_ms: number;
  pre_roll_ms: number;
  recurrence: Recurrence | null;
  retention_days: number | null;
  snapshot_interval_secs: number | null;
  start_at: Timestamp;
  state: ScheduleState;
  stream: string;
};

/** record = 窓の間録画、live = 窓の間だけ publish を受け付ける、snapshot = 窓の間一定間隔で静止画 */
export type ScheduleAction = "record" | "live" | "snapshot";

export type ScheduleCheck = {
  ok: true;
  /** 同じ stream で時間が重なる録画予約（pre/post roll を含む） */
  overlaps: Array<{
    end_at: Timestamp;
    id: string;
    start_at: Timestamp;
  }>;
  /** 重なりがあり、入力を共有するか */
  shared_input: boolean;
};

export type ScheduleCreate = {
  action: ScheduleAction;
  /** RFC 3339。start_at より後、窓は 24 時間まで、未来 */
  end_at: string;
  /** 既定 60000。実行時刻を過ぎてもこの間なら途中から実行 */
  grace_ms?: number | null;
  name?: string | null;
  /** 既定 0 */
  post_roll_ms?: number | null;
  /** 既定 0 */
  pre_roll_ms?: number | null;
  recurrence?: Recurrence | null;
  retention_days?: number | null;
  /** action が snapshot のときだけ（必須） */
  snapshot_interval_secs?: number | null;
  /** RFC 3339 */
  start_at: string;
  stream: string;
};

export type ScheduleList = {
  items: Schedule[];
  next_cursor: string | null;
};

export type SchedulePatch = {
  end_at?: string | null;
  grace_ms?: number | null;
  name?: string | null;
  post_roll_ms?: number | null;
  pre_roll_ms?: number | null;
  retention_days?: number | null;
  snapshot_interval_secs?: number | null;
  start_at?: string | null;
  state?: "canceled" | null;
};

export type ScheduleRun = {
  end_at: Timestamp;
  occurrence: number;
  reason: string | null;
  recording: string | null;
  start_at: Timestamp;
  state: ScheduleState;
};

export type ScheduleRunList = {
  items: ScheduleRun[];
};

export type ScheduleState = "pending" | "running" | "completed" | "missed" | "canceled" | "failed";

export type Scope = "stream:read" | "stream:write" | "publish" | "subscribe" | "recording:read" | "recording:delete" | "tenant:admin" | "node:admin";

export type Secret = {
  created_at: Timestamp;
  id: string;
  name: string;
  /** `credential_ref` に書く値 */
  ref: string;
};

export type SecretCreate = {
  name: StreamName;
  /** 1〜8192 バイト（UTF-8）。応答には出ない */
  value: string;
};

export type SecretList = {
  items: Secret[];
  /** 常に null（ページングは無い） */
  next_cursor: null;
};

export type SnapshotList = {
  items: Array<{
    /** JPEG の大きさ */
    bytes: number;
    height: number;
    id: string;
    /** 撮った理由 */
    reason: "interval" | "schedule" | "board_change";
    taken_at: Timestamp;
    width: number;
  }>;
};

export type SnapshotPolicy = {
  interval_secs: number;
  keep: number;
};

export type SseEventData = {
  /** 種類ごとの中身（秘密は含まない）。部屋の核の出来事は `{type, data}` */
  data: Record<string, unknown>;
  /** 部屋の核の出来事の ID（部屋の核の出来事だけ。SSE の `id` は無い） */
  event_id?: string;
  /** 出来事を出したノードの ID（部屋の核の出来事だけ） */
  node?: string;
  occurred_at: Timestamp;
  /** 資源の ID（部屋の出来事は部屋の ID、`grid.decisions` は空） */
  resource_id: string;
  type: "stream.created" | "stream.updated" | "stream.deleted" | "stream.owner" | "recording.updated" | "recording.deleted" | "schedule.created" | "schedule.updated" | "schedule.deleted" | "schedule.run" | "node.updated" | "input.updated" | "input.deleted" | "output.updated" | "output.deleted" | "secret.created" | "secret.deleted" | "pipeline.updated" | "pipeline.deleted" | "room.updated" | "room.deleted" | "media_pipeline.updated" | "media_pipeline.deleted" | "node.down" | "node.up" | "device.down" | "device.up" | "device.disabled" | "device.enabled" | "grid.decisions" | "room.started" | "room.stopped" | "room.connection.created" | "room.connection.updated" | "room.connection.destroyed" | "room.track.published" | "room.track.unpublished" | "room.spotlight.changed";
};

export type Stream = {
  /** 字幕と翻訳の設定（無ければ null）。資格情報は `secret:{name}` の参照で、値は返さない。 */
  captions: CaptionSettings | null;
  codec_profile: CodecProfile;
  created_at: Timestamp;
  desired_state: "running" | "stopped";
  generation: number;
  hls: HlsSettings;
  id: string;
  ingest: Ingest;
  latency_profile: LatencyProfile;
  limits: Limits;
  live: LiveInfo | null;
  metadata: Metadata;
  name: string;
  /** 仕様 10.4 の provisioning・draining・degraded・failed は今は出さない。 */
  observed_state: "waiting_input" | "live" | "stopped";
  recording: RecordingPolicy;
  snapshots: SnapshotPolicy | null;
  updated_at: Timestamp;
};

export type StreamCreate = {
  captions?: CaptionSettings | null;
  codec_profile?: CodecProfile;
  desired_state?: "running" | "stopped";
  hls?: HlsSettings;
  latency_profile?: LatencyProfile;
  limits?: Limits;
  metadata?: Metadata;
  name: StreamName;
  recording?: RecordingPolicy;
  snapshots?: SnapshotPolicy | null;
};

export type StreamList = {
  items: Stream[];
  next_cursor: string | null;
};

export type StreamName = string;

/** 書いたフィールドだけ置き換える。`snapshots: null` は静止画の方針を、`captions: null` は字幕を消す。他のフィールドの null は「変えない」。 */
export type StreamPatch = {
  captions?: CaptionSettings | null;
  codec_profile?: CodecProfile;
  desired_state?: "running" | "stopped";
  hls?: HlsSettings;
  latency_profile?: LatencyProfile;
  limits?: Limits;
  metadata?: Metadata;
  name?: StreamName;
  recording?: RecordingPolicy;
  snapshots?: SnapshotPolicy | null;
};

export type Timestamp = string;

export type Token = {
  created_at: Timestamp;
  expires_at: string | null;
  id: string;
  name: string;
  scopes: Scope[];
  /** 値。発行の応答でだけ入る（一覧では null）。 */
  token: string | null;
  /** Idempotency-Key の再送で値を伏せたとき */
  token_redacted?: true;
};

export type TokenCreate = {
  /** RFC 3339（未来） */
  expires_at?: string | null;
  name: string;
  scopes: Scope[];
};

export type TokenList = {
  items: Token[];
  next_cursor: string | null;
};

export type TranscodeCapability = {
  codec: string;
  device: string;
  direction: "encode" | "decode";
  hardware: boolean;
  max_height: number;
  max_sessions: number | null;
  max_width: number;
  provider: string;
};

export type VodPlayback = {
  expires_at: Timestamp;
  playback: {
    /** `{http}/s/{exp}/{sig}/vod/{id}/index.m3u8` */
    hls: string;
    /** 今は常に null */
    mp4: null;
  };
};

export {};
