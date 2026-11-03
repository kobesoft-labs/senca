/**
 * create・list・get・update・delete を持つ資源。
 * @template T, C, P
 */
export class Collection<T, C, P> {
    /**
     * @internal
     * @param {Http} http
     * @param {string} path 一覧のパス（`/streams`）
     * @param {string} [itemPath] 1 つのパスの前半（既定は path と同じ）
     */
    constructor(http: Http, path: string, itemPath?: string);
    /** @protected */
    protected http: Http;
    /** @protected */
    protected path: string;
    /** @protected */
    protected itemPath: string;
    /**
     * 作る（POST、Idempotency-Key を自動で付ける）。
     * @param {C} body
     * @param {CreateOptions} [o]
     * @returns {Promise<T>}
     */
    create(body: C, o?: CreateOptions): Promise<T>;
    /**
     * 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。
     * @param {ListOptions} [o]
     * @returns {Paginator<T>}
     */
    list(o?: ListOptions): Paginator<T>;
    /**
     * 一覧の 1 頁。
     * @param {ListOptions & {query?: Record<string, string | undefined>}} [o]
     * @returns {Promise<{items: T[], next_cursor: string | null}>}
     */
    listPage(o?: ListOptions & {
        query?: Record<string, string | undefined>;
    }): Promise<{
        items: T[];
        next_cursor: string | null;
    }>;
    /**
     * 取得（ETag は `SencaClient.etagOf(x)` で読める）。
     * @param {string} id
     * @param {CallOptions} [o]
     * @returns {Promise<T>}
     */
    get(id: string, o?: CallOptions): Promise<T>;
    /**
     * 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。
     * @param {string} id
     * @param {P} patch
     * @param {CallOptions & {ifMatch: IfMatch}} o
     * @returns {Promise<T>}
     */
    update(id: string, patch: P, o: CallOptions & {
        ifMatch: IfMatch;
    }): Promise<T>;
    /**
     * 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。
     * @param {string} id
     * @param {(current: T) => P | Promise<P>} change
     * @param {CallOptions & {attempts?: number}} [o]
     * @returns {Promise<T>}
     */
    modify(id: string, change: (current: T) => P | Promise<P>, o?: CallOptions & {
        attempts?: number;
    }): Promise<T>;
    /**
     * 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。
     * @param {string} id
     * @param {CallOptions & {ifMatch?: IfMatch}} [o]
     * @returns {Promise<void>}
     */
    delete(id: string, o?: CallOptions & {
        ifMatch?: IfMatch;
    }): Promise<void>;
}
/** @extends {Collection<Stream, StreamCreate, StreamPatch>} */
export class Streams extends Collection<import("./openapi.gen.js").Stream, import("./openapi.gen.js").StreamCreate, import("./openapi.gen.js").StreamPatch> {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** stream の静止画。 */
    snapshots: {
        /** @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').SnapshotList['items']>} */
        list: (id: string, o?: CallOptions) => Promise<import("./openapi.gen.js").SnapshotList["items"]>;
        /** @param {string} id @param {string} sid @param {CallOptions} [o] @returns {Promise<Blob>} */
        get: (id: string, sid: string, o?: CallOptions) => Promise<Blob>;
        /** @param {string} id @param {string} sid @param {CallOptions} [o] */
        delete: (id: string, sid: string, o?: CallOptions) => Promise<void>;
    };
    /** stream key を作り直す（新しい key はこの応答でだけ見える）。 @param {string} id @param {CreateOptions} [o] @returns {Promise<Stream>} */
    rotateKey(id: string, o?: CreateOptions): Promise<Stream>;
    /** 署名付きの再生 URL。 @param {string} id @param {import('./openapi.gen.js').PlaybackRequest} [body] @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').LivePlayback>} */
    playbackUrl(id: string, body?: import("./openapi.gen.js").PlaybackRequest, o?: CreateOptions): Promise<import("./openapi.gen.js").LivePlayback>;
    /** 録画を始める。 @param {string} id @param {import('./openapi.gen.js').RecordingStart} [body] @param {CreateOptions} [o] @returns {Promise<Recording>} */
    startRecording(id: string, body?: import("./openapi.gen.js").RecordingStart, o?: CreateOptions): Promise<Recording>;
    /** 計画（入力・出力・変換の見積もりと不適合の理由）。 @param {string} id @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').Plan>} */
    plan(id: string, o?: CreateOptions): Promise<import("./openapi.gen.js").Plan>;
    /** 今の静止画。 @param {string} id @param {CallOptions & {format?: 'jpeg' | 'png'}} [o] @returns {Promise<Blob>} */
    snapshot(id: string, o?: CallOptions & {
        format?: "jpeg" | "png";
    }): Promise<Blob>;
}
/** @extends {Collection<Recording, never, RecordingPatch>} */
export class Recordings extends Collection<import("./openapi.gen.js").Recording, never, import("./openapi.gen.js").RecordingPatch> {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /**
     * 録画は stream から始める（`streams.startRecording`）。
     * @param {never} _body
     * @returns {Promise<never>}
     */
    create(_body: never): Promise<never>;
    /**
     * 一覧（`stream` で絞れる）。
     * @param {ListOptions & {stream?: string}} [o]
     * @returns {Paginator<Recording>}
     */
    list(o?: ListOptions & {
        stream?: string;
    }): Paginator<Recording>;
    /** 止める。 @param {string} id @param {CreateOptions} [o] @returns {Promise<Recording>} */
    stop(id: string, o?: CreateOptions): Promise<Recording>;
    /** 署名付きの VOD の URL。 @param {string} id @param {import('./openapi.gen.js').PlaybackRequest} [body] @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').VodPlayback>} */
    playbackUrl(id: string, body?: import("./openapi.gen.js").PlaybackRequest, o?: CreateOptions): Promise<import("./openapi.gen.js").VodPlayback>;
    /** サムネイル（JPEG）。 @param {string} id @param {CallOptions} [o] @returns {Promise<Blob>} */
    thumbnail(id: string, o?: CallOptions): Promise<Blob>;
    /** MP4 の書き出し（大きいので Response のまま返す。`res.body` を流して読む）。 @param {string} id @param {CallOptions} [o] @returns {Promise<Response>} */
    exportMp4(id: string, o?: CallOptions): Promise<Response>;
}
/** @extends {Collection<Schedule, ScheduleCreate, SchedulePatch>} */
export class Schedules extends Collection<import("./openapi.gen.js").Schedule, import("./openapi.gen.js").ScheduleCreate, import("./openapi.gen.js").SchedulePatch> {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** @param {ListOptions & {stream?: string}} [o] @returns {Paginator<Schedule>} */
    list(o?: ListOptions & {
        stream?: string;
    }): Paginator<Schedule>;
    /** 作らずに確かめる（重なり・上限）。 @param {ScheduleCreate} body @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').ScheduleCheck>} */
    check(body: ScheduleCreate, o?: CallOptions): Promise<import("./openapi.gen.js").ScheduleCheck>;
    /** 実行の履歴。 @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').ScheduleRun[]>} */
    runs(id: string, o?: CallOptions): Promise<import("./openapi.gen.js").ScheduleRun[]>;
}
/** API トークン（更新は無い）。 */
export class Tokens {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** @private @type {Collection<Token, TokenCreate, never>} */
    private c;
    /** 作る（`token` の値はこの応答でだけ見える）。 @param {TokenCreate} body @param {CreateOptions} [o] */
    create(body: TokenCreate, o?: CreateOptions): Promise<import("./openapi.gen.js").Token>;
    /** @param {ListOptions} [o] */
    list(o?: ListOptions): Paginator<import("./openapi.gen.js").Token>;
    /** @param {ListOptions} [o] */
    listPage(o?: ListOptions): Promise<{
        items: import("./openapi.gen.js").Token[];
        next_cursor: string | null;
    }>;
    /** @param {string} id @param {CallOptions} [o] */
    get(id: string, o?: CallOptions): Promise<import("./openapi.gen.js").Token>;
    /** 失効させる。 @param {string} id @param {CallOptions} [o] */
    delete(id: string, o?: CallOptions): Promise<void>;
}
/**
 * stream の下の入力・出力（作成・一覧は stream ごと、他は ID で）。
 * @template T, C
 * @extends {Collection<T, C, IoPatch>}
 */
