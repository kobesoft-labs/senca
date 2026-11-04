# @senca/sdk API

`node scripts/build.mjs` が型定義（types/）から作る。手で直さない。使い方は README.md。

## 関数・定数

### `connect`

```ts
connect: (options: ConnectOptions) => Promise<Session>
```

部屋に入る。参加し、最初の交渉が済み、ピア接続がつながったら解決する。

### `etagOf`

```ts
etagOf: (resource: object) => string | undefined
```

資源の ETag（GET・PATCH の応答の `ETag`、無ければ `"{generation}"`）。

### `DEFAULT_SIMULCAST`

```ts
DEFAULT_SIMULCAST: readonly (Readonly<{ rid: "r0"; scaleResolutionDownBy: 4; maxBitrateKbps: 150; }> | Readonly<{ rid: "r1"; scaleResolutionDownBy: 2; maxBitrateKbps: 500; }> | Readonly<{ rid: "r2"; scaleResolutionDownBy: 1; maxBitrateKbps: 1500; }>)[]
```

既定の 3 層（低い順。Chrome で 3 層にするには送る映像が 1280x720 以上であること）。

### `PROTOCOL_VERSION`

```ts
PROTOCOL_VERSION: 1
```

プロトコルの版。

### `SDK_NAME`

```ts
SDK_NAME: "@senca/sdk"
```

### `SDK_VERSION`

```ts
SDK_VERSION: "0.0.1"
```

### `Senca`

```ts
Senca: Readonly<{ connect: (options: ConnectOptions) => Promise<Session>; version: "0.0.1"; }>
```

入口（`Senca.connect()`）。

- `Senca.connect`: 部屋に入る。参加し、最初の交渉が済み、ピア接続がつながったら解決する。
- `Senca.version`: 

### `SUBPROTOCOL`

```ts
SUBPROTOCOL: "senca.rooms.v1"
```

WebSocket の subprotocol。

## クラス

### `ApiError`

REST（problem+json、RFC 9457）の誤り。`code` は problem の `code`（`NOT_FOUND`・`PRECONDITION_FAILED` など）。
輸送の失敗（応答が無い）は `status` 0・`code` `NETWORK_ERROR`。

| メンバー | 説明 |
|---|---|
| `new ApiError(p: { status: number; code: string; title: string; type?: string \| undefined; detail?: string \| undefined; requestId?: string \| null \| undefined; retryable?: boolean \| undefined; retryAfterMs?: number \| ... 1 more ... \| undefined; ... 4 more ...; cause?: unknown; }): ApiError` | |
| `status: number` | HTTP の状態コード（輸送の失敗は 0）。 |
| `title: string` |  |
| `type: string \| null` | problem の `type`（`urn:senca:error:...`）。 |
| `detail: string \| null` |  |
| `requestId: string \| null` | `X-Request-Id`（問い合わせのときに伝える）。 |
| `retryable: boolean` | 同じ要求を送り直してよいか（problem の `retryable`、408・429・502・503・504、輸送の失敗）。 |
| `retryAfterMs: number \| null` | `Retry-After`（ミリ秒）。 |
| `details: {} \| null` | problem の `details`（code ごとの補足。パイプラインの検証の問題など）。 |
| `method: string \| undefined` |  |
| `path: string \| undefined` |  |
| `problem: Record<string, unknown> \| null` | problem+json の本文そのもの。 |
| `code: string` | 誤りの種類（シグナリングの `code`、REST の problem の `code`、SDK の code）。 |
| `fatal: boolean` | 真ならセッション（接続）は続かない。偽ならその要求だけが失敗した。 |
| `closeCode: number \| undefined` | WebSocket の close code（あれば）。 |
| `request: string \| undefined` | 誤りの元になった message の `type`（シグナリング）。 |
| `name: string` |  |
| `message: string` |  |
| `stack?: string` |  |
| `cause?: unknown` |  |

### `Audit`

監査ログ（読み出しだけ。新しい順。クラスターでは全ノードの分を合わせて返す）。

| メンバー | 説明 |
|---|---|
| `list(o?: ListOptions & AuditFilter): Paginator<AuditEntry>` | 一覧（cursor を自動でたどる）。`action` は完全一致、`.` か `*` で終われば前方一致。`tenant` は別のテナント・`*` に node:admin が要る。 |
| `listPage(o?: ListOptions & AuditFilter): Promise<AuditList>` | 一覧の 1 頁（`unavailable_nodes` は答えなかったノード。項目が `limit` より少なくても `next_cursor` があれば続きがある）。 |

### `Auth`

ログインと本人の操作（`/auth/...`）。`providers`・`login`・`loginMfa`・`exchange` は認証なしで使えます（token を省いた client でよい）。
ログインで得た `token`（`sencas_...`）は `client.setToken(token)` で以後の要求に使います。

| メンバー | 説明 |
|---|---|
| `providers(o?: CallOptions): Promise<import("./openapi.gen.js").AuthProviders>` | ログインの方法（パスワード・IdP の一覧・最初の管理者が要るか）。 |
| `login(body: import("./openapi.gen.js").LoginRequest, o?: CallOptions): Promise<LoginSuccess \| LoginMfaRequired>` | パスワードでログインします。`status: 'mfa_required'` なら `loginMfa()` に `mfa_token` を渡します。 |
| `loginMfa(body: import("./openapi.gen.js").LoginMfaRequest, o?: CallOptions): Promise<LoginSuccess>` | 2 つ目の要素（TOTP のコードか回復コード）でログインします。 |
| `exchange(code: string, o?: CallOptions & { cookie?: string; }): Promise<LoginSuccess>` | OIDC の一度きりのコード（`/console/#/auth/complete?code=...`）をセッションに替えます。ログインを始めたブラウザーのクッキー（`senca_oidc`）を送るため、 |
| `oidcStartUrl(provider: string, returnTo?: string): string` | OIDC のログインを始める URL（ブラウザーで開くと IdP へリダイレクトします）。`returnTo` は管理画面のパス（`/console/...`）。 |
| `oidcCallbackUrl(provider: string): string` | OIDC のコールバックの URL（IdP に登録する redirect URI の既定値。ブラウザーが IdP から戻る先です）。 |
| `me(o?: CallOptions): Promise<import("./openapi.gen.js").Me>` | 自分の情報（主体・テナント・権限・セッション・先に済ませること）。 |
| `logout(o?: CallOptions): Promise<void>` | ログアウト（今のセッションを失効させます）。 |
| `changePassword(body: import("./openapi.gen.js").PasswordChange, o?: CallOptions): Promise<void>` | 自分のパスワードを変えます（ほかのセッションは失効します）。 |
| `setupTotp(body: import("./openapi.gen.js").MfaSetupRequest, o?: CallOptions): Promise<import("./openapi.gen.js").MfaSetup>` | TOTP の登録を始めます（`secret`・`otpauth_uri` はこの応答でだけ返ります）。 |
| `confirmTotp(body: import("./openapi.gen.js").MfaConfirmRequest, o?: CallOptions): Promise<import("./openapi.gen.js").MfaConfirmResult>` | 確認コードで TOTP を有効にします（回復コードはこの応答でだけ返ります）。 |
| `disableMfa(body: import("./openapi.gen.js").MfaReauth, o?: CallOptions): Promise<void>` | 自分の MFA を解除します（`password` か `code`）。 |
| `regenerateRecoveryCodes(body: import("./openapi.gen.js").MfaReauth, o?: CallOptions): Promise<import("./openapi.gen.js").RecoveryCodes>` | 回復コードを作り直します。 |
| `sessions(o?: ListOptions): Paginator<Session>` | 自分のセッションの一覧。 |
| `revokeSession(id: string, o?: CallOptions): Promise<void>` | 自分のセッションを失効させます。 |

