// 生成物: apps/sdk-js/scripts/gen_types.py が docs/api/openapi.yaml から作る。手で直さない。
// 管理 API v1 の components.schemas（135 個）。

/**
 * 監査ログの 1 件（`audit.log` の 1 行に `node`・`node_name` を足したもの）。秘密情報やリクエストの本文は含みません。
 * 操作によってフィールドが増えることがあります。
 */
export type AuditEntry = {
  /** 操作の種類（`stream.create`・`token.delete`・`node.drain` など。`{資源}.{操作}` の形） */
  action: string;
  /** 記録したノード（要求を受けたノード）の ID */
  node: string;
  /** 記録したノードの名前 */
  node_name: string;
  /** 要求 ID（応答の `X-Request-Id`） */
  request_id: string;
  /** 対象の資源の ID（装置・参加者などは `{ノード}/{装置}`・`{部屋}/{接続}` の形） */
  resource: string;
  /** 操作した主体。API トークンなら `token:{ID}`、JWT なら `sub` の値 */
  subject: string;
  tenant: string;
  ts: Timestamp;
};

export type AuditList = {
  items: AuditEntry[];
  next_cursor: string | null;
  /** 時間内に答えなかったノードの ID（その記録は含まれません。単一ノードでは常に空） */
  unavailable_nodes: string[];
};

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
  /** 使用できる入力の種類 */
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
    /** MoQ（draft-22）による購読と配信（`--quic`） */
    moq?: boolean;
    rtmp_play: boolean;
    whep: boolean;
  };
  /** 使用できる出力の種類 */
  outputs?: string[];
  paas_providers?: string[];
  /** パイプラインが使えるか（コーデック worker が動いているとき） */
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
 * 字幕と翻訳の設定です。配信中の音声を `asr_endpoint` に送って文字に起こし、`targets` の言語へ翻訳して、ライブ HLS に WebVTT の字幕を追加します。
 * 検査: `asr_provider`・`translate_provider` の値、`asr_endpoint`・`translate_endpoint` が URL として解釈できること、言語（`language` と `targets`）が英数字と `-` の 1〜8 文字であること（`targets` は 8 個まで）。違反すると 400（`INVALID_CONFIG`）です。
 * `asr_credential_ref`・`translate_credential_ref` は `secret:{name}` の形式で、テナントにその秘密値が登録されている必要があります（無ければ 400）。参照されている秘密値は削除できません（409）。
 */
export type CaptionSettings = {
  asr_credential_ref?: CredentialRef;
  /** 送信先の URL（例: `https://api.openai.com/v1`、社内の whisper サーバー） */
  asr_endpoint: string;
  asr_model?: string | null;
  /** openai = OpenAI 互換の `/audio/transcriptions`、whisper_cpp = whisper.cpp のサーバー */
  asr_provider: "openai" | "whisper_cpp";
  /** 話されている言語（ISO 639-1） */
  language?: string;
  /** 翻訳先の言語 */
  targets?: string[];
  translate_credential_ref?: CredentialRef;
  translate_endpoint?: string | null;
  translate_model?: string | null;
  /** 翻訳サービス（省略すると翻訳しません）。指定する場合は translate_endpoint が必要です */
  translate_provider?: "anthropic" | "deepl" | "openai" | null;
};

/** royalty_free は H.264・H.265・AAC を理由付きで拒否します。royalty_free では hls.container を fmp4 にしてください。 */
export type CodecProfile = "any" | "royalty_free";

/** `secret:{name}`（`POST /secrets` で登録した秘密値の名前）。存在しない秘密値や形式の誤りは 400 です。 */
export type CredentialRef = string | null;

export type DegradeLevel = {
  /** この段階で停止する処理ノードの id（グラフに無ければ 400）。下流の処理ノードは、入力がすべて停止すると停止します。段階は累積します（段階 2 では段階 1 の分も停止します） */
  disable: PipelineNodeId[];
  /** 段階の名前（例: `no_translation`・`audio_only`）。`full`（段階 0 の名前）や他の段階と同じ名前は使えません */
  label: string;
};

export type DeviceHealth = {
  device: string;
  /** disable は false、enable は true */
  enabled: boolean;
  /** ノードの ID（名前で指定した場合も ID で返します） */
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
     * このノードのキャプチャ機器です（分離された `senca-capture-worker` に問い合わせます。入力の `protocol: device` で使います）。
     * worker が無い場合や失敗した場合は `{available: false, reason}`、ランタイムが無ければ null です。
     */
    capture: {
      /** バックエンド（V4L2・ALSA・DeckLink・合成）ごとの利用可否 */
      backends: Array<Record<string, unknown>>;
      /** 機器の一覧（`id` を入力の `url: device:{id}` に指定します） */
      devices: Array<Record<string, unknown>>;
      version: number;
    } | {
      available: false;
      reason: string;
    } | null;
    /** コーデック worker のプールが動いていなければ、`providers` は空、`sandbox` は null です。 */
    devices: DeviceStatus;
    node: string;
  }>;
};

/** 転送フィルター（`docs/realtime/signaling.md` の 8 章）。`rules` は「AND 条件の組」の OR です。`block` は一致したものを配信せず、`allow` は一致したものだけを配信します。 */
export type ForwardingFilter = {
  action: "block" | "allow";
  rules: ForwardingRule[][];
};