export class StreamIo<T, C> extends Collection<T, C, import("./openapi.gen.js").IoPatch> {
    /** @internal @param {Http} http @param {'inputs' | 'outputs'} kind */
    constructor(http: Http, kind: "inputs" | "outputs");
    /** @private */
    private kind;
    /**
     * stream に作る。
     * @param {string} streamId
     * @param {C} body
     * @param {CreateOptions} [o]
     * @returns {Promise<T>}
     */
    create(streamId: string, body: C, o?: CreateOptions): Promise<T>;
    /**
     * stream の一覧。
     * @param {string} streamId
     * @param {ListOptions} [o]
     * @returns {Paginator<T>}
     */
    list(streamId: string, o?: ListOptions): Paginator<T>;
}
/** 秘密値（値は読めない。作成・一覧・削除だけ）。 */
export class Secrets {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** @private */
    private http;
    /** @param {SecretCreate} body @param {CreateOptions} [o] @returns {Promise<Secret>} */
    create(body: SecretCreate, o?: CreateOptions): Promise<Secret>;
    /** 全部（頁は無い）。 @param {CallOptions} [o] @returns {Promise<Secret[]>} */
    list(o?: CallOptions): Promise<Secret[]>;
    /** @param {string} id @param {CallOptions} [o] */
    delete(id: string, o?: CallOptions): Promise<void>;
}
/** @extends {Collection<Pipeline, PipelineCreate, PipelinePatch>} */
export class Pipelines extends Collection<import("./openapi.gen.js").Pipeline, import("./openapi.gen.js").PipelineCreate, import("./openapi.gen.js").PipelinePatch> {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** `snapshot_on_change`（板書の変化）で保存した静止画。 */
    snapshots: {
        /** @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').SnapshotList['items']>} */
        list: (id: string, o?: CallOptions) => Promise<import("./openapi.gen.js").SnapshotList["items"]>;
        /** @param {string} id @param {string} sid @param {CallOptions} [o] @returns {Promise<Blob>} */
        get: (id: string, sid: string, o?: CallOptions) => Promise<Blob>;
        /** @param {string} id @param {string} sid @param {CallOptions} [o] */
        delete: (id: string, sid: string, o?: CallOptions) => Promise<void>;
    };
    /** グラフを検証する（保存前のグラフも）。 @param {string} id @param {import('./openapi.gen.js').PipelineGraph} graph @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').PipelineValidation>} */
    validate(id: string, graph: import("./openapi.gen.js").PipelineGraph, o?: CallOptions): Promise<import("./openapi.gen.js").PipelineValidation>;
    /** 動いているパイプラインを操作する（スイッチャーの take など）。 @param {string} id @param {import('./openapi.gen.js').PipelineControl} op @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').PipelineControlResult>} */
    control(id: string, op: import("./openapi.gen.js").PipelineControl, o?: CreateOptions): Promise<import("./openapi.gen.js").PipelineControlResult>;
}
/** ノード（運用、`node:admin`）。 */
export class Nodes {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** @private */
    private http;
    /** GPU・キャプチャ機器の切り離し・戻し。 */
    devices: {
        /** @param {string} nodeId @param {string} device @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').DeviceHealth>} */
        disable: (nodeId: string, device: string, o?: CreateOptions) => Promise<import("./openapi.gen.js").DeviceHealth>;
        /** @param {string} nodeId @param {string} device @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').DeviceHealth>} */
        enable: (nodeId: string, device: string, o?: CreateOptions) => Promise<import("./openapi.gen.js").DeviceHealth>;
    };
    /** @param {CallOptions} [o] @returns {Promise<Node[]>} */
    list(o?: CallOptions): Promise<Node[]>;
    /** @param {string} id @param {CallOptions} [o] @returns {Promise<Node>} */
    get(id: string, o?: CallOptions): Promise<Node>;
    /** 新しい仕事を受けない（今の仕事は移す）。 @param {string} id @param {CreateOptions} [o] @returns {Promise<Node>} */
    drain(id: string, o?: CreateOptions): Promise<Node>;
    /** @param {string} id @param {CreateOptions} [o] @returns {Promise<Node>} */
    undrain(id: string, o?: CreateOptions): Promise<Node>;
}
/**
 * リアルタイムの部屋（ADR-0015）。部屋そのものはどのノードでも扱え、参加者・message・統計は部屋を置いたノードへ中継される。
 * ブラウザーは `createToken()` の `token` と `signaling_url` で `Senca.connect({url: signaling_url, room, token})`。
 * @extends {Collection<Room, RoomCreate, RoomPatch>}
 */