### `Collection`

create・list・get・update・delete を持つ資源。

| メンバー | 説明 |
|---|---|
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `list(o?: ListOptions): Paginator<T>` | 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `EventStreamParser`

text/event-stream の解析器（行を足すと、まとまったイベントを返す）。

| メンバー | 説明 |
|---|---|
| `buf: string` |  |
| `event: string` |  |
| `data: string[]` |  |
| `id: string \| undefined` |  |
| `lastId: string \| null` |  |
| `retry: number \| null` |  |
| `feed(text: string): Array<{ type: string; id: string \| null; data: string; }>` |  |

### `InboundMessage`

届いたアプリの message（DataChannel の `#` ラベル、またはシグナリングの `message`）。

| メンバー | 説明 |
|---|---|
| `label: string` |  |
| `data: string \| Uint8Array<ArrayBufferLike>` | 本文（text は string、binary は Uint8Array）。 |
| `from: string \| null` | 送り手の connection_id。DataChannel で部屋全体に届いたもの（送り手を運ばない）と API からのものは null。 |
| `via: "signaling" \| "datachannel"` | 届いた経路。 |
| `receivedAt: number` |  |
| `sender: Participant \| undefined` | 送り手（分からなければ undefined）。 |
| `binary: boolean` |  |
| `text(): string` | 本文を文字列で。 |
| `json(): any` | 本文を JSON として読む（`send()` にオブジェクトを渡すと JSON で送る）。 |

### `LocalTrack`

送っているトラック 1 本。

| メンバー | 説明 |
|---|---|
| `kind: Kind` |  |
| `mediaStreamTrack: MediaStreamTrack` | 送っている MediaStreamTrack（`replace()` で替わる）。 |
| `id: string \| undefined` | サーバーが付けたトラック ID（answer の `published`。再接続で変わる）。 |
| `mid: string \| undefined` | 送る m 行の mid。 |
| `rids: readonly string[]` | サイマルキャストの層（低い順）。 |
| `publication: Publication` |  |
| `replace(track: MediaStreamTrack \| null): Promise<void>` | 別の MediaStreamTrack に差し替える（再交渉しない。カメラの切り替えなど）。 |
| `enabled: boolean` | 送るのを止める・再開する（MediaStreamTrack.enabled。止めている間は黒・無音を送る）。 |

### `MediaPipelines`

部屋のメディアパイプライン（ADR-0016、AWS Chime SDK Media Pipelines 相当）。部屋の参加者を stream に書き出し、
録画（`recording`）・合成（`composite`）・送り出し（`egress`）・文字起こし（`captions`）を 1 つの資源で指定する。
子の資源（書き出した stream・合成のパイプライン・送り出しの出力）の ID は `artifacts` に出る。

| メンバー | 説明 |
|---|---|
| `list(o?: ListOptions & { room?: string; }): Paginator<MediaPipeline>` | 一覧（`room` で部屋を絞れる）。 |
| `start(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<MediaPipeline>` | 動かす（`desired_state: running`）。 |
| `stop(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<MediaPipeline>` | 止める（`desired_state: stopped`。合成・送り出しを片付け、書き出した stream・録画は残る）。 |
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `Nodes`

ノード（運用、`node:admin`）。

| メンバー | 説明 |
|---|---|
| `devices: { /** @param {string} nodeId @param {string} device @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').DeviceHealth>} */ disable: (nodeId: string, device: string, o?: CreateOptions) => Promise<import("./openapi.gen.js").DeviceHealth>; /** @param {string} nodeId @param {string} device @param {CreateOptions} [o] @returns {Promise<import('./openapi.gen.js').DeviceHealth>} */ enable: (nodeId: string, device: string, o?: CreateOptions) => Promise<import("./openapi.gen.js").DeviceHealth>; }` | GPU・キャプチャ機器の切り離し・戻し。 |
| `list(o?: CallOptions): Promise<Node[]>` |  |
| `get(id: string, o?: CallOptions): Promise<Node>` |  |
| `drain(id: string, o?: CreateOptions): Promise<Node>` | 新しい仕事を受けない（今の仕事は移す）。 |
| `undrain(id: string, o?: CreateOptions): Promise<Node>` |  |

### `Paginator`

カーソルでたどる一覧。`for await (const x of list)` で全部、`pages()` で頁ごと。

| メンバー | 説明 |
|---|---|
| `new Paginator<T>(fetchPage: (cursor: string \| null, signal?: AbortSignal \| undefined) => Promise<{ items: T[]; next_cursor?: string \| null \| undefined; }>, o?: { signal?: AbortSignal \| undefined; cursor?: string \| undefined; } \| undefined): Paginator<...>` | |
| `pages(): AsyncGenerator<{ items: T[]; nextCursor: string \| null; }>` | 頁ごと（`next_cursor` が null になるまで）。 |
| `toArray(o?: { max?: number; }): Promise<T[]>` | 全部（`max` 個まで）を配列で。 |
| `first(): Promise<T \| undefined>` | 最初の 1 つ（無ければ undefined）。 |