export type ForwardingRule = {
  /** 配信者の属性: `connection_id`・`client_id`・`bundle_id`・`kind`・`metadata.<key>`（key は 1〜64 文字。値が文字列・数値・真偽値のとき） */
  field: string;
  operator: "is_in" | "is_not_in";
  /** `kind` の場合は `audio`・`video` */
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
    /** ワークロードに割り当てられる量（`--capacity-cpu-millis`・`--capacity-memory-mb`・`--net-mbps`。既定は実機の 90%・85%・10000） */
    capacity: Resources;
    capture: Array<{
      /** 運用者が手動で無効にした（無効のときだけ含まれます） */
      disabled?: boolean;
      healthy: boolean;
      /** 機器の ID（`GET /devices` の `capture.devices[].id`。例: `synthetic:0`） */
      id: string;
      /** 機器の種類（機器の一覧の `backend`。例: `synthetic`・`v4l2`） */
      kind: string;
    }>;
    draining: boolean;
    gpus: Array<{
      decode_reserved: number;
      decode_sessions: number;
      /** 運用者が手動で無効にしていないかどうか（`disable`・`enable`）。ワークロードは healthy かつ enabled の装置にだけ配置します */
      enabled: boolean;
      encode_reserved: number;
      encode_sessions: number;
      /** 計測による健全性（ハートビートで更新されます） */
      healthy: boolean;
      /** `{provider}:{装置の番号}`（例: `nvidia:0`） */
      id: string;
      /** `nvidia`・`intel`・`amd`・`vaapi` など */
      vendor: string;
    }>;
    id: string;
    /** `--label` の値と、予約済みのラベル `senca.io/name`（ノードの名前） */
    labels: { [key: string]: string };
    /** 最後のハートビート（スケジューラーのノードだけが把握しており、それ以外では null） */
    last_heartbeat: Timestamp | null;
    /** ノードの名前（`GET /nodes` に無ければ null） */
    name: string | null;
    /** 割り当てたワークロードの要求（現在の段階）の合計 */
    reserved: Resources;
    /** control = Raft の投票者、media = ワークロードを実行する（ワークロードは media のノードにだけ配置します） */
    roles: Array<"control" | "media">;
    /** ハートビートが届いているかどうか（5 秒途絶えると false） */
    up: boolean;
    /** 最後のハートビートで報告された実際の使用量（CPU は /proc/stat の差分、メモリは /proc/meminfo の MemTotal − MemAvailable、帯域はループバックを除く /proc/net/dev の差分）。スケジューラーのノードだけが把握しており、それ以外では null です。配置は予約（reserved）で計算し、この値は使いません */
    usage: Resources | null;
    /** このノードに割り当てたワークロードの数 */
    workloads: number;
  }>;
  /** 応答したノードがスケジューラー（リーダー）であればその ID、そうでなければ null */
  scheduler: string | null;
  workloads: Array<{
    device: string | null;
    /** 資源から求めたワークロードの ID（`input:{id}`・`output:{id}`・`pipeline:{id}`・`room:{id}`） */
    id: string;
    /** 入力は `ingest`（`device` は `capture`）、出力は `egress`、パイプラインは `pipeline`、部屋は `room` */
    kind: "ingest" | "room" | "pipeline" | "recording" | "transcode" | "capture" | "relay" | "media_pipeline" | "egress";
    /** 現在の縮退の段階（0 = full） */
    level: number | null;
    level_label: string | null;
    node: string | null;
    /** `placement.node`・`placement.device` で手動で割り当てたかどうか */
    pinned: boolean;
    priority: PlacementPriority;
    reason: GridReason | null;
    state: "running" | "degraded" | "unschedulable" | "pending";
    unschedulable: RejectCount[] | null;
  }>;
};

/** 割り当て・決定の理由（`code` で種類を表します。`node_down` は `node`、`device_down` は `device`、`preempted` は `by` を持ちます）。 */
export type GridReason = {
  /** 退去させたワークロードの ID */
  by?: string;
  /**
   * placed = 通常どおり配置した、no_capacity = 資源が足りず縮退した段階で配置した・縮退した、node_down = ノードが停止した・削除された、device_down = 装置が故障した（手動で無効にした場合も含む）、
   * draining = ノードの drain で移動した、constraint_changed = 条件・ラベルが変わって合わなくなった、preempted = 優先度の高いワークロードのために縮退・退去させられた、
   * overcommitted = ノードの容量が減って収まらなくなった、capacity_returned = 資源が戻ったので元の段階へ戻した、pin_returned = 手動の割り当て先が戻ったので戻した、
   * pin_target_down = 手動の割り当て先（strict）が停止している。
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
  /** RTMP の publish 先（`{public-rtmp-url}/live`）。stream name には stream_key を使います。 */
  rtmp_url: string;
  srt_streamid_format?: "publish:{stream_key}";
  /** `--public-srt-url` を設定したとき */
  srt_url?: string;
  /** publish 鍵。作成時と再発行時の応答でだけ値が入ります（それ以外は null）。 */
  stream_key: string | null;
  /** Idempotency-Key による再送で鍵を伏せたとき */
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
  /** このノードでの実行の状態。`enabled: false` の場合、実行がまだ始まっていない場合、またはこのノードに割り当てられていない場合は null です（クラスターでは、割り当てたノード `placement_status.node` に問い合わせてください）。作成・PATCH の応答では、実行の作り直しが非同期で行われるため、以前の実行の状態（または null）のことがあります。 */
  status: IoStatus | null;
  stream: string;
  url: string;
};

export type InputCreate = {
  credential_ref?: CredentialRef;
  enabled?: boolean | null;
  options?: IoOptions;
  /** 配置の指定（`degrade` は使えません） */
  placement?: PlacementSpec | null;
  protocol: "rtmp_pull" | "rtsp_pull" | "srt_pull" | "hls_pull" | "device";
  /**
   * 取得元の URL。スキームは protocol によって決まります（rtmp_pull: `rtmp`・`rtmps`、rtsp_pull: `rtsp`・`rtsps`、srt_pull: `srt`、hls_pull: `http`・`https`、device: `device:{機器 ID}`（300 文字まで））。
   * ホストが必要です。空白・制御文字は使えません。資格情報（`user:pass@`）は含められません（`credential_ref` を使います）。
   */
  url: string;
};

export type InputList = {
  items: Input[];
  next_cursor: string | null;
};

/** プロトコルごとの設定（例: rtsp_pull の `allow_basic: "true"`・`tls_spki_sha256`（rtsps で証明書の公開鍵を固定します。SHA-256 の base64 か 16 進）、device の `width`・`height`・`fps`・`codec`・`bitrate_bps`・`audio_device`・`audio_codec`）。16 個まで、キーは 64 文字・値は 256 文字までです。RTSP は TCP interleaved で接続します。 */
export type IoOptions = { [key: string]: string };

/** 指定したフィールドだけを置き換えます。`credential_ref: null` で参照を、`placement: null` で配置の指定を削除します（スケジューラーに任せます）。他のフィールドの null は「変更しない」の意味です。 */
export type IoPatch = {
  credential_ref?: CredentialRef;
  enabled?: boolean | null;
  options?: IoOptions | null;
  /** 配置の指定の全体を置き換えます（一部だけの変更はできません。`degrade` は使えません）。null で削除します */
  placement?: PlacementSpec | null;
  url?: string | null;
};