export class Rooms extends Collection<import("./openapi.gen.js").Room, import("./openapi.gen.js").RoomCreate, import("./openapi.gen.js").RoomPatch> {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /**
     * 部屋の参加者の操作。
     * @param {string} roomId
     */
    participants(roomId: string): {
        /** @param {ListOptions} [o] @returns {Paginator<RoomParticipant>} */
        list: (o?: ListOptions) => Paginator<RoomParticipant>;
        /** @param {string} connectionId @param {CallOptions} [o] @returns {Promise<RoomParticipant>} */
        get: (connectionId: string, o?: CallOptions) => Promise<RoomParticipant>;
        /** 他の参加者に見せる metadata を変える（参加者には `connection.updated` が届く）。 @param {string} connectionId @param {unknown} metadata @param {CallOptions} [o] @returns {Promise<RoomParticipant>} */
        updateMetadata: (connectionId: string, metadata: unknown, o?: CallOptions) => Promise<RoomParticipant>;
        /** 切断する（参加者には `disconnect {reason: "kicked"}`）。 @param {string} connectionId @param {CallOptions} [o] */
        disconnect: (connectionId: string, o?: CallOptions) => Promise<void>;
    };
    /** 部屋へ送る（`label` があれば DataChannel、無ければシグナリングの push）。 @param {string} roomId @param {import('./openapi.gen.js').RoomMessage} msg @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').RoomMessageResult>} */
    sendMessage(roomId: string, msg: import("./openapi.gen.js").RoomMessage, o?: CreateOptions): Promise<import("./openapi.gen.js").RoomMessageResult>;
    /** 統計（参加者ごとの RTT・帯域・送受の量・層・クライアントの getStats）。 @param {string} roomId @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').RoomStats>} */
    stats(roomId: string, o?: CallOptions): Promise<import("./openapi.gen.js").RoomStats>;
    /** 参加トークンを作る（`token` と `signaling_url` をブラウザーへ渡す）。 @param {string} roomId @param {import('./openapi.gen.js').RoomTokenRequest} body @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').RoomToken>} */
    createToken(roomId: string, body: import("./openapi.gen.js").RoomTokenRequest, o?: CreateOptions): Promise<import("./openapi.gen.js").RoomToken>;
}
/**
 * 部屋のメディアパイプライン（ADR-0016、AWS Chime SDK Media Pipelines 相当）。部屋の参加者を stream に書き出し、
 * 録画（`recording`）・合成（`composite`）・送り出し（`egress`）・文字起こし（`captions`）を 1 つの資源で指定する。
 * 子の資源（書き出した stream・合成のパイプライン・送り出しの出力）の ID は `artifacts` に出る。
 * @extends {Collection<MediaPipeline, MediaPipelineCreate, MediaPipelinePatch>}
 */