### `Participant`

部屋の他の参加者（自分は含まない）。オブジェクトは参加している間同じもので、metadata などはその場で変わる。

| メンバー | 説明 |
|---|---|
| `id: string` | connection_id（接続ごとに変わる）。 |
| `clientId: string \| undefined` | client_id（トークン・認証 Webhook が決めたもの）。 |
| `bundleId: string` |  |
| `role: Role` |  |
| `metadata: unknown` |  |
| `node: string \| undefined` | 別のノードの参加者ならそのノード（グリッドのカスケード）。 |
| `tracks: Map<string, PublishedTrackInfo>` |  |
| `remote: boolean` | 別のノードにいる参加者か。 |
| `focused: boolean` | スポットライトで注目されているか。 |
| `subscriptions: RemoteTrack[]` | この参加者から受けているトラック。 |
| `toJSON(): { id: string; clientId: string \| undefined; bundleId: string; role: import("./protocol.js").Role; metadata: unknown; node: string \| undefined; tracks: PublishedTrackInfo[]; }` |  |

### `Pipelines`

| メンバー | 説明 |
|---|---|
| `snapshots: { /** @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').SnapshotList['items']>} */ list: (id: string, o?: CallOptions) => Promise<import("./openapi.gen.js").SnapshotList["items"]>; /** @param {string} id @param {string} sid @param {CallOptions} [o] @returns {Promise<Blob>} */ get: (id: string, sid: string, o?: CallOptions) => Promise<Blob>; /** @param {string} id @param {string} sid @param {CallOptions} [o] */ delete: (id: string, sid: string, o?: CallOptions) => Promise<void>; }` | `snapshot_on_change`（板書の変化）で保存した静止画。 |
| `validate(id: string, graph: import("./openapi.gen.js").PipelineGraph, o?: CallOptions): Promise<import("./openapi.gen.js").PipelineValidation>` | グラフを検証する（保存前のグラフも）。 |
| `control(id: string, op: import("./openapi.gen.js").PipelineControl, o?: CreateOptions): Promise<import("./openapi.gen.js").PipelineControlResult>` | 動いているパイプラインを操作する（スイッチャーの take など）。 |
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `list(o?: ListOptions): Paginator<T>` | 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `Publication`

`session.publish()` の結果。

| メンバー | 説明 |
|---|---|
| `tracks: LocalTrack[]` |  |
| `options: PublishOptions` |  |
| `stream: MediaStream \| undefined` |  |
| `active: boolean` |  |
| `track(kind: Kind): LocalTrack \| undefined` |  |
| `audio: LocalTrack \| undefined` |  |
| `video: LocalTrack \| undefined` |  |
| `unpublish(): Promise<void>` | 公開をやめる（送る m 行を止めて再交渉する。他の参加者には track.unpublished が届く）。 |
| `setMaxBitrate(kbps: number): Promise<void>` | 映像の上限を変える（再交渉しない。サイマルキャストなら一番上の層を、他の層は比で縮める）。 |

### `Recordings`

| メンバー | 説明 |
|---|---|
| `create(_body: never): Promise<never>` | 録画は stream から始める（`streams.startRecording`）。 |
| `list(o?: ListOptions & { stream?: string; }): Paginator<Recording>` | 一覧（`stream` で絞れる）。 |
| `stop(id: string, o?: CreateOptions): Promise<Recording>` | 止める。 |
| `playbackUrl(id: string, body?: import("./openapi.gen.js").PlaybackRequest, o?: CreateOptions): Promise<import("./openapi.gen.js").VodPlayback>` | 署名付きの VOD の URL。 |
| `thumbnail(id: string, o?: CallOptions): Promise<Blob>` | サムネイル（JPEG）。 |
| `exportMp4(id: string, o?: CallOptions): Promise<Response>` | MP4 の書き出し（大きいので Response のまま返す。`res.body` を流して読む）。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `RemoteTrack`

受けているトラック。サーバーが受信の枠にこのトラックを割り当てている間だけ生きる（枠が別の送り手へ替わる・送り手が抜ける・
再接続で `ended`）。`mediaStreamTrack` はブラウザーの受信のトラックで、枠が使い回されると次の RemoteTrack も同じものを持つ。

| メンバー | 説明 |
|---|---|
| `id: string` | トラック ID（`{connection_id}.{mid}`）。 |
| `kind: Kind` |  |
| `participant: Participant \| undefined` | 送り手（`connected`・`connection.created` で知らない送り手なら undefined）。 |
| `participantId: string \| undefined` | 送り手の connection_id。 |
| `clientId: string \| undefined` |  |
| `mid: string` | 受信の枠（m 行）の mid。 |
| `mediaStreamTrack: MediaStreamTrack` | ブラウザーの受信のトラック。 |
| `stream: MediaStream \| undefined` | `mediaStreamTrack` だけを入れた MediaStream（`<video>.srcObject` にそのまま渡せる）。 |
| `rids: readonly string[]` | サイマルキャストの層（低い順）。 |
| `layer: string \| null` | サーバーが今配っている層（`layer.changed` で更新。まだ知らなければ null）。 |
| `requestedLayer: string` | 選んだ層（`setLayer`、既定 `'auto'`）。 |
| `state: "active" \| "paused" \| "ended"` | `active`（届いている）・`paused`（スポットライトの外などで止めている）・`ended`。 |
| `ended: Promise<{ reason: string; }>` | 終わったとき（理由つき）に解決する。 |
| `setLayer(layer: string, opts?: { signal?: AbortSignal; timeoutMs?: number; }): Promise<void>` | 受ける層を選ぶ。`'auto'` はサーバーが帯域で選ぶ。層を指定したときは、サーバーがその層へ切り替えた（`layer.changed`）ときに |
| `events(opts?: { signal?: AbortSignal; }): AsyncIterableIterator<RemoteTrackEvent>` | このトラックの出来事（層の変化・一時停止・終わり）。終わると反復も終わる。 |
| `stats(): Promise<import("./media.js").InboundStats \| undefined>` | 受信の統計（終わっていれば undefined）。 |
| `attach<E extends HTMLMediaElement>(el: E): E` | `<video>`・`<audio>` に付ける（`srcObject` を設定して返す）。 |
| `toJSON(): { id: string; kind: import("./protocol.js").Kind; participantId: string \| undefined; clientId: string \| undefined; mid: string; rids: readonly string[]; layer: string \| null; state: "active" \| "paused" \| "ended"; }` |  |