/** このノードでの実行の状態です（制御状態には保存されません）。 */
export type IoStatus = {
  /** 送受信したバイト数（目安） */
  bytes: number;
  /** 接続に成功した回数 */
  connects: number;
  /** 最後の失敗の理由（接続先が拒否された場合は「… は許されていない宛先です」）。その後成功しても消えません */
  last_error: string | null;
  /** 現在の state になった時刻 */
  since: string | null;
  /**
   * starting = 実行を開始した、provisioning = PaaS の資源を準備している、waiting_input = 出力が配信の開始を待っている、connecting = 接続中、
   * active = 送受信している、backoff = 失敗の後の待機中（1〜60 秒の指数バックオフ）、error = 失敗した（すぐに backoff になる）、ended = VOD の HLS を最後まで読んだ（`hls_pull` だけ）。
   */
  state: "starting" | "provisioning" | "waiting_input" | "connecting" | "active" | "backoff" | "error" | "ended";
};

/** ノードのラベル（`--label`、予約済みの `senca.io/name`）の条件です。`op` で種類を指定します。キーと値は 1〜128 文字で、制御文字を含めることはできません。 */
export type LabelSelector = {
  key: string;
  op: "eq";
  value: string;
} | {
  key: string;
  op: "in" | "not_in";
  values: string[];
};

/** standard = HLS、low = LL-HLS（part は min(500 ms, segment_ms / 4)）、interactive = WebRTC・MoQ 向け（LL-HLS の part は 200 ms。開始時の待ち時間と送信のバッファーを小さくします） */
export type LatencyProfile = "standard" | "low" | "interactive";

export type Limits = {
  /** 入力のビットレートの上限（直近 3 秒の平均）。5 秒続けて超えると配信者を切断し、その後 10 秒間は新しい配信者を受け付けません */
  max_ingress_bps?: number | null;
  /** ノードごとの同時の視聴者の上限（最近プレイリストを取得した HLS のクライアントと、RTMP・SRT・RTSP・WHEP・MoQ の接続の合計）。上限を超える新しい視聴は断ります（HLS・WHEP・RTSP は 503） */
  max_viewers?: number | null;
};

export type LiveInfo = {
  bitrate_bps: number;
  /** 再接続のたびに増えます */
  epoch: number;
  started_at: Timestamp;
  tracks: Array<Record<string, unknown>>;
  /** rtmp・srt など */
  transport: string;
  /** このノードの視聴者の数（`limits.max_viewers` で数えるもの）。`hls` は最近プレイリストを取得した HLS のクライアントの数です */
  viewers?: {
    hls?: number;
    moq?: number;
    rtmp?: number;
    rtsp?: number;
    srt?: number;
    total?: number;
    webrtc?: number;
  };
  /** ストリームの内部の購読の数（中継・録画以外の内部の購読も含む目安） */
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
    /** `{http}/whep/{id}?exp=&sig=`（`--webrtc-udp` を設定していないノードでは null） */
    whep: string | null;
  };
};

export type MediaPipeline = {
  artifacts: MediaPipelineArtifacts;
  /** 資格情報は `secret:{name}` で参照し、値は返しません */
  captions: CaptionSettings | null;
  composite: MediaPipelineComposite | null;
  created_at: Timestamp;
  desired_state: MediaPipelineState;
  egress: MediaPipelineEgress[];
  export_participants: boolean;
  generation: number;
  id: string;
  /** 部屋の設定 `recording` から作られたものかどうか（ID は `room-rec.{部屋の ID}`。API から変更・削除はできません） */
  managed?: boolean;
  name: MediaPipelineName;
  participants: MediaPipelineParticipants;
  recording: MediaPipelineRecording | null;
  room: string;
  /**
   * このノードでの実行の状態。部屋がこのノードに配置されていない場合、`stopped` の場合、作成直後でまだ確認が行われていない場合は null です
   * （クラスターでは、部屋を配置したノードに問い合わせてください）。停止・削除の直後の 1 秒ほどは、以前の状態が残ることがあります。
   */
  status: MediaPipelineStatus | null;
  updated_at: Timestamp;
};

/** 子の資源 */
export type MediaPipelineArtifacts = {
  /** `composite` が無ければ null */
  composite: MediaPipelineCompositeArtifacts | null;
  /** 書き出した参加者ごとの stream（`metadata.media_pipeline` がこの ID の stream。現在いない参加者のものも残ります）。ID の昇順 */
  participant_streams: MediaPipelineParticipantStream[];
};

/**
 * 合成の設定です（会議全体を 1 本の映像・音声にします）。指定しなかったフィールドは既定値になります（応答ではすべて含まれます）。映像のある参加者を並べ、音声はすべてミックスして Opus 96 kbit/s にします。
 * `grid` は格子状（列数は人数の平方根の切り上げ）、`speaker` は最初に書き出した参加者を上に大きく表示し、残りを下の 1/4 の帯に並べます（話者の検出結果は、まだ合成に使っていません）。
 * H.264・H.265 には GPU が必要です。GOP は `fps` の 2 倍です。
 */
export type MediaPipelineComposite = {
  bitrate_kbps?: number;
  fps?: number;
  height?: number;
  layout?: "grid" | "speaker";
  video_codec?: "av1" | "vp9" | "vp8" | "h264" | "h265";
  width?: number;
};

/** 合成の子の資源の ID（メディアパイプラインの ID から決まります。まだ作成していなくても ID は表示されます） */
export type MediaPipelineCompositeArtifacts = {
  /** `egress` の順に並んだ外部配信の出力の ID（`GET /outputs/{id}`） */
  outputs: string[];
  /** 合成のパイプラインの ID（`GET /pipelines/{id}`。映像のある参加者が流れ始めるまでは存在しないことがあります） */
  pipeline: string;
  /** 合成の stream の ID（`GET /streams/{id}`） */
  stream: string;
};

export type MediaPipelineCreate = {
  /** 文字起こし・翻訳（AWS の media insights pipeline に相当）。参加者ごとの stream の `captions` として設定されます（省略または null の場合は設定しません） */
  captions?: CaptionSettings | null;
  /** 合成（省略または null の場合は合成しません） */
  composite?: MediaPipelineComposite | null;
  /** 省略または null の場合は running */
  desired_state?: MediaPipelineState | null;
  /** 外部への配信（`composite` が必要） */
  egress?: MediaPipelineEgress[];
  /** 参加者ごとの stream を成果物として残します（AWS の media stream pipeline に相当）。false の場合も合成のために書き出しますが、`recording.participants`・`captions` は使えません */
  export_participants?: boolean | null;
  name: MediaPipelineName;
  participants?: MediaPipelineParticipants;
  /** 録画（省略または null の場合は録画しません） */
  recording?: MediaPipelineRecording | null;
  /** 部屋の ID（テナントの部屋。無ければ 400） */
  room: string;
};