export class MediaPipelines extends Collection<import("./openapi.gen.js").MediaPipeline, import("./openapi.gen.js").MediaPipelineCreate, import("./openapi.gen.js").MediaPipelinePatch> {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /**
     * 一覧（`room` で部屋を絞れる）。
     * @param {ListOptions & {room?: string}} [o]
     * @returns {Paginator<MediaPipeline>}
     */
    list(o?: ListOptions & {
        room?: string;
    }): Paginator<MediaPipeline>;
    /** 動かす（`desired_state: running`）。 @param {string} id @param {CallOptions & {ifMatch?: IfMatch}} [o] @returns {Promise<MediaPipeline>} */
    start(id: string, o?: CallOptions & {
        ifMatch?: IfMatch;
    }): Promise<MediaPipeline>;
    /** 止める（`desired_state: stopped`。合成・送り出しを片付け、書き出した stream・録画は残る）。 @param {string} id @param {CallOptions & {ifMatch?: IfMatch}} [o] @returns {Promise<MediaPipeline>} */
    stop(id: string, o?: CallOptions & {
        ifMatch?: IfMatch;
    }): Promise<MediaPipeline>;
    /** @private @param {string} id @param {'running' | 'stopped'} state @param {CallOptions & {ifMatch?: IfMatch}} o @returns {Promise<MediaPipeline>} */
    private setState;
}
/** 監査ログ（読み出しだけ。新しい順。クラスターでは全ノードの分を合わせて返す）。 */
export class Audit {
    /** @internal @param {Http} http */
    constructor(http: Http);
    /** @private */
    private http;
    /**
     * 一覧（cursor を自動でたどる）。`action` は完全一致、`.` か `*` で終われば前方一致。`tenant` は別のテナント・`*` に node:admin が要る。
     * `for await (const e of client.audit.list({ action: 'stream.', since: new Date(Date.now() - 3600e3) }))`。
     * @param {ListOptions & AuditFilter} [o]
     * @returns {Paginator<AuditEntry>}
     */
    list(o?: ListOptions & AuditFilter): Paginator<AuditEntry>;
    /**
     * 一覧の 1 頁（`unavailable_nodes` は答えなかったノード。項目が `limit` より少なくても `next_cursor` があれば続きがある）。
     * @param {ListOptions & AuditFilter} [o]
     * @returns {Promise<AuditList>}
     */
    listPage(o?: ListOptions & AuditFilter): Promise<AuditList>;
}
/** 管理 API の client。 */
export class SencaClient {
    /**
     * 資源の ETag（GET・PATCH の `ETag`、無ければ `"{generation}"`）。
     * @param {object} resource
     */
    static etagOf(resource: object): string | undefined;
    /** 新しい Idempotency-Key（同じ作成を確実に 1 回にしたいとき、自分で持って再送に使う）。 */
    static newIdempotencyKey(): any;
    /** 管理 API の操作の表（operationId → method・path・権限。docs/api/openapi.yaml から生成）。 */
    static get operations(): Readonly<Record<string, {
        method: string;
        path: string;
        scope: string | null;
    }>>;
    /** @param {import('./http.js').ClientOptions} options */
    constructor(options: import("./http.js").ClientOptions);
    /** @private */
    private http;
    streams: Streams;
    recordings: Recordings;
    schedules: Schedules;
    tokens: Tokens;
    /** @type {StreamIo<Input, InputCreate>} */
    inputs: StreamIo<Input, InputCreate>;
    /** @type {StreamIo<Output, OutputCreate>} */
    outputs: StreamIo<Output, OutputCreate>;
    secrets: Secrets;
    pipelines: Pipelines;
    nodes: Nodes;
    /** リアルタイムの部屋。 */
    rooms: Rooms;
    /** 部屋のメディアパイプライン（書き出し・録画・合成・送り出し・文字起こし）。 */
    mediaPipelines: MediaPipelines;
    /** 監査ログ（読み出しだけ）。 */
    audit: Audit;
    /** API の base（`.../api/v1`）。 */
    get baseUrl(): string;
    /** 対応版・コーデック・機能。 @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').Capabilities>} */
    capabilities(o?: CallOptions): Promise<import("./openapi.gen.js").Capabilities>;
    /** 全ノードのハードウェア。 @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').Devices>} */
    devices(o?: CallOptions): Promise<import("./openapi.gen.js").Devices>;
    /** グリッド（ノード・資源・仕事の配置）。 @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').Grid>} */
    grid(o?: CallOptions): Promise<import("./openapi.gen.js").Grid>;
    /**
     * SSE のイベント。切れたら付け直す。`for await (const ev of client.events({types: ['stream.*']}))`。
     * @param {import('./sse.js').EventsOptions} [o]
     */
    events(o?: import("./sse.js").EventsOptions): AsyncGenerator<import("./sse.js").ServerEvent, any, any>;
    /**
     * 型の付いていない要求（新しい操作を SDK より先に使うとき）。`path` は `/api/v1` の後ろ。
     * @template [T=unknown]
     * @param {string} method
     * @param {string} path
     * @param {RequestOptions} [o]
     * @returns {Promise<import('./http.js').HttpResult<T>>}
     */
    request<T = unknown>(method: string, path: string, o?: RequestOptions): Promise<import("./http.js").HttpResult<T>>;
}
export type Stream = import("./openapi.gen.js").Stream;
export type StreamCreate = import("./openapi.gen.js").StreamCreate;
export type StreamPatch = import("./openapi.gen.js").StreamPatch;
export type Recording = import("./openapi.gen.js").Recording;
export type RecordingPatch = import("./openapi.gen.js").RecordingPatch;
export type Schedule = import("./openapi.gen.js").Schedule;
export type ScheduleCreate = import("./openapi.gen.js").ScheduleCreate;
export type SchedulePatch = import("./openapi.gen.js").SchedulePatch;
export type Token = import("./openapi.gen.js").Token;
export type TokenCreate = import("./openapi.gen.js").TokenCreate;
export type Input = import("./openapi.gen.js").Input;
export type InputCreate = import("./openapi.gen.js").InputCreate;
export type Output = import("./openapi.gen.js").Output;
export type OutputCreate = import("./openapi.gen.js").OutputCreate;
export type IoPatch = import("./openapi.gen.js").IoPatch;
export type Secret = import("./openapi.gen.js").Secret;
export type SecretCreate = import("./openapi.gen.js").SecretCreate;
export type Pipeline = import("./openapi.gen.js").Pipeline;
export type PipelineCreate = import("./openapi.gen.js").PipelineCreate;
export type PipelinePatch = import("./openapi.gen.js").PipelinePatch;
export type Node = import("./openapi.gen.js").Node;
export type Room = import("./openapi.gen.js").Room;
export type RoomCreate = import("./openapi.gen.js").RoomCreate;
export type RoomPatch = import("./openapi.gen.js").RoomPatch;
export type RoomParticipant = import("./openapi.gen.js").Participant;
export type MediaPipeline = import("./openapi.gen.js").MediaPipeline;
export type MediaPipelineCreate = import("./openapi.gen.js").MediaPipelineCreate;
export type MediaPipelinePatch = import("./openapi.gen.js").MediaPipelinePatch;
export type AuditEntry = import("./openapi.gen.js").AuditEntry;
export type AuditList = import("./openapi.gen.js").AuditList;
export type IfMatch = import("./http.js").IfMatch;
export type RequestOptions = import("./http.js").RequestOptions;
/**
 * 呼び出しごとの指定。
 */
export type CallOptions = {
    signal?: AbortSignal;
};
/**
 * 作成の指定（Idempotency-Key を自分で決めると、同じ鍵の再送は 24 時間同じ答えになる）。
 */
export type CreateOptions = {
    signal?: AbortSignal;
    idempotencyKey?: string;
};
/**
 * 一覧の指定。
 */
export type ListOptions = {
    limit?: number;
    cursor?: string;
    signal?: AbortSignal;
};
/**
 * 監査ログの絞り込み（`GET /audit`）。時刻は RFC 3339 の文字列か Date。
 */
export type AuditFilter = {
    since?: string | Date;
    until?: string | Date;
    action?: string;
    subject?: string;
    resource?: string;
    q?: string;
    node?: string;
    tenant?: string;
};
import { Http } from './http.js';
import { Paginator } from './http.js';