### `Rooms`

リアルタイムの部屋（ADR-0015）。部屋そのものはどのノードでも扱え、参加者・message・統計は部屋を置いたノードへ中継される。
ブラウザーは `createToken()` の `token` と `signaling_url` で `Senca.connect({url: signaling_url, room, token})`。

| メンバー | 説明 |
|---|---|
| `participants(roomId: string): { /** @param {ListOptions} [o] @returns {Paginator<RoomParticipant>} */ list: (o?: ListOptions) => Paginator<RoomParticipant>; /** @param {string} connectionId @param {CallOptions} [o] @returns {Promise<RoomParticipant>} */ get: (connectionId: string, o?: CallOptions) => Promise<RoomParticipant>; /** 他の参加者に見せる metadata を変える（参加者には `connection.updated` が届く）。 @param {string} connectionId @param {unknown} metadata @param {CallOptions} [o] @returns {Promise<RoomParticipant>} */ updateMetadata: (connectionId: string, metadata: unknown, o?: CallOptions) => Promise<RoomParticipant>; /** 切断する（参加者には `disconnect {reason: "kicked"}`）。 @param {string} connectionId @param {CallOptions} [o] */ disconnect: (connectionId: string, o?: CallOptions) => Promise<void>; }` | 部屋の参加者の操作。 |
| `sendMessage(roomId: string, msg: import("./openapi.gen.js").RoomMessage, o?: CreateOptions): Promise<import("./openapi.gen.js").RoomMessageResult>` | 部屋へ送る（`label` があれば DataChannel、無ければシグナリングの push）。 |
| `stats(roomId: string, o?: CallOptions): Promise<import("./openapi.gen.js").RoomStats>` | 統計（参加者ごとの RTT・帯域・送受の量・層・クライアントの getStats）。 |
| `createToken(roomId: string, body: import("./openapi.gen.js").RoomTokenRequest, o?: CreateOptions): Promise<import("./openapi.gen.js").RoomToken>` | 参加トークンを作る（`token` と `signaling_url` をブラウザーへ渡す）。 |
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `list(o?: ListOptions): Paginator<T>` | 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `Schedules`

| メンバー | 説明 |
|---|---|
| `list(o?: ListOptions & { stream?: string; }): Paginator<Schedule>` | 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。 |
| `check(body: ScheduleCreate, o?: CallOptions): Promise<import("./openapi.gen.js").ScheduleCheck>` | 作らずに確かめる（重なり・上限）。 |
| `runs(id: string, o?: CallOptions): Promise<import("./openapi.gen.js").ScheduleRun[]>` | 実行の履歴。 |
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `Secrets`

秘密値（値は読めない。作成・一覧・削除だけ）。

| メンバー | 説明 |
|---|---|
| `create(body: SecretCreate, o?: CreateOptions): Promise<Secret>` |  |
| `list(o?: CallOptions): Promise<Secret[]>` | 全部（頁は無い）。 |
| `delete(id: string, o?: CallOptions): Promise<void>` |  |

### `SencaClient`

管理 API の client。

| メンバー | 説明 |
|---|---|
| `new SencaClient(options: ClientOptions): SencaClient` | |
| `static etagOf(resource: object): string \| undefined` | 資源の ETag（GET・PATCH の `ETag`、無ければ `"{generation}"`）。 |
| `static newIdempotencyKey(): any` | 新しい Idempotency-Key（同じ作成を確実に 1 回にしたいとき、自分で持って再送に使う）。 |
| `static operations: Readonly<Record<string, { method: string; path: string; scope: string \| null; }>>` | 管理 API の操作の表（operationId → method・path・権限。docs/api/openapi.yaml から生成）。 |
| `streams: Streams` |  |
| `recordings: Recordings` |  |
| `schedules: Schedules` |  |
| `tokens: Tokens` |  |
| `inputs: StreamIo<Input, InputCreate>` |  |
| `outputs: StreamIo<Output, OutputCreate>` |  |
| `secrets: Secrets` |  |
| `pipelines: Pipelines` |  |
| `nodes: Nodes` |  |
| `rooms: Rooms` | リアルタイムの部屋。 |
| `mediaPipelines: MediaPipelines` | 部屋のメディアパイプライン（書き出し・録画・合成・送り出し・文字起こし）。 |
| `audit: Audit` | 監査ログ（読み出しだけ）。 |
| `auth: Auth` | ログインと本人の操作。 |
| `users: Users` | 利用者の管理。 |
| `setToken(token: string \| (() => string \| Promise<string>) \| undefined): void` | 以後の要求の認証の値を替えます（ログインで得たセッションの値 `sencas_...` など）。 |
| `baseUrl: string` | API の base（`.../api/v1`）。 |
| `capabilities(o?: CallOptions): Promise<import("./openapi.gen.js").Capabilities>` | 対応版・コーデック・機能。 |
| `devices(o?: CallOptions): Promise<import("./openapi.gen.js").Devices>` | 全ノードのハードウェア。 |
| `grid(o?: CallOptions): Promise<import("./openapi.gen.js").Grid>` | グリッド（ノード・資源・仕事の配置）。 |
| `events(o?: import("./sse.js").EventsOptions): AsyncGenerator<import("./sse.js").ServerEvent, any, any>` | SSE のイベント。切れたら付け直す。`for await (const ev of client.events({types: ['stream.*']}))`。 |
| `request<T = unknown>(method: string, path: string, o?: RequestOptions): Promise<import("./http.js").HttpResult<T>>` | 型の付いていない要求（新しい操作を SDK より先に使うとき）。`path` は `/api/v1` の後ろ。 |

### `SencaError`

SDK のすべての誤りの基底。

| メンバー | 説明 |
|---|---|
| `new SencaError(code: string, message: string, opts?: { fatal?: boolean \| undefined; closeCode?: number \| undefined; request?: string \| undefined; cause?: unknown; } \| undefined): SencaError` | |
| `code: string` | 誤りの種類（シグナリングの `code`、REST の problem の `code`、SDK の code）。 |
| `fatal: boolean` | 真ならセッション（接続）は続かない。偽ならその要求だけが失敗した。 |
| `closeCode: number \| undefined` | WebSocket の close code（あれば）。 |
| `request: string \| undefined` | 誤りの元になった message の `type`（シグナリング）。 |
| `name: string` |  |
| `message: string` |  |
| `stack?: string` |  |
| `cause?: unknown` |  |