/**
 * 外部への配信の設定です（AWS の media live connector pipeline に相当）。合成の stream の出力（`Output`）として作成します。形式と検査は `OutputCreate` と同じです
 * （`paas` には `credential_ref` が必要です）。応答では、null や空のフィールドは省かれます。
 */
export type MediaPipelineEgress = {
  credential_ref?: CredentialRef;
  options?: IoOptions;
  protocol: "rtmp_push" | "srt_push" | "paas";
  /** `paas` で必須 */
  provider?: "cloudflare_stream" | "aws_ivs" | "aws_medialive" | null;
  /** `paas` 以外で必須。rtmp_push: `rtmp`・`rtmps`、srt_push: `srt`。資格情報は含められません。 */
  url?: string | null;
};

export type MediaPipelineList = {
  items: MediaPipeline[];
  next_cursor: string | null;
};

/** テナント内で一意（作成時）。1〜40 文字の英数字と `-_.`（`.` で始まらないこと）。子の stream・パイプラインの名前の先頭に使われます */
export type MediaPipelineName = string;

export type MediaPipelineParticipantStream = {
  /** stream の名前（`{name}-{参加者}-{ID の先頭 6 文字}`） */
  name: string;
  /** stream の ID */
  stream: string;
};

/** 書き出す参加者の `client_id`（参加トークンの `client_id`）。空なら全員（`client_id` の無い参加者は `connection_id` で区別します） */
export type MediaPipelineParticipants = string[];

/** 指定したフィールドだけを置き換えます。`composite`・`recording`・`captions` の null は設定の削除、他のフィールドの null は「変更しない」の意味です。`egress` は配列全体を置き換えます。 */
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
 * 録画の設定です（AWS の media capture pipeline に相当）。書き出した stream の自動録画（`RecordingPolicy`）を有効にします。録画は通常の録画の API（`GET /recordings?stream=...`）で取得します。
 * 録画の設定は stream の作成時に決まります（後から `recording` を変更しても、作成済みの stream には反映されません）。
 */
export type MediaPipelineRecording = {
  /** 合成の stream を録画します（`composite` が必要） */
  composite?: boolean;
  /** 参加者ごとの stream を録画します（`export_participants: true` が必要） */
  participants?: boolean;
  /** 保持日数（省略すると録画の既定値。省略した場合は応答にも項目がありません） */
  retention_days?: number | null;
};

/** running = 部屋を配置したノードが実行する、stopped = 停止する（子の合成・外部配信を削除します。書き出した stream と録画は残ります） */
export type MediaPipelineState = "running" | "stopped";

/** このノードでの実行の状態です（制御状態には保存されません）。 */
export type MediaPipelineStatus = {
  /** `speaker` の並びで現在大きく表示している参加者（`client_id`、無ければ `connection_id`）。決まる前と `grid` では null */
  active_speaker?: string | null;
  /** 合成のグラフに入っている参加者の数（`composite` が無い場合、または映像のある参加者がまだ流れ始めていない場合は null） */
  composite_members: number | null;
  /** 現在書き出している参加者 */
  participants: Array<{
    /** `client_id`（無ければ `connection_id`） */
    participant: string;
    /** 書き出し先の stream の ID */
    stream: string;
    /** 映像を送信しているかどうか */
    video: boolean;
  }>;
};

export type Metadata = { [key: string]: string };

export type Node = {
  draining: boolean;
  id: string;
  labels: { [key: string]: string };
  /** 配信中の stream の数（他のノードについては現在常に 0） */
  live_streams: number;
  name: string;
  registered_at: Timestamp;
  /** ノード間の中継（ADR-0014）の状態。応答したノード自身（`self: true`）で、クラスターを構成しているときだけ値が入ります（単一ノードや他のノードでは null）。 */
  relay: RelayStatus | null;
  /** このノードのリアルタイムの部屋の状態（カスケードの役割・イベント Webhook の配送）。応答したノード自身で、WebRTC（`--webrtc-udp`）が有効なときだけ値が入ります（それ以外は null）。 */
  rooms?: NodeRoomsStatus | null;
  /** 応答したノード自身かどうか */
  self: boolean;
};

export type NodeList = {
  items: Node[];
};

export type NodeRoomsStatus = {
  /** イベント Webhook の配送の状況（`--rooms-event-webhook` を設定したときだけ） */
  event_webhook?: {
    /** 再送をあきらめたイベントの数 */
    dead_letters: number;
    delivered: number;
    /** 待ち行列があふれて捨てたイベントの数 */
    dropped: number;
    pending: number;
    /** 直近にあきらめたイベントの ID（新しい順、最大 20） */
    recent_dead_letters: string[];
  };
  /** ほかのノードへ送っている部屋のトラックの数 */
  relay_downstreams: number;
  /** ほかのノードから受けている部屋のトラックの数 */
  relay_upstreams: number;
  /** このノードにある部屋のインスタンス（カスケードしている部屋） */
  rooms: Array<{
    /** `edge` のとき、home のノード（不明なら null） */
    home: string | null;
    /** ノードごとの参加者の数 */
    peers: { [key: string]: number };
    /** `home` は部屋の配置先のノード、`edge` は参加者がつないだ別のノード */
    role: "home" | "edge";
    room: string;
  }>;
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
  /** SENCA が PaaS に作成した資源（後片付けに使います） */
  provider_resource: string | null;
  /** このノードでの実行の状態。`enabled: false` の場合、実行がまだ始まっていない場合、またはこのノードに割り当てられていない場合は null です（クラスターでは、割り当てたノード `placement_status.node` に問い合わせてください）。作成・PATCH の応答では、実行の作り直しが非同期で行われるため、以前の実行の状態（または null）のことがあります。 */
  status: IoStatus | null;
  stream: string;
  url: string | null;
};

export type OutputCreate = {
  credential_ref?: CredentialRef;
  enabled?: boolean | null;
  options?: IoOptions;
  /** 配置の指定（`degrade` は使えません）。指定が無ければ、配信を受信したノードを優先します */
  placement?: PlacementSpec | null;
  protocol: "rtmp_push" | "srt_push" | "paas";
  /** `paas` で必須（`credential_ref` も必須） */
  provider?: "cloudflare_stream" | "aws_ivs" | "aws_medialive" | null;
  /** `paas` 以外で必須。rtmp_push: `rtmp`・`rtmps`、srt_push: `srt`。資格情報は含められません。 */
  url?: string | null;
};

