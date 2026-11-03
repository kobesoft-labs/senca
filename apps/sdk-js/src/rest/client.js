// 管理 API（/api/v1、docs/api/openapi.yaml）の client。資源ごとに create・list・get・update・delete と、その資源の操作を持つ。
//
//   const senca = new SencaClient({ baseUrl: 'https://node.example', token: 'senca_...' });
//   const s = await senca.streams.create({ name: 'studio-a' });          // Idempotency-Key は自動
//   for await (const x of senca.streams.list()) console.log(x.name);    // cursor をたどる
//   await senca.streams.update(s.id, { metadata: { a: 'b' } }, { ifMatch: s });   // If-Match（generation から）
//   await senca.streams.modify(s.id, (cur) => ({ metadata: { ...cur.metadata, n: '1' } }));  // 412 なら読み直して再試行
//   for await (const ev of senca.events({ types: ['stream.*'] })) ...   // SSE

import { ApiError } from '../errors.js';
import { Http, Paginator, etagOf, newIdempotencyKey } from './http.js';
import { streamEvents } from './sse.js';
import { OPERATIONS } from './operations.gen.js';

/** @typedef {import('./openapi.gen.js').Stream} Stream */
/** @typedef {import('./openapi.gen.js').StreamCreate} StreamCreate */
/** @typedef {import('./openapi.gen.js').StreamPatch} StreamPatch */
/** @typedef {import('./openapi.gen.js').Recording} Recording */
/** @typedef {import('./openapi.gen.js').RecordingPatch} RecordingPatch */
/** @typedef {import('./openapi.gen.js').Schedule} Schedule */
/** @typedef {import('./openapi.gen.js').ScheduleCreate} ScheduleCreate */
/** @typedef {import('./openapi.gen.js').SchedulePatch} SchedulePatch */
/** @typedef {import('./openapi.gen.js').Token} Token */
/** @typedef {import('./openapi.gen.js').TokenCreate} TokenCreate */
/** @typedef {import('./openapi.gen.js').Input} Input */
/** @typedef {import('./openapi.gen.js').InputCreate} InputCreate */
/** @typedef {import('./openapi.gen.js').Output} Output */
/** @typedef {import('./openapi.gen.js').OutputCreate} OutputCreate */
/** @typedef {import('./openapi.gen.js').IoPatch} IoPatch */
/** @typedef {import('./openapi.gen.js').Secret} Secret */
/** @typedef {import('./openapi.gen.js').SecretCreate} SecretCreate */
/** @typedef {import('./openapi.gen.js').Pipeline} Pipeline */
/** @typedef {import('./openapi.gen.js').PipelineCreate} PipelineCreate */
/** @typedef {import('./openapi.gen.js').PipelinePatch} PipelinePatch */
/** @typedef {import('./openapi.gen.js').Node} Node */
/** @typedef {import('./openapi.gen.js').Room} Room */
/** @typedef {import('./openapi.gen.js').RoomCreate} RoomCreate */
/** @typedef {import('./openapi.gen.js').RoomPatch} RoomPatch */
/** @typedef {import('./openapi.gen.js').Participant} RoomParticipant */
/** @typedef {import('./openapi.gen.js').MediaPipeline} MediaPipeline */
/** @typedef {import('./openapi.gen.js').MediaPipelineCreate} MediaPipelineCreate */
/** @typedef {import('./openapi.gen.js').MediaPipelinePatch} MediaPipelinePatch */
/** @typedef {import('./http.js').IfMatch} IfMatch */
/** @typedef {import('./http.js').RequestOptions} RequestOptions */

/**
 * 呼び出しごとの指定。
 * @typedef {{signal?: AbortSignal}} CallOptions
 */
/**
 * 作成の指定（Idempotency-Key を自分で決めると、同じ鍵の再送は 24 時間同じ答えになる）。
 * @typedef {{signal?: AbortSignal, idempotencyKey?: string}} CreateOptions
 */