### `Session`

部屋のセッション。`Senca.connect()` が返す。

| メンバー | 説明 |
|---|---|
| `state: "connecting" \| "connected" \| "reconnecting" \| "closed"` | 今の状態。 |
| `connectionId: string` | 自分の connection_id（再接続で変わる）。 |
| `clientId: string \| undefined` |  |
| `bundleId: string` |  |
| `role: Role` |  |
| `metadata: unknown` | 他の参加者に見せる自分の metadata（トークン・認証 Webhook が決めたもの）。 |
| `room: RoomInfo \| null` |  |
| `network: NetworkStatus \| null` | 回線の状態（`network.status` を受けるまで null）。 |
| `closed: Promise<CloseInfo>` | セッションが閉じたら解決する（拒否はしない）。 |
| `participants: ReadonlyMap<string, Participant>` | 他の参加者（connection_id → Participant）。その場で変わる。 |
| `tracks: ReadonlyMap<string, RemoteTrack>` | 今受けているトラック（トラック ID → RemoteTrack）。その場で変わる。 |
| `publications: readonly Publication[]` | 自分の公開。 |
| `spotlight: { enabled: boolean; focused: readonly string[]; }` | スポットライトで注目されている参加者の connection_id（新しく注目した順）。 |
| `events(opts?: { types?: SessionEventType[]; signal?: AbortSignal; }): AsyncIterableIterator<SessionEvent>` | 出来事の列。セッションが閉じると（`closed` を流してから）終わる。 |
| `remoteTracks(opts?: { kind?: Kind; participant?: string \| Participant; signal?: AbortSignal; }): AsyncIterableIterator<RemoteTrack>` | 受けるトラックの列。今受けているものを先に流し、その後に新しく受けたものを流す。 |
| `messages(label?: string \| { label?: string; signal?: AbortSignal; }, opts?: { signal?: AbortSignal; }): AsyncIterableIterator<InboundMessage>` | 届いた message の列（ラベルを省けば全部）。 |
| `waitFor<T extends SessionEventType>(type: T, opts?: { filter?: (e: Extract<SessionEvent, { type: T; }>) => boolean; signal?: AbortSignal; timeoutMs?: number; }): Promise<Extract<SessionEvent, { type: T; }>>` | 最初に来た出来事（`type` が一致し `filter` が真のもの）を待つ。 |
| `publish(source: MediaStream \| MediaStreamTrack \| MediaStreamTrack[], opts?: PublishOptions): Promise<Publication>` | カメラ・マイク・画面などを公開する。交渉が済み、サーバーがトラック ID を付けたら解決する。 |
| `send(label: string, data: string \| ArrayBuffer \| ArrayBufferView \| object, opts?: { to?: string \| Participant \| Array<string \| Participant>; signal?: AbortSignal; }): Promise<void>` | アプリの message を送る。`to` を省けば、そのラベルを開いている他の全員へ（DataChannel、開いていなければシグナリング）。 |
| `stats(): Promise<SessionStats>` | 統計（getStats の要約と、そのままの報告）。 |
| `close(reason?: string): Promise<CloseInfo>` | 閉じる（サーバーへ `disconnect` を送る）。何度呼んでもよい。 |

### `StreamIo`

stream の下の入力・出力（作成・一覧は stream ごと、他は ID で）。

| メンバー | 説明 |
|---|---|
| `create(streamId: string, body: C, o?: CreateOptions): Promise<T>` | stream に作る。 |
| `list(streamId: string, o?: ListOptions): Paginator<T>` | stream の一覧。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `Streams`

| メンバー | 説明 |
|---|---|
| `snapshots: { /** @param {string} id @param {CallOptions} [o] @returns {Promise<import('./openapi.gen.js').SnapshotList['items']>} */ list: (id: string, o?: CallOptions) => Promise<import("./openapi.gen.js").SnapshotList["items"]>; /** @param {string} id @param {string} sid @param {CallOptions} [o] @returns {Promise<Blob>} */ get: (id: string, sid: string, o?: CallOptions) => Promise<Blob>; /** @param {string} id @param {string} sid @param {CallOptions} [o] */ delete: (id: string, sid: string, o?: CallOptions) => Promise<void>; }` | stream の静止画。 |
| `rotateKey(id: string, o?: CreateOptions): Promise<Stream>` | stream key を作り直す（新しい key はこの応答でだけ見える）。 |
| `playbackUrl(id: string, body?: import("./openapi.gen.js").PlaybackRequest, o?: CreateOptions): Promise<import("./openapi.gen.js").LivePlayback>` | 署名付きの再生 URL。 |
| `startRecording(id: string, body?: import("./openapi.gen.js").RecordingStart, o?: CreateOptions): Promise<Recording>` | 録画を始める。 |
| `plan(id: string, o?: CreateOptions): Promise<import("./openapi.gen.js").Plan>` | 計画（入力・出力・変換の見積もりと不適合の理由）。 |
| `snapshot(id: string, o?: CallOptions & { format?: "jpeg" \| "png"; }): Promise<Blob>` | 今の静止画。 |
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `list(o?: ListOptions): Paginator<T>` | 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

### `Tokens`

API トークン（更新は無い）。

| メンバー | 説明 |
|---|---|
| `create(body: TokenCreate, o?: CreateOptions): Promise<import("./openapi.gen.js").Token>` | 作る（`token` の値はこの応答でだけ見える）。 |
| `list(o?: ListOptions): Paginator<import("./openapi.gen.js").Token>` |  |
| `listPage(o?: ListOptions): Promise<{ items: import("./openapi.gen.js").Token[]; next_cursor: string \| null; }>` |  |
| `get(id: string, o?: CallOptions): Promise<import("./openapi.gen.js").Token>` |  |
| `delete(id: string, o?: CallOptions): Promise<void>` | 失効させる。 |

### `Users`

利用者の管理（`/users`、`tenant:admin`）。