export type OutputList = {
  items: Output[];
  next_cursor: string | null;
};

export type Participant = {
  bundle_id: string;
  /** 無ければ省きます */
  client_id?: string;
  connection_id: string;
  joined_at?: Timestamp;
  metadata: RoomMetadata;
  /** 他のノードにいる参加者（カスケード）の場合、そのノード */
  node?: string;
  /** WebRTC（ICE・DTLS）が接続済みかどうか（他のノードの参加者は true） */
  peer_connected: boolean;
  role: "sendrecv" | "sendonly" | "recvonly";
  /** シグナリングの経路（remote は他のノードの参加者） */
  signaling: "websocket" | "datachannel" | "remote";
  /** 受信の枠（他のノードの参加者は空） */
  subscriptions: RoomSlot[];
  /** 送信しているトラック */
  tracks: RoomTrack[];
};

export type ParticipantList = {
  items: Participant[];
  /** 続きを取得するための `cursor`（続きがあるときだけ値が入り、無ければ null） */
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
  /** 直近の失敗（例: `codec worker が動いていません`）。実行が始まると null に戻ります */
  last_error: string | null;
  /** 実行していなければ null です。実行中は `audio_mix` の処理ノードの id → 直近のメーターです（ミキサーが無い場合や、まだ通知が無い場合は `{}`）。 */
  meters: PipelineMeters | null;
  name: string;
  /**
   * このノードでの実行の状態。starting = セッションを開いている（`desired_state: running` で、まだ実行を始めていない場合も含む）、running = 実行中、
   * error = 失敗して作り直しを待っている（`last_error`、1〜32 秒のバックオフ）、stopped = 停止している。
   */
  observed_state: "starting" | "running" | "error" | "stopped";
  /** 配置の指定。指定が無ければ null */
  placement: PlacementSpecView | null;
  placement_status: PlacementStatus;
  /** 実行中の計画（worker が開いたセッションのもの）。実行中でなければ null です（検証時の計画は `POST /pipelines/{id}/validate` で取得します）。 */
  plan: PipelinePlan | null;
  /** 直近の状態の通知。まだ無ければ null です。 */
  status: PipelineStatus | null;
  updated_at: Timestamp;
};

/**
 * 操作です（`op` で種類を指定します。未知の項目は 400）。`node` は対象の処理ノードの id（`compose` または `audio_mix`）です。
 * `prepare`・`commit`・`rollback` は形式としては受け付けますが、この API では 400 です（グラフの変更には `PATCH /pipelines/{id}` を使います）。
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
  /** 操作への応答（現在の操作ではすべて空のオブジェクト） */
  reply: Record<string, unknown>;
};

export type PipelineCreate = {
  desired_state?: "running" | "stopped" | null;
  graph: PipelineGraph;
  name: StreamName;
  /** 配置の指定と縮退の段階 */
  placement?: PlacementSpec | null;
};

/**
 * パイプラインの定義です。有向非巡回グラフで、未知の項目は拒否します。サイズは 16 MiB までです（ただし、HTTP 層の本文の上限 1 MiB が先に適用されます）。
 * 処理ノード（`nodes` の各要素）は `type` で種類を指定します。入力は `"処理ノード"` または `"処理ノード.ポート"` の形式で参照します（`source` のポートは `video`・`audio` で、省略できません。`compose` は `program`（既定）・`preview`、それ以外は `out`）。
 * 処理ノードごとの設定の詳細はガイドの「7. パイプライン」（`docs/guide/ja/07-pipelines.md`）を参照してください。ここでは種類と必須の項目だけを記載します。値の範囲は検証（`POST /pipelines/{id}/validate`）で確認できます。
 */
export type PipelineGraph = {
  nodes: PipelineNode[];
  options?: PipelineOptions;
  version: 1;
};

/** 検証で見つかった問題（400・503 の `details.issues` の要素）。 */
export type PipelineIssue = {
  code: "MALFORMED" | "INVALID_PARAM" | "DUPLICATE_ID" | "UNKNOWN_REFERENCE" | "TYPE_MISMATCH" | "CYCLE" | "UNCONSUMED_OUTPUT" | "POLICY_REJECTED" | "UNSUPPORTED_CODEC" | "CAPACITY_UNAVAILABLE" | "LIMIT_EXCEEDED";
  message: string;
  /** 問題のある処理ノードの id */
  node?: string;
};

/** シーンのレイヤー（source・image・color のいずれか、x・y・width・height、crop・fit・z・opacity・corner_radius・border・visible・filter） */
export type PipelineLayer = Record<string, unknown>;

export type PipelineList = {
  items: Pipeline[];
  next_cursor: string | null;
};

/** `audio_mix` の処理ノードの id → 直近のメーター（`inputs`・`master` の peak/rms dBFS、`limiter_reduction_db`）。 */
export type PipelineMeters = { [key: string]: {
  event: "meters";
} };

/** 処理ノードです。`type` ごとの必須の項目だけを記載します（他の項目はガイドの「7. パイプライン」を参照）。`id` は英数字・`_`・`-` からなる 1〜64 文字で、グラフ内で一意です。 */
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
  /** av1・vp9・vp8・opus・h264・h265 など（使えるものはコーデック worker の能力と方針で決まります） */
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
  /** 出力先の stream の ID（テナントのもの。パイプラインが配信者になります） */
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
  /** 状態・メーターを通知する間隔（0 で通知しない） */
  status_interval_ms?: number;
};

/** 指定したフィールドだけを置き換えます（null は「変更しない」の意味です。ただし `placement: null` は配置の指定を削除します）。 */
export type PipelinePatch = {
  desired_state?: "running" | "stopped" | null;
  graph?: PipelineGraph | null;
  name?: StreamName | null;
  /** 配置の指定の全体を置き換えます（一部だけの変更はできません）。null で削除します。指定しなければ、現在の指定を新しい `graph` で検査し直します（縮退の段階の `disable` が新しいグラフに無ければ 400） */
  placement?: PlacementSpec | null;
};

/** 実行の計画（処理の構成・必要な資源・追加の遅延）。 */
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
  /** 実行の順序（トポロジカル順） */
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
 * パイプラインからの直近の通知です（`event` で種類を表します）。
 * `event: status`（`status_interval_ms` ごと）には、`sources`（入力ごとの state `waiting`・`live`・`stalled`・`ended`、AU 数、タリー `on_program`/`on_preview`）、
 * `compose`（シーン、トランジション中かどうか、スレートを表示している入力）、`encoders`（フレーム数・エラー）が含まれます。
 * メーター（`event: meters`）は `meters` に入り、ここにはそれ以外の直近の通知が入ります（世代やエラーの通知のこともあります）。
 */