/**
 * 一覧の指定。
 * @typedef {{limit?: number, cursor?: string, signal?: AbortSignal}} ListOptions
 */

/** @param {string} s */
const enc = (s) => encodeURIComponent(s);

/**
 * create・list・get・update・delete を持つ資源。
 * @template T, C, P
 */
export class Collection {
  /**
   * @internal
   * @param {Http} http
   * @param {string} path 一覧のパス（`/streams`）
   * @param {string} [itemPath] 1 つのパスの前半（既定は path と同じ）
   */
  constructor(http, path, itemPath) {
    /** @protected */
    this.http = http;
    /** @protected */
    this.path = path;
    /** @protected */
    this.itemPath = itemPath ?? path;
  }

  /**
   * 作る（POST、Idempotency-Key を自動で付ける）。
   * @param {C} body
   * @param {CreateOptions} [o]
   * @returns {Promise<T>}
   */
  async create(body, o = {}) {
    return (await this.http.request('POST', this.path, { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /**
   * 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。
   * @param {ListOptions} [o]
   * @returns {Paginator<T>}
   */
  list(o = {}) {
    return new Paginator((cursor, signal) => this.listPage({ limit: o.limit, cursor: cursor ?? undefined, signal }), { signal: o.signal, cursor: o.cursor });
  }

  /**
   * 一覧の 1 頁。
   * @param {ListOptions & {query?: Record<string, string | undefined>}} [o]
   * @returns {Promise<{items: T[], next_cursor: string | null}>}
   */
  async listPage(o = {}) {
    return (await this.http.request('GET', this.path, { query: { limit: o.limit, cursor: o.cursor, ...(o.query ?? {}) }, signal: o.signal })).data;
  }

  /**
   * 取得（ETag は `SencaClient.etagOf(x)` で読める）。
   * @param {string} id
   * @param {CallOptions} [o]
   * @returns {Promise<T>}
   */
  async get(id, o = {}) {
    return (await this.http.request('GET', `${this.itemPath}/${enc(id)}`, { signal: o.signal })).data;
  }

  /**
   * 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。
   * @param {string} id
   * @param {P} patch
   * @param {CallOptions & {ifMatch: IfMatch}} o
   * @returns {Promise<T>}
   */
  async update(id, patch, o) {
    if (!o || o.ifMatch === undefined) throw new TypeError('update: ifMatch が要る（ETag・generation・資源）。読み直して書くなら modify()');
    return (await this.http.request('PATCH', `${this.itemPath}/${enc(id)}`, { body: patch, ifMatch: o.ifMatch, signal: o.signal })).data;
  }

  /**
   * 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。
   * @param {string} id
   * @param {(current: T) => P | Promise<P>} change
   * @param {CallOptions & {attempts?: number}} [o]
   * @returns {Promise<T>}
   */
  async modify(id, change, o = {}) {
    const attempts = o.attempts ?? 3;
    for (let i = 0; ; i++) {
      const cur = await this.get(id, o);
      try {
        return await this.update(id, await change(cur), { ifMatch: /** @type {any} */ (etagOf(/** @type {any} */ (cur))), signal: o.signal });
      } catch (e) {
        if (!(e instanceof ApiError) || e.status !== 412 || i + 1 >= attempts) throw e;
      }
    }
  }

  /**
   * 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。
   * @param {string} id
   * @param {CallOptions & {ifMatch?: IfMatch}} [o]
   * @returns {Promise<void>}
   */
  async delete(id, o = {}) {
    await this.http.request('DELETE', `${this.itemPath}/${enc(id)}`, { ifMatch: o.ifMatch, signal: o.signal });
  }
}

/** @extends {Collection<Stream, StreamCreate, StreamPatch>} */
export class Streams extends Collection {
  /** @internal @param {Http} http */
  constructor(http) {
    super(http, '/streams');
    /** stream の静止画。 */
    this.snapshots = {
      /** @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').SnapshotList['items']>} */
      list: async (id, o = {}) => (await http.request('GET', `/streams/${enc(id)}/snapshots`, { signal: o.signal })).data.items,
      /** @param {string} id @param {string} sid @param {CallOptions} [o] @returns {Promise<Blob>} */
      get: async (id, sid, o = {}) => (await http.request('GET', `/streams/${enc(id)}/snapshots/${enc(sid)}`, { as: 'blob', signal: o.signal })).data,
      /** @param {string} id @param {string} sid @param {CallOptions} [o] */
      delete: async (id, sid, o = {}) => { await http.request('DELETE', `/streams/${enc(id)}/snapshots/${enc(sid)}`, { signal: o.signal }); },
    };
  }

  /** stream key を作り直す（新しい key はこの応答でだけ見える）。 @param {string} id @param {CreateOptions} [o] @returns {Promise<Stream>} */
  async rotateKey(id, o = {}) {
    return (await this.http.request('POST', `/streams/${enc(id)}/keys`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 署名付きの再生 URL。 @param {string} id @param {import('./openapi.gen.js').PlaybackRequest} [body] @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').LivePlayback>} */
  async playbackUrl(id, body = {}, o = {}) {
    return (await this.http.request('POST', `/streams/${enc(id)}/playback-url`, { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 録画を始める。 @param {string} id @param {import('./openapi.gen.js').RecordingStart} [body] @param {CreateOptions} [o] @returns {Promise<Recording>} */
  async startRecording(id, body = {}, o = {}) {
    return (await this.http.request('POST', `/streams/${enc(id)}/recordings`, { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 計画（入力・出力・変換の見積もりと不適合の理由）。 @param {string} id @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').Plan>} */
  async plan(id, o = {}) {
    return (await this.http.request('POST', `/streams/${enc(id)}/plan`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 今の静止画。 @param {string} id @param {CallOptions & {format?: 'jpeg' | 'png'}} [o] @returns {Promise<Blob>} */
  async snapshot(id, o = {}) {
    return (await this.http.request('GET', `/streams/${enc(id)}/snapshot`, { query: { format: o.format }, as: 'blob', signal: o.signal })).data;
  }
}

/** @extends {Collection<Recording, never, RecordingPatch>} */
export class Recordings extends Collection {
  /** @internal @param {Http} http */
  constructor(http) {
    super(http, '/recordings');
  }

  /**
   * 録画は stream から始める（`streams.startRecording`）。
   * @param {never} _body
   * @returns {Promise<never>}
   */
  async create(_body) {
    throw new TypeError('録画は client.streams.startRecording(streamId) で始める');
  }

  /**
   * 一覧（`stream` で絞れる）。
   * @param {ListOptions & {stream?: string}} [o]
   * @returns {Paginator<Recording>}
   */
  list(o = {}) {
    return new Paginator((cursor, signal) => this.listPage({ limit: o.limit, cursor: cursor ?? undefined, signal, query: { stream: o.stream } }), { signal: o.signal, cursor: o.cursor });
  }

  /** 消す（非同期。応答は消している途中の録画）。 @param {string} id @param {CallOptions & {ifMatch?: IfMatch}} [o] @returns {Promise<void>} */
  async delete(id, o = {}) {
    await this.http.request('DELETE', `/recordings/${enc(id)}`, { ifMatch: o.ifMatch, signal: o.signal });
  }

  /** 止める。 @param {string} id @param {CreateOptions} [o] @returns {Promise<Recording>} */
  async stop(id, o = {}) {
    return (await this.http.request('POST', `/recordings/${enc(id)}/stop`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 署名付きの VOD の URL。 @param {string} id @param {import('./openapi.gen.js').PlaybackRequest} [body] @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').VodPlayback>} */
  async playbackUrl(id, body = {}, o = {}) {
    return (await this.http.request('POST', `/recordings/${enc(id)}/playback-url`, { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** サムネイル（JPEG）。 @param {string} id @param {CallOptions} [o] @returns {Promise<Blob>} */
  async thumbnail(id, o = {}) {
    return (await this.http.request('GET', `/recordings/${enc(id)}/thumbnail`, { as: 'blob', signal: o.signal })).data;
  }

  /** MP4 の書き出し（大きいので Response のまま返す。`res.body` を流して読む）。 @param {string} id @param {CallOptions} [o] @returns {Promise<Response>} */
  async exportMp4(id, o = {}) {
    return (await this.http.request('GET', `/recordings/${enc(id)}/export.mp4`, { as: 'response', signal: o.signal, timeoutMs: 0 })).data;
  }
}

/** @extends {Collection<Schedule, ScheduleCreate, SchedulePatch>} */
export class Schedules extends Collection {
  /** @internal @param {Http} http */
  constructor(http) {
    super(http, '/schedules');
  }

  /** @param {ListOptions & {stream?: string}} [o] @returns {Paginator<Schedule>} */
  list(o = {}) {
    return new Paginator((cursor, signal) => this.listPage({ limit: o.limit, cursor: cursor ?? undefined, signal, query: { stream: o.stream } }), { signal: o.signal, cursor: o.cursor });
  }

  /** 作らずに確かめる（重なり・上限）。 @param {ScheduleCreate} body @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').ScheduleCheck>} */
  async check(body, o = {}) {
    return (await this.http.request('POST', '/schedules/check', { body, signal: o.signal })).data;
  }

  /** 実行の履歴。 @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').ScheduleRun[]>} */
  async runs(id, o = {}) {
    return (await this.http.request('GET', `/schedules/${enc(id)}/runs`, { signal: o.signal })).data.items;
  }
}

/** API トークン（更新は無い）。 */
export class Tokens {
  /** @internal @param {Http} http */
  constructor(http) {
    /** @private @type {Collection<Token, TokenCreate, never>} */
    this.c = new Collection(http, '/tokens');
  }

  /** 作る（`token` の値はこの応答でだけ見える）。 @param {TokenCreate} body @param {CreateOptions} [o] */
  create(body, o) { return this.c.create(body, o); }
  /** @param {ListOptions} [o] */
  list(o) { return this.c.list(o); }
  /** @param {ListOptions} [o] */
  listPage(o) { return this.c.listPage(o); }
  /** @param {string} id @param {CallOptions} [o] */
  get(id, o) { return this.c.get(id, o); }
  /** 失効させる。 @param {string} id @param {CallOptions} [o] */
  delete(id, o) { return this.c.delete(id, o); }
}

/**
 * stream の下の入力・出力（作成・一覧は stream ごと、他は ID で）。
 * @template T, C
 * @extends {Collection<T, C, IoPatch>}
 */
export class StreamIo extends Collection {
  /** @internal @param {Http} http @param {'inputs' | 'outputs'} kind */
  constructor(http, kind) {
    super(http, `/${kind}`);
    /** @private */
    this.kind = kind;
  }

  /**
   * stream に作る。
   * @param {string} streamId
   * @param {C} body
   * @param {CreateOptions} [o]
   * @returns {Promise<T>}
   */
  // @ts-ignore 引数の形が Collection.create と違う（stream の ID を先に取る）
  async create(streamId, body, o = {}) {
    return (await this.http.request('POST', `/streams/${enc(streamId)}/${this.kind}`, { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /**
   * stream の一覧。
   * @param {string} streamId
   * @param {ListOptions} [o]
   * @returns {Paginator<T>}
   */
  // @ts-ignore 引数の形が Collection.list と違う
  list(streamId, o = {}) {
    return new Paginator(async (cursor, signal) => (await this.http.request('GET', `/streams/${enc(streamId)}/${this.kind}`, { query: { limit: o.limit, cursor }, signal })).data, { signal: o.signal, cursor: o.cursor });
  }
}

/** 秘密値（値は読めない。作成・一覧・削除だけ）。 */
export class Secrets {
  /** @internal @param {Http} http */
  constructor(http) {
    /** @private */
    this.http = http;
  }

  /** @param {SecretCreate} body @param {CreateOptions} [o] @returns {Promise<Secret>} */
  async create(body, o = {}) {
    return (await this.http.request('POST', '/secrets', { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 全部（頁は無い）。 @param {CallOptions} [o] @returns {Promise<Secret[]>} */
  async list(o = {}) {
    return (await this.http.request('GET', '/secrets', { signal: o.signal })).data.items;
  }

  /** @param {string} id @param {CallOptions} [o] */
  async delete(id, o = {}) {
    await this.http.request('DELETE', `/secrets/${enc(id)}`, { signal: o.signal });
  }
}

/** @extends {Collection<Pipeline, PipelineCreate, PipelinePatch>} */
export class Pipelines extends Collection {
  /** @internal @param {Http} http */
  constructor(http) {
    super(http, '/pipelines');
  }

  /** グラフを検証する（保存前のグラフも）。 @param {string} id @param {import('./openapi.gen.js').PipelineGraph} graph @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').PipelineValidation>} */
  async validate(id, graph, o = {}) {
    return (await this.http.request('POST', `/pipelines/${enc(id)}/validate`, { body: graph, signal: o.signal })).data;
  }

  /** 動いているパイプラインを操作する（スイッチャーの take など）。 @param {string} id @param {import('./openapi.gen.js').PipelineControl} op @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').PipelineControlResult>} */
  async control(id, op, o = {}) {
    return (await this.http.request('POST', `/pipelines/${enc(id)}/control`, { body: op, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }
}

/** ノード（運用、`node:admin`）。 */
export class Nodes {
  /** @internal @param {Http} http */
  constructor(http) {
    /** @private */
    this.http = http;
    /** GPU・キャプチャ機器の切り離し・戻し。 */
    this.devices = {
      /** @param {string} nodeId @param {string} device @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').DeviceHealth>} */
      disable: async (nodeId, device, o = {}) => (await http.request('POST', `/nodes/${enc(nodeId)}/devices/${enc(device)}/disable`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data,
      /** @param {string} nodeId @param {string} device @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').DeviceHealth>} */
      enable: async (nodeId, device, o = {}) => (await http.request('POST', `/nodes/${enc(nodeId)}/devices/${enc(device)}/enable`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data,
    };
  }

  /** @param {CallOptions} [o] @returns {Promise<Node[]>} */
  async list(o = {}) {
    return (await this.http.request('GET', '/nodes', { signal: o.signal })).data.items;
  }

  /** @param {string} id @param {CallOptions} [o] @returns {Promise<Node>} */
  async get(id, o = {}) {
    return (await this.http.request('GET', `/nodes/${enc(id)}`, { signal: o.signal })).data;
  }

  /** 新しい仕事を受けない（今の仕事は移す）。 @param {string} id @param {CreateOptions} [o] @returns {Promise<Node>} */
  async drain(id, o = {}) {
    return (await this.http.request('POST', `/nodes/${enc(id)}/drain`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** @param {string} id @param {CreateOptions} [o] @returns {Promise<Node>} */
  async undrain(id, o = {}) {
    return (await this.http.request('POST', `/nodes/${enc(id)}/undrain`, { signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }
}

/**
 * リアルタイムの部屋（ADR-0015）。部屋そのものはどのノードでも扱え、参加者・message・統計は部屋を置いたノードへ中継される。
 * ブラウザーは `createToken()` の `token` と `signaling_url` で `Senca.connect({url: signaling_url, room, token})`。
 * @extends {Collection<Room, RoomCreate, RoomPatch>}
 */
export class Rooms extends Collection {
  /** @internal @param {Http} http */
  constructor(http) {
    super(http, '/rooms');
  }

  /**
   * 部屋の参加者の操作。
   * @param {string} roomId
   */
  participants(roomId) {
    const http = this.http;
    const base = `/rooms/${enc(roomId)}/participants`;
    return {
      /** @param {ListOptions} [o] @returns {Paginator<RoomParticipant>} */
      list: (o = {}) => new Paginator(async (cursor, signal) => (await http.request('GET', base, { query: { limit: o.limit, cursor }, signal })).data, { signal: o.signal, cursor: o.cursor }),
      /** @param {string} connectionId @param {CallOptions} [o] @returns {Promise<RoomParticipant>} */
      get: async (connectionId, o = {}) => (await http.request('GET', `${base}/${enc(connectionId)}`, { signal: o.signal })).data,
      /** 他の参加者に見せる metadata を変える（参加者には `connection.updated` が届く）。 @param {string} connectionId @param {unknown} metadata @param {CallOptions} [o] @returns {Promise<RoomParticipant>} */
      updateMetadata: async (connectionId, metadata, o = {}) => (await http.request('PATCH', `${base}/${enc(connectionId)}`, { body: { metadata }, signal: o.signal })).data,
      /** 切断する（参加者には `disconnect {reason: "kicked"}`）。 @param {string} connectionId @param {CallOptions} [o] */
      disconnect: async (connectionId, o = {}) => { await http.request('DELETE', `${base}/${enc(connectionId)}`, { signal: o.signal }); },
    };
  }

  /** 部屋へ送る（`label` があれば DataChannel、無ければシグナリングの push）。 @param {string} roomId @param {import('./openapi.gen.js').RoomMessage} msg @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').RoomMessageResult>} */
  async sendMessage(roomId, msg, o = {}) {
    return (await this.http.request('POST', `/rooms/${enc(roomId)}/messages`, { body: msg, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }

  /** 統計（参加者ごとの RTT・帯域・送受の量・層・クライアントの getStats）。 @param {string} roomId @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').RoomStats>} */
  async stats(roomId, o = {}) {
    return (await this.http.request('GET', `/rooms/${enc(roomId)}/stats`, { signal: o.signal })).data;
  }

  /** 参加トークンを作る（`token` と `signaling_url` をブラウザーへ渡す）。 @param {string} roomId @param {import('./openapi.gen.js').RoomTokenRequest} body @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').RoomToken>} */
  async createToken(roomId, body, o = {}) {
    return (await this.http.request('POST', `/rooms/${enc(roomId)}/tokens`, { body, signal: o.signal, idempotencyKey: o.idempotencyKey })).data;
  }
}

/**
 * 部屋のメディアパイプライン（ADR-0016、AWS Chime SDK Media Pipelines 相当）。部屋の参加者を stream に書き出し、
 * 録画（`recording`）・合成（`composite`）・送り出し（`egress`）・文字起こし（`captions`）を 1 つの資源で指定する。
 * 子の資源（書き出した stream・合成のパイプライン・送り出しの出力）の ID は `artifacts` に出る。
 * @extends {Collection<MediaPipeline, MediaPipelineCreate, MediaPipelinePatch>}
 */
export class MediaPipelines extends Collection {
  /** @internal @param {Http} http */
  constructor(http) {
    super(http, '/media-pipelines');
  }

  /**
   * 一覧（`room` で部屋を絞る。今のノードは `room` を 400 にする不備があるので、絞るときは全件から `room` で選ぶ）。
   * @param {ListOptions & {room?: string}} [o]
   * @returns {Paginator<MediaPipeline>}
   */
  list(o = {}) {
    return new Paginator((cursor, signal) => this.listPage({ limit: o.limit, cursor: cursor ?? undefined, signal, query: { room: o.room } }), { signal: o.signal, cursor: o.cursor });
  }

  /** 動かす（`desired_state: running`）。 @param {string} id @param {CallOptions & {ifMatch?: IfMatch}} [o] @returns {Promise<MediaPipeline>} */
  async start(id, o = {}) {
    return this.setState(id, 'running', o);
  }

  /** 止める（`desired_state: stopped`。合成・送り出しを片付け、書き出した stream・録画は残る）。 @param {string} id @param {CallOptions & {ifMatch?: IfMatch}} [o] @returns {Promise<MediaPipeline>} */
  async stop(id, o = {}) {
    return this.setState(id, 'stopped', o);
  }

  /** @private @param {string} id @param {'running' | 'stopped'} state @param {CallOptions & {ifMatch?: IfMatch}} o @returns {Promise<MediaPipeline>} */
  async setState(id, state, o) {
    if (o.ifMatch !== undefined) return this.update(id, { desired_state: state }, { ifMatch: o.ifMatch, signal: o.signal });
    return this.modify(id, () => ({ desired_state: state }), { signal: o.signal });
  }
}

/** 管理 API の client。 */
export class SencaClient {
  /** @param {import('./http.js').ClientOptions} options */
  constructor(options) {
    /** @private */
    this.http = new Http(options);
    this.streams = new Streams(this.http);
    this.recordings = new Recordings(this.http);
    this.schedules = new Schedules(this.http);
    this.tokens = new Tokens(this.http);
    /** @type {StreamIo<Input, InputCreate>} */
    this.inputs = new StreamIo(this.http, 'inputs');
    /** @type {StreamIo<Output, OutputCreate>} */
    this.outputs = new StreamIo(this.http, 'outputs');
    this.secrets = new Secrets(this.http);
    this.pipelines = new Pipelines(this.http);
    this.nodes = new Nodes(this.http);
    /** リアルタイムの部屋。 */
    this.rooms = new Rooms(this.http);
    /** 部屋のメディアパイプライン（書き出し・録画・合成・送り出し・文字起こし）。 */
    this.mediaPipelines = new MediaPipelines(this.http);
  }

  /** API の base（`.../api/v1`）。 */
  get baseUrl() {
    return this.http.base;
  }

  /** 対応版・コーデック・機能。 @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').Capabilities>} */
  async capabilities(o = {}) {
    return (await this.http.request('GET', '/capabilities', { signal: o.signal })).data;
  }

  /** 全ノードのハードウェア。 @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').Devices>} */
  async devices(o = {}) {
    return (await this.http.request('GET', '/devices', { signal: o.signal })).data;
  }

  /** グリッド（ノード・資源・仕事の配置）。 @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').Grid>} */
  async grid(o = {}) {
    return (await this.http.request('GET', '/grid', { signal: o.signal })).data;
  }

  /**
   * SSE のイベント。切れたら付け直す。`for await (const ev of client.events({types: ['stream.*']}))`。
   * @param {import('./sse.js').EventsOptions} [o]
   */
  events(o = {}) {
    return streamEvents(this.http, o);
  }

  /**
   * 型の付いていない要求（新しい操作を SDK より先に使うとき）。`path` は `/api/v1` の後ろ。
   * @template [T=unknown]
   * @param {string} method
   * @param {string} path
   * @param {RequestOptions} [o]
   * @returns {Promise<import('./http.js').HttpResult<T>>}
   */
  request(method, path, o) {
    return this.http.request(method, path, o);
  }

  /**
   * 資源の ETag（GET・PATCH の `ETag`、無ければ `"{generation}"`）。
   * @param {object} resource
   */
  static etagOf(resource) {
    return etagOf(resource);
  }

  /** 新しい Idempotency-Key（同じ作成を確実に 1 回にしたいとき、自分で持って再送に使う）。 */
  static newIdempotencyKey() {
    return newIdempotencyKey();
  }

  /** 管理 API の操作の表（operationId → method・path・権限。docs/api/openapi.yaml から生成）。 */
  static get operations() {
    return OPERATIONS;
  }
}