| メンバー | 説明 |
|---|---|
| `resetPassword(id: string, body?: import("./openapi.gen.js").UserPasswordReset, o?: CreateOptions): Promise<import("./openapi.gen.js").UserPasswordResetResult>` | パスワードを再設定します（`password` を省くと仮のパスワードが `temporary_password` で返ります）。 |
| `resetMfa(id: string, o?: CallOptions): Promise<void>` | MFA を外します。 |
| `sessions(id: string, o?: ListOptions): Paginator<Session>` | 利用者のセッションの一覧。 |
| `revokeSessions(id: string, o?: CallOptions): Promise<void>` | 利用者のセッションをすべて失効させます。 |
| `create(body: C, o?: CreateOptions): Promise<T>` | 作る（POST、Idempotency-Key を自動で付ける）。 |
| `list(o?: ListOptions): Paginator<T>` | 一覧（cursor を自動でたどる）。`for await (const x of client.streams.list())`。 |
| `listPage(o?: ListOptions & { query?: Record<string, string \| undefined>; }): Promise<{ items: T[]; next_cursor: string \| null; }>` | 一覧の 1 頁。 |
| `get(id: string, o?: CallOptions): Promise<T>` | 取得（ETag は `SencaClient.etagOf(x)` で読める）。 |
| `update(id: string, patch: P, o: CallOptions & { ifMatch: IfMatch; }): Promise<T>` | 更新（PATCH、If-Match 必須）。`ifMatch` は ETag・generation・資源のどれか。古ければ ApiError（`PRECONDITION_FAILED`、412）。 |
| `modify(id: string, change: (current: T) => P \| Promise<P>, o?: CallOptions & { attempts?: number; }): Promise<T>` | 読んで、変えて、書く。412（他の人が先に書いた）なら読み直してやり直す（既定 3 回）。 |
| `delete(id: string, o?: CallOptions & { ifMatch?: IfMatch; }): Promise<void>` | 消す。`ifMatch` を付ければ版を確かめる（資源によっては見ない）。 |

## 型

管理 API の資源の型（`Stream` など）は docs/api/openapi.yaml から生成した `types/rest/openapi.gen.d.ts`。

### `CallOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `signal?` | `AbortSignal \| undefined` |  |

### `ClientOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `baseUrl` | `string` | `https://node.example`（`/api/v1` は足す）か `https://node.example/api/v1` |
| `token?` | `string \| (() => string \| Promise<string>) \| undefined` | API トークン（`senca_...`）・ログインのセッション（`sencas_...`）・JWT。関数なら要求のたびに呼ぶ。 ログインだけをするなら省いてよい（ログインの操作は認証なしで使えます） |
| `fetch?` | `((input: RequestInfo \| URL, init?: RequestInit \| undefined) => Promise<Response>) \| undefined` | 既定は globalThis.fetch |
| `retry?` | `boolean \| RetryOptions \| undefined` | 既定 true（3 回まで） |
| `timeoutMs?` | `number \| undefined` | 1 回の要求の時間切れ（既定 30000、SSE には使わない） |
| `headers?` | `Record<string, string> \| undefined` | すべての要求に付ける見出し |
| `idempotency?` | `boolean \| undefined` | POST に自動で Idempotency-Key を付ける（既定 true。再試行を安全にする） |

### `CloseInfo`

| 項目 | 型 | 説明 |
|---|---|---|
| `reason` | `string` | `normal`・`kicked`・`room_deleted`・`error`・`connection_failed`・`aborted` など |
| `error` | `SencaError \| undefined` | 誤りで閉じたとき |

### `ConnectOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `url` | `string \| string[]` | シグナリングの URL（`wss://node.example/signaling`。`https://node.example` なら `/signaling` を足す）。 複数なら再接続のたびに順に替える |
| `room` | `string` | 部屋の ID |
| `token?` | `string \| ((ctx: { room: string; attempt: number; }) => string \| Promise<string>) \| undefined` | 参加トークン。関数なら参加・再接続のたびに呼ぶ（期限の短いトークンを取り直す） |
| `role?` | `Role \| undefined` | 既定 `sendrecv` |
| `clientId?` | `string \| undefined` |  |
| `bundleId?` | `string \| undefined` | 同じ値の接続どうしは互いのトラックを受けない |
| `metadata?` | `unknown` | 認証 Webhook へだけ渡す（他の参加者に見せる metadata はトークン・Webhook が決める） |
| `multistream?` | `boolean \| undefined` | 既定 true（false で種類ごとに 1 本だけ受ける） |
| `spotlight?` | `{ number: number; focusLayer?: string \| undefined; unfocused?: "lowest" \| "off" \| undefined; } \| undefined` | 受け手としてのスポットライト |
| `receiveLayer?` | `string \| undefined` | 受ける映像の最初の層（`'r0'` など。サイマルキャストのトラックを受けたら自動で選ぶ） |
| `publish?` | `MediaStream \| MediaStreamTrack[] \| (PublishOptions & { stream: MediaStream \| MediaStreamTrack[]; }) \| undefined` | 参加と同時に公開する（最初の offer に入れるので再交渉が 1 回少ない） |
| `video?` | `false \| { codec?: VideoCodec \| undefined; bitrateKbps?: number \| undefined; } \| undefined` | 映像の希望（`false` で送らない） |
| `audio?` | `false \| { codec?: "opus" \| undefined; bitrateKbps?: number \| undefined; } \| undefined` |  |
| `dataChannels?` | `DataChannelOption[] \| undefined` | 使う DataChannel（`#` で始まるラベル） |
| `signaling?` | `{ dataChannel?: boolean \| undefined; closeWebSocket?: boolean \| undefined; } \| undefined` | シグナリングを DataChannel へ切り替える（§6） |
| `e2ee?` | `boolean \| undefined` | E2EE の部屋に入る（部屋の設定と同じでなければ `e2ee_mismatch`。暗号化（SFrame）は利用者が Encoded Transform で行う） |
| `forwardingFilter?` | `ForwardingFilter \| undefined` | この接続の購読を絞る |
| `iceServers?` | `RTCIceServer[] \| undefined` |  |
| `rtcConfiguration?` | `RTCConfiguration \| undefined` | RTCPeerConnection の設定（bundlePolicy は max-bundle に固定） |
| `reconnect?` | `boolean \| ReconnectOptions \| undefined` | 既定 true |
| `connectTimeoutMs?` | `number \| undefined` | 1 回の参加の時間切れ（既定 20000。ピア接続がつながるまで） |
| `reportStats?` | `boolean \| { intervalMs?: number \| undefined; } \| undefined` | getStats の要約を pong でサーバーへ送る（REST の統計に出る） |
| `signal?` | `AbortSignal \| undefined` | 参加中に取り消すと AbortError。参加の後に取り消すとセッションを閉じる |
| `environment?` | `Environment \| undefined` |  |
| `logger?` | `{ debug?: ((...a: unknown[]) => void) \| undefined; warn?: ((...a: unknown[]) => void) \| undefined; } \| undefined` |  |