export type PipelineStatus = {
  compose: Array<Record<string, unknown>>;
  encoders: Array<Record<string, unknown>>;
  event: "status";
  /** 実行中のグラフの世代（worker 内部の番号で、パイプラインの generation とは別のもの） */
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

/** 優先度（low < normal < high < critical）。資源が足りないときは、優先度の低いワークロードから縮退・退去させます。 */
export type PlacementPriority = "low" | "normal" | "high" | "critical";

/**
 * 配置の指定です（ADR-0013）。省略または null の場合はスケジューラーに任せます（優先度 normal、spread）。検査に失敗すると 400 です（`INVALID_CONFIG`。未知のフィールドや型の誤りは `INVALID_REQUEST`）。失敗する条件:
 * `node` が存在しない、文字列が空・128 文字を超える・制御文字を含む、`labels` が 17 個以上、`values` が空または 33 個以上、入力・出力に `degrade` を指定した、
 * `degrade` が 9 段階以上、段階の名前が `full` または重複している、`disable` が空、グラフに無い処理ノードを指定した。
 */
export type PlacementSpec = {
  /** グループの名前。同じグループのワークロードは同じノードに配置しません */
  anti_affinity?: string | null;
  /** 縮退の段階（**パイプライン専用**。品質の高い順）。資源が足りないときは段階を下げて（処理ノードを停止して）実行を続けます。資源が戻れば元の段階へ戻します */
  degrade?: DegradeLevel[];
  /** 手動の割り当て。装置の ID（GPU・キャプチャ機器、`GET /grid`）。`node` が無ければ、その装置を持つノードのいずれかに配置します */
  device?: string | null;
  /** すべての条件に合うノードにだけ配置します */
  labels?: LabelSelector[];
  /** 手動の割り当て。ノードの ID または名前（`GET /nodes`）。保存時に ID に変換します（応答では ID） */
  node?: string | null;
  /** 手動の割り当て先が停止したときの動作。prefer = 他のノードへ移し、割り当て先が戻ったら（クールダウンの後に）戻す、strict = 停止したままにする（`placement_status.state: unschedulable`） */
  pin_policy?: "strict" | "prefer";
  priority?: PlacementPriority;
  /** spread = 余裕の多いノードへ分散する、binpack = 詰めて配置する */
  strategy?: "spread" | "binpack";
};

/** 保存された配置の指定（`node` は ID に変換したもの）。値の無い項目と空の配列は省きます。`pin_policy`・`priority`・`strategy` は既定値でも含まれます。 */
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
 * 配置の結果（グリッドでの割り当て）です。`state` によって形が変わります。
 * - `not_scheduled`: ワークロードになっていない（入力・出力が `enabled: false`、パイプラインが `desired_state: stopped`）。
 * - `running`・`degraded`: 割り当て済み（`degraded` は縮退の段階 `level` が 1 以上）。割り当てたノードだけが実行します（実際の実行状態は `status`・`observed_state` で確認します）。
 * - `pending`: 配置先を探している（作成・変更の直後。スケジューラーは約 1 秒ごとと、資源が変わるたびに計画します）。
 * - `unschedulable`: 配置できるノードが無い（`reasons` に理由ごとのノード）。
 */
export type PlacementStatus = {
  state: "not_scheduled";
} | {
  /** 使用する GPU の ID */
  device: string | null;
  /** 現在の縮退の段階（0 = full、i は `placement.degrade[i-1]`） */
  level: number;
  /** 段階の名前（段階 0 は `full`） */
  level_label: string;
  /** 割り当てたノードの ID */
  node: string;
  /** そのノードの名前 */
  node_name: string | null;
  reason: GridReason;
  /** このノードに配置した時刻 */
  since: Timestamp;
  state: "running" | "degraded";
} | {
  /** 配置できない理由（`unschedulable` のとき。`pending` では null） */
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

/** RFC 9457 の problem details です。内部の詳細や秘密は含めません。 */
export type Problem = {
  code: ProblemCode;
  /** code ごとの補足情報（例: 本文の解析エラーは `{reason}`、予約の衝突は `{limit, overlaps}`） */
  details?: unknown;
  /** `X-Request-Id` と同じ値（Idempotency-Key による再送では、再送した要求の ID）。 */
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
  /** 予約で作成した録画であれば予約の ID */
  schedule: string | null;
  segments: number | null;
  started_at: Timestamp;
  /** 削除中（deleting）の録画は API から見えません（404）。 */
  state: "recording" | "finalized" | "interrupted";
  /** 保存先（S3 互換のオブジェクトストレージ）に上げ終わっていれば、その場所と時刻。null なら録画したノードの手元だけです。 保存先にある録画は、同じ保存先を設定したどのノードからでも再生・書き出し・サムネイルを取得できます。 */
  storage: null | {
    archived_at: Timestamp;
    /** `s3://bucket/prefix` */
    uri: string;
  };
  /** 元の stream の ID（stream を削除しても残ります） */
  stream: string;
  /** `GET /recordings/{id}/thumbnail` があるかどうか */
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
  /** 解除には recording:delete が必要です */
  legal_hold?: boolean;
  name?: string;
  /** RFC 3339。null は無期限。 */
  retention_until?: string | null;
};

export type RecordingPolicy = {
  /** publish が始まったら自動で録画します */
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
  /** IANA のタイムゾーン名（例: Asia/Tokyo）。現地時刻で繰り返します */
  timezone: string;
  /** 繰り返しの終了（この時刻を含まない、UTC ミリ秒） */
  until_ms?: number | null;
  /** weekly の曜日（0 = 日曜） */
  weekdays?: number[];
};

export type RejectCount = {
  /** 該当するノードの ID（8 個まで） */
  examples: string[];
  /** この理由で受け入れられないノードの数 */
  nodes: number;
  /** ノードがワークロードを受け入れられない理由（`contended` は、単独なら収まるが、優先度が同じか高いワークロードで埋まっている状態） */
  reason: "node_down" | "not_media" | "draining" | "pinned_elsewhere" | "pin_target_down" | "labels" | "anti_affinity" | "no_device" | "capture" | "cpu" | "memory" | "net_in" | "net_out" | "gpu_encode" | "gpu_decode" | "gpu_memory" | "contended";
};

export type RelayStatus = {
  /** 中継で受信しているミラーの stream の ID */
  mirrors: string[];
  /** 中継の接続（相手のノードごと） */
  peers: Array<{
    /** この接続で送信している stream の数 */
    downstreams: number;
    /** どちらかが接続を閉じようとしているかどうか */
    going_away: boolean;
    /** 相手のノードのクラスター内の番号（クラスター設定の `peers[].id`。API のノード ID とは別のもの） */
    node: number;
    /** 送信した AU 1 つあたりのヘッダーのバイト数（フレームのヘッダー・制御フレームを含む） */
    overhead_per_au: number;
    /** RTT（平滑化した値、ミリ秒）。まだ計測していなければ null */
    rtt_ms: number | null;
    /** 受信した AU の数 */
    rx_aus: number;
    /** 直近の受信速度（bit/s） */
    rx_bps: number;
    rx_bytes: number;
    /** 送信した AU の数 */
    tx_aus: number;
    /** 直近の送信速度（bit/s） */
    tx_bps: number;
    tx_bytes: number;
    /** この接続で受信している stream の数 */
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
  /** 参加トークンの発行で自動作成された部屋かどうか（誰もいない状態で `empty_timeout_secs` が経過すると削除されます） */
  auto: boolean;
  created_at: Timestamp;
  generation: number;
  id: RoomId;
  metadata: RoomMetadata;
  /** 配置の指定。指定が無ければ null */
  placement: PlacementSpecView | null;
  placement_status: PlacementStatus;
  settings: RoomSettings;
  /** 接続先（`GET /signaling` の WebSocket）。部屋を別のノードに配置した場合はそのノードの URL です。このノードに `--public-http-url` が設定されておらず、別のノードにも配置していなければ null */
  signaling_url: string | null;
  updated_at: Timestamp;
};

export type RoomCreate = {
  /** 省略すると生成します（24 文字の 16 進数） */
  id?: RoomId;
  metadata?: RoomMetadata;
  /** 配置の指定（`degrade` は使えません） */
  placement?: PlacementSpec | null;
  /** 部屋の設定（省略または null の場合は既定値） */
  settings?: RoomSettings | null;
};

/** 部屋の ID（1〜128 文字の英数字と `-_.:@`。テナントをまたいで一意） */
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
  /** 送信する内容（JSON で 64 KiB まで）。`label` がある場合は文字列 */
  data: unknown;
  /** DataChannel のラベル（`#` で始まります）。無ければシグナリングの `push` で送ります */
  label?: string | null;
  /** 送信先の connection_id（空なら全員） */
  to?: string[];
};

export type RoomMessageResult = {
  /** 送信した相手の数 */
  delivered: number;
};

/** 部屋・参加者の metadata（任意の JSON。JSON にして 8 KiB まで）。参加者の metadata は他の参加者にも見えます */
export type RoomMetadata = unknown;

export type RoomParticipantStats = {
  /** 受信者への推定帯域 */
  bandwidth_bps: number;
  /** クライアントが送信した getStats の要約（無ければ省きます） */
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
  /** この参加者へ送信している量 */
  send_bitrate_bps: number;
  subscriptions: Array<{
    mid: string;
    /** 現在配信している層（層の無いトラックや、まだ配信していない場合は null） */
    rid: string | null;
    state: "empty" | "active" | "paused";
    track_id: string | null;
  }>;
};

/** 指定したフィールドだけを変更します。`settings` は JSON Merge Patch です（項目の値の null で既定値に戻ります）。`settings`・`metadata` の null は「変更しない」の意味で、`placement: null` は配置の指定を削除します。 */
export type RoomPatch = {
  metadata?: RoomMetadata;
  /** 配置の指定の全体を置き換えます（`degrade` は使えません）。null で削除します */
  placement?: PlacementSpec | null;
  /** 変更する設定の項目だけを書きます（JSON Merge Patch。書かない項目はそのまま残り、項目の値の null はその項目を既定値に戻します） */
  settings?: RoomSettings | null;
};

/** 参加者に許可する操作（省略するとすべて許可）。 */
export type RoomPermissions = {
  /** 使用を許可する DataChannel のラベルのパターン（`#*` ですべて） */
  data?: string[];
  /** 送信を許可するトラックの種類 */
  publish?: Array<"audio" | "video">;
  /** 他の参加者のトラックを受信してよいか */
  subscribe?: boolean;
};

/**
 * 部屋の設定です。応答では既定値を補った全体を返します（`max_video_bitrate_kbps`・`spotlight`・`forwarding_filter` は値が無ければ省きます）。
 * API で検査するのは形式と `max_participants` だけです（それ以外の範囲の制約は、部屋を開くときに検査します。`POST /rooms` の説明を参照）。
 */
export type RoomSettings = {
  audio_codec?: "opus";
  /** 使用を許可する DataChannel のラベルのパターン（`#` で始まります。末尾の `*` は前方一致） */
  data_labels?: string[];
  /** E2EE（SFrame）の部屋。SFU は中身を見ずに転送するため、合成や合成の録画はできません */
  e2ee?: boolean;
  /** 誰もいなくなってから削除するまでの秒数（参加トークンの発行で自動作成された部屋だけが対象） */
  empty_timeout_secs?: number;
  /** 部屋全体の転送フィルター（購読者ごとのフィルターと、両方の条件を満たすものだけを配信します） */
  forwarding_filter?: ForwardingFilter | null;
  /** 参加者の上限（他のノードにいる参加者も数えます） */
  max_participants?: number;
  /** 配信者に推奨する映像のビットレートの上限（kbit/s） */
  max_video_bitrate_kbps?: number | null;
  /** 録画の方針。`per_participant`・`composite` にすると、部屋の録画のメディアパイプライン（`managed: true`）を作ります。`composite` は `e2ee` と併用できません（422） */
  recording?: "none" | "per_participant" | "composite";
  spotlight?: RoomSpotlight | null;
  /** 部屋の映像のコーデック（SFU は変換しないため、全員が同じコーデックを使います） */
  video_codec?: "vp8" | "vp9" | "h264" | "av1";
};

/** 受信の枠（クライアントが受信する m 行 1 つ）と、そこに現在流しているトラックです。 */
export type RoomSlot = {
  client_id?: string;
  connection_id?: string;
  kind: "audio" | "video";
  mid: string;
  /** active = 配信中、paused = 割り当て済みだが配信していない、empty = 空き */
  state: "empty" | "active" | "paused";
  track_id?: string;
};

/** スポットライト（話している人に注目し、それ以外の人の映像の品質を下げます）。 */
export type RoomSpotlight = {
  /** 注目した配信者の層（無ければ帯域に応じて自動で選びます） */
  focus_rid?: string;
  /** 注目する話者の数（受信者から見て、自分を除いたこの人数） */
  number: number;
  /** lowest = 注目していない人は最も低い層で配信する、off = 配信しない（受信の枠は `paused`） */
  unfocused?: "lowest" | false;
};

export type RoomStats = {
  at: Timestamp;
  /** 部屋を配置したノードに接続した参加者 */
  participants: RoomParticipantStats[];
  room_id: RoomId;
};

export type RoomToken = {
  expires_at: Timestamp;
  room_id: RoomId;
  /** `Room.signaling_url` と同じ */
  signaling_url: string | null;
  /** 参加トークン（JWT）。Idempotency-Key による再送では null */
  token: string | null;
  /** Idempotency-Key による再送で値を伏せたとき */
  token_redacted?: true;
};

export type RoomTokenRequest = {
  /** 同じ値を持つ接続どうしは、互いのトラックを受信しません */
  bundle_id?: string | null;
  /** 指定すると、`connect.client_id` はこれと同じ値でなければなりません（転送フィルターの条件に使われます） */
  client_id?: string | null;
  /** 参加者の metadata（他の参加者にも見えます） */
  metadata?: RoomMetadata;
  permissions?: RoomPermissions | null;
  /** 許可する役割の上限（`sendrecv` のトークンではどの役割でも参加できます） */
  role: "sendrecv" | "sendonly" | "recvonly";
  /** 有効期間（秒） */
  ttl_secs?: number | null;
};

export type RoomTrack = {
  /** 配信者の接続 */
  connection_id: string;
  kind: "audio" | "video";
  /** サイマルキャストの層（低い順）。サイマルキャストでなければ省きます */
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

/** record = 期間中に録画する、live = 期間中だけ publish を受け付ける、snapshot = 期間中に一定間隔で静止画を撮る */
export type ScheduleAction = "record" | "live" | "snapshot";

export type ScheduleCheck = {
  ok: true;
  /** 同じ stream で時間が重なる録画予約（pre/post roll を含む） */
  overlaps: Array<{
    end_at: Timestamp;
    id: string;
    start_at: Timestamp;
  }>;
  /** 重なりがあり、入力を共有するかどうか */
  shared_input: boolean;
};

export type ScheduleCreate = {
  action: ScheduleAction;
  /** RFC 3339。start_at より後で未来の時刻。期間は 24 時間まで */
  end_at: string;
  /** 既定 60000。実行時刻を過ぎても、この時間内であれば途中から実行します */
  grace_ms?: number | null;
  name?: string | null;
  /** 既定 0 */
  post_roll_ms?: number | null;
  /** 既定 0 */
  pre_roll_ms?: number | null;
  recurrence?: Recurrence | null;
  retention_days?: number | null;
  /** action が snapshot のときだけ指定します（その場合は必須） */
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

/**
 * `publish` は送信の権限です。`POST /whip/{stream}` の `Authorization: Bearer` に、publish 鍵の代わりに `publish` を持つ同じテナントの API トークン・JWT を使えます。
 * `subscribe` は `POST /streams/{id}/playback-url` に使います。
 */
export type Scope = "stream:read" | "stream:write" | "publish" | "subscribe" | "recording:read" | "recording:delete" | "tenant:admin" | "node:admin";

export type Secret = {
  created_at: Timestamp;
  id: string;
  name: string;
  /** `credential_ref` に指定する値 */
  ref: string;
};

export type SecretCreate = {
  name: StreamName;
  /** 1〜8192 バイト（UTF-8）。応答には含まれません */
  value: string;
};

export type SecretList = {
  items: Secret[];
  /** 常に null（ページングはありません） */
  next_cursor: null;
};

export type SnapshotList = {
  items: Array<{
    /** JPEG のサイズ */
    bytes: number;
    /** 前回の保存からの変化の割合（パイプラインの静止画のみ） */
    changed_fraction?: number;
    height: number;
    id: string;
    /** `snapshot_on_change` の処理ノードの ID（パイプラインの静止画のみ） */
    node?: string;
    /** 撮影した理由 */
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
  /** 種類ごとの内容（秘密は含みません）。部屋のイベントは `{type, data}` */
  data: Record<string, unknown>;
  /** 部屋のイベントの ID（部屋のイベントだけ。SSE の `id` はありません） */
  event_id?: string;
  /** イベントを発生させたノードの ID（部屋のイベントだけ） */
  node?: string;
  occurred_at: Timestamp;
  /** 資源の ID（部屋のイベントは部屋の ID、`grid.decisions` は空） */
  resource_id: string;
  type: "stream.created" | "stream.updated" | "stream.deleted" | "stream.owner" | "recording.updated" | "recording.deleted" | "schedule.created" | "schedule.updated" | "schedule.deleted" | "schedule.run" | "node.updated" | "input.updated" | "input.deleted" | "output.updated" | "output.deleted" | "secret.created" | "secret.deleted" | "pipeline.updated" | "pipeline.deleted" | "room.updated" | "room.deleted" | "media_pipeline.updated" | "media_pipeline.deleted" | "node.down" | "node.up" | "device.down" | "device.up" | "device.disabled" | "device.enabled" | "grid.decisions" | "room.started" | "room.stopped" | "room.connection.created" | "room.connection.updated" | "room.connection.destroyed" | "room.track.published" | "room.track.unpublished" | "room.spotlight.changed";
};

export type Stream = {
  /** 字幕と翻訳の設定（無ければ null）。資格情報は `secret:{name}` で参照し、値は返しません。 */
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
  /** 観測した配信の状態です。 */
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

/** 指定したフィールドだけを置き換えます。`snapshots: null` は静止画の方針を、`captions: null` は字幕の設定を削除します。他のフィールドの null は「変更しない」の意味です。 */
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
  /** トークンの値。発行時の応答でだけ値が入ります（一覧では null）。 */
  token: string | null;
  /** Idempotency-Key による再送で値を伏せたとき */
  token_redacted?: true;
};

export type TokenCreate = {
  /** RFC 3339（未来の時刻） */
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
    /** 現在は常に null */
    mp4: null;
  };
};

export {};