### `CreateOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `signal?` | `AbortSignal \| undefined` |  |
| `idempotencyKey?` | `string \| undefined` |  |

### `DataChannelOption`

```ts
type DataChannelOption = string | { label: string; direction?: "sendrecv" | "sendonly" | "recvonly" | undefined; ordered?: boolean | undefined; maxRetransmits?: number | undefined; maxPacketLifeTime?: number | undefined; }
```

### `Environment`

| 項目 | 型 | 説明 |
|---|---|---|
| `WebSocket?` | `{ new (url: string \| URL, protocols?: string \| string[] \| undefined): WebSocket; prototype: WebSocket; readonly CONNECTING: 0; readonly OPEN: 1; readonly CLOSING: 2; readonly CLOSED: 3; } \| undefined` |  |
| `RTCPeerConnection?` | `{ new (configuration?: RTCConfiguration \| undefined): RTCPeerConnection; prototype: RTCPeerConnection; generateCertificate(keygenAlgorithm: AlgorithmIdentifier): Promise<RTCCertificate>; } \| undefined` |  |
| `MediaStream?` | `{ new (): MediaStream; new (stream: MediaStream): MediaStream; new (tracks: MediaStreamTrack[]): MediaStream; prototype: MediaStream; } \| undefined` |  |
| `RTCRtpSender?` | `{ getCapabilities(kind: string): RTCRtpCapabilities \| null; } \| undefined` |  |
| `RTCRtpReceiver?` | `{ getCapabilities(kind: string): RTCRtpCapabilities \| null; } \| undefined` |  |

### `EventsOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `types?` | `string[] \| undefined` | この種類だけ（`stream.created` など。`stream.*` の形で前方一致） |
| `lastEventId?` | `string \| undefined` | ここから（`Last-Event-ID`） |
| `reconnect?` | `boolean \| BackoffPolicy \| undefined` | 切れたら付け直す（既定 true） |
| `signal?` | `AbortSignal \| undefined` | 取り消すと反復を終える |

### `ForwardingFilter`

| 項目 | 型 | 説明 |
|---|---|---|
| `action` | `"block" \| "allow"` |  |
| `rules` | `{ field: string; operator: "is_in" \| "is_not_in"; values: string[]; }[][]` |  |

### `IfMatch`

```ts
type IfMatch = string | number | { generation: number; } | { etag: string; }
```

### `InboundStats`

| 項目 | 型 | 説明 |
|---|---|---|
| `trackId` | `string` | SENCA のトラック ID |
| `participantId` | `string \| undefined` | 送り手の connection_id |
| `clientId` | `string \| undefined` |  |
| `kind` | `"audio" \| "video"` |  |
| `mid` | `string` |  |
| `layer` | `string \| null` | サーバーが今配っている層（`layer.changed`） |
| `bytesReceived` | `number` |  |
| `packetsReceived` | `number` |  |
| `packetsLost` | `number` |  |
| `jitterMs` | `number \| undefined` |  |
| `framesDecoded` | `number \| undefined` |  |
| `framesDropped` | `number \| undefined` |  |
| `framesPerSecond` | `number \| undefined` |  |
| `frameWidth` | `number \| undefined` |  |
| `frameHeight` | `number \| undefined` |  |
| `audioLevel` | `number \| undefined` |  |
| `bitrateBps` | `number \| undefined` | 前回の stats() からの受信の速さ |

### `Kind`

```ts
type Kind = "audio" | "video"
```

### `ListOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `limit?` | `number \| undefined` |  |
| `cursor?` | `string \| undefined` |  |
| `signal?` | `AbortSignal \| undefined` |  |

### `NetworkStatus`

| 項目 | 型 | 説明 |
|---|---|---|
| `level` | `number` |  |
| `bandwidthBps` | `number` |  |
| `rttMs` | `number` |  |

### `OutboundStats`

| 項目 | 型 | 説明 |
|---|---|---|
| `trackId` | `string \| undefined` | SENCA のトラック ID（answer の `published`） |
| `kind` | `"audio" \| "video"` |  |
| `rid` | `string \| undefined` |  |
| `mid` | `string` |  |
| `bytesSent` | `number` |  |
| `packetsSent` | `number` |  |
| `framesSent` | `number \| undefined` |  |
| `framesPerSecond` | `number \| undefined` |  |
| `frameWidth` | `number \| undefined` |  |
| `frameHeight` | `number \| undefined` |  |
| `targetBitrateBps` | `number \| undefined` |  |
| `qualityLimitationReason` | `string \| undefined` |  |
| `active` | `boolean \| undefined` |  |
| `bitrateBps` | `number \| undefined` | 前回の stats() からの送信の速さ |

### `PublishedTrackInfo`

| 項目 | 型 | 説明 |
|---|---|---|
| `id` | `string` | トラック ID |
| `kind` | `Kind` |  |
| `rids` | `readonly string[]` | サイマルキャストの層（低い順、無ければ空） |

### `PublishOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `simulcast?` | `boolean \| SimulcastLayer[] \| { layers?: SimulcastLayer[] \| undefined; } \| undefined` | サイマルキャスト（`true` で既定の 3 層 r0・r1・r2） |
| `maxBitrateKbps?` | `number \| undefined` | 映像の上限（サイマルキャストなら一番上の層）。部屋の上限があればそれ以下 |
| `audioBitrateKbps?` | `number \| undefined` | 音声の上限 |
| `codec?` | `VideoCodec \| undefined` | 部屋のコーデックと違えば `unsupported_codec`（部屋が決める。確かめるために書ける） |
| `signal?` | `AbortSignal \| undefined` |  |

### `ReconnectOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `maxAttempts?` | `number \| undefined` | あきらめるまでの回数（既定 10） |
| `initialDelayMs?` | `number \| undefined` | 最初の待ち（既定 500） |
| `maxDelayMs?` | `number \| undefined` | 待ちの上限（既定 10000） |
| `factor?` | `number \| undefined` | 倍率（既定 2） |
| `jitter?` | `number \| undefined` | ゆらぎ（既定 0.2） |

### `RemoteTrackEvent`

```ts
type RemoteTrackEvent =
  | { type: "layer"; rid: string | null; }
  | { type: "paused"; }
  | { type: "resumed"; }
  | { type: "ended"; reason: string; }
```

### `RequestOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `query?` | `Record<string, string \| number \| boolean \| null \| undefined> \| undefined` |  |
| `body?` | `unknown` | JSON にして送る |
| `headers?` | `Record<string, string> \| undefined` |  |
| `ifMatch?` | `IfMatch \| undefined` |  |
| `idempotencyKey?` | `string \| false \| undefined` | POST の Idempotency-Key（既定は自動で作る。`false` で付けない） |
| `signal?` | `AbortSignal \| undefined` |  |
| `as?` | `"blob" \| "json" \| "response" \| undefined` | 応答の読み方（既定 json） |
| `retry?` | `boolean \| RetryOptions \| undefined` |  |
| `timeoutMs?` | `number \| undefined` |  |
| `credentials?` | `RequestCredentials \| undefined` | fetch の credentials（既定 `omit`。クッキーを送る `POST /auth/exchange` だけ `same-origin`） |

### `RetryOptions`

| 項目 | 型 | 説明 |
|---|---|---|
| `maxAttempts?` | `number \| undefined` | 最初の 1 回を含む回数（既定 3） |
| `initialDelayMs?` | `number \| undefined` | 既定 200 |
| `maxDelayMs?` | `number \| undefined` | 既定 5000 |
| `factor?` | `number \| undefined` | 既定 2 |
| `jitter?` | `number \| undefined` | 既定 0.2 |

### `Role`

```ts
type Role = "sendrecv" | "sendonly" | "recvonly"
```

### `RoomInfo`

| 項目 | 型 | 説明 |
|---|---|---|
| `id` | `string` |  |
| `videoCodec` | `VideoCodec` |  |
| `audioCodec` | `"opus"` |  |
| `e2ee` | `boolean` |  |
| `multistream` | `boolean` |  |
| `maxVideoBitrateKbps` | `number \| undefined` |  |
| `spotlight` | `{ number: number; focusLayer?: string \| undefined; unfocused?: "lowest" \| "off" \| undefined; } \| undefined` |  |

### `SencaErrorCode`

```ts
type SencaErrorCode = string
```

### `ServerEvent`

```ts
type ServerEvent = { id: string | null; type: "stream.created" | "stream.updated" | "stream.deleted" | "stream.owner" | "recording.updated" | "recording.deleted" | "schedule.created" | "schedule.updated" | "schedule.deleted" | "schedule.run" | "node.updated" | "input.updated" | "input.deleted" | "output.updated" | "output.deleted" | "secret.created" | "secret.deleted" | "pipeline.updated" | "pipeline.deleted" | "room.updated" | "room.deleted" | "media_pipeline.updated" | "media_pipeline.deleted" | "user.created" | "user.updated" | "user.deleted" | "node.down" | "node.up" | "device.down" | "device.up" | "device.disabled" | "device.enabled" | "grid.decisions" | "room.started" | "room.stopped" | "room.connection.created" | "room.connection.updated" | "room.connection.destroyed" | "room.track.published" | "room.track.unpublished" | "room.spotlight.changed" | (string & {}); data: SseEventData; } | { id: string | null; type: "resync"; data: Record<string, never>; }
```

### `SessionEvent`

```ts
type SessionEvent =
  | { type: "participant.joined"; participant: Participant; }
  | { type: "participant.left"; participant: Participant; reason: string; }
  | { type: "participant.updated"; participant: Participant; }
  | { type: "track.published"; participant: Participant | undefined; track: PublishedTrackInfo; }
  | { type: "track.unpublished"; participant: Participant | undefined; trackId: string; }
  | { type: "track.subscribed"; track: RemoteTrack; }
  | { type: "track.unsubscribed"; track: RemoteTrack; reason: string; }
  | { type: "track.paused"; track: RemoteTrack; }
  | { type: "track.resumed"; track: RemoteTrack; }
  | { type: "layer.changed"; track: RemoteTrack; rid: string | null; }
  | { type: "spotlight.changed"; focused: readonly string[]; }
  | { type: "network"; status: NetworkStatus; }
  | { type: "push"; data: unknown; }
  | { type: "signaling.switched"; }
  | { type: "ice.restarting"; }
  | { type: "ice.restarted"; }
  | { type: "reconnecting"; attempt: number; delayMs: number; error: SencaError | undefined; }
  | { type: "reconnected"; connectionId: string; }
  | { type: "redirected"; location: string; }
  | { type: "error"; error: SencaError; }
  | { type: "closed"; reason: string; error: SencaError | undefined; }
```

### `SessionEventType`

```ts
type SessionEventType = "closed" | "reconnecting" | "participant.joined" | "participant.left" | "participant.updated" | "track.published" | "track.unpublished" | "track.subscribed" | "track.unsubscribed" | "track.paused" | "track.resumed" | "layer.changed" | "spotlight.changed" | "network" | "push" | "signaling.switched" | "ice.restarting" | "ice.restarted" | "reconnected" | "redirected" | "error"
```

### `SessionStats`

| 項目 | 型 | 説明 |
|---|---|---|
| `at` | `number` | 測った時刻（UNIX ミリ秒） |
| `transport` | `{ state: string; rttMs: number \| undefined; availableOutgoingBitrateBps: number \| undefined; availableIncomingBitrateBps: number \| undefined; bytesSent: number; bytesReceived: number; }` |  |
| `outbound` | `OutboundStats[]` |  |
| `inbound` | `InboundStats[]` |  |
| `dataChannels` | `{ label: string; state: string; messagesSent: number; messagesReceived: number; bytesSent: number; bytesReceived: number; }[]` |  |
| `raw` | `RTCStatsReport` | ブラウザーの getStats() そのもの |

### `SimulcastLayer`

| 項目 | 型 | 説明 |
|---|---|---|
| `rid` | `string` | 層の名前（英数字 16 文字まで） |
| `scaleResolutionDownBy?` | `number \| undefined` | 解像度を何分の 1 にするか（既定: 低い層から 4・2・1） |
| `maxBitrateKbps?` | `number \| undefined` | この層の上限（kbit/s） |
| `maxFramerate?` | `number \| undefined` |  |

### `VideoCodec`

```ts
type VideoCodec = "vp8" | "vp9" | "h264" | "av1"
```
