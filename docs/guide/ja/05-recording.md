# 5. 録画

[English](../en/05-recording.md)

このページでは、ストリームの録画、VOD としての再生、MP4・HLS としての書き出しについて説明します。あわせて、予約（スケジュール）を使って、
決まった時刻に録画・配信の受け付け・静止画の保存を行う方法も説明します。

- 設計の背景: [ADR-0005 録画の形式とインデックス](../../dev/adr/0005-recording.md)、[ADR-0006 ストリームのタスク](../../dev/adr/0006-stream-hub.md)
- 関連するページ: [4. 取り込みと再生](04-ingest-playback.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[13. 運用](13-operations.md)、[14. コーデックと特許](14-codecs-and-patents.md)、[15. API と SDK](15-api-and-sdks.md)

例で使う `API`・`TOKEN`・`H` は、[4. 取り込みと再生](04-ingest-playback.md) と同じです。

## 5.1 仕組み

- 録画には、**ライブの HLS と同じセグメント**をそのまま使います（録画用に別途多重化はしません）。コンテナ形式は、ストリームの
  `hls.container`（TS または fMP4）です。
- 録画中のストリームがあるときだけ、セグメントを `{data-dir}/segments/{stream}/{run}/` にファイルとして書き込みます。書き込んだファイルは
  変更しません（一時ファイル名で書き込み → fsync → rename の順で保存します）。
- 録画ごとに、追記のみのインデックス `{data-dir}/recordings/{id}/index.jsonl` を持ちます。各行は `begin`・`init`・`segment`・`gap`・`end`
  のいずれかです。セグメントを書き込んでからインデックスに追記するため、インデックスに記載されたセグメントは常に完全です。
- 同じ時間帯に複数の録画がある場合は、同じセグメントのファイルを共有します（ディスク容量は 1 本分で済みます）。
- 録画の区切りは**セグメント単位**です。録画は「作成中のセグメント」から始まり、「停止した時刻を含むセグメント」までが記録されます。
- 録画の書き込みは、ライブ配信より優先度が低くなっています。書き込みのキューがあふれた場合はセグメントを破棄し、`gap`（`queue_overflow`）
  として記録します。ライブ配信は止めません。キューの長さは `--record-queue` で設定します（既定 4096。ストリームごとにはその 1/64 で、16〜4096）。
- クラスターでは、録画のファイルは録画を作成したノードのディスクにだけ保存されます（複製はしません。[9. クラスタとグリッド](09-cluster-grid.md)）。

## 5.2 録画の状態とフィールド

| `state` | 説明 |
|---|---|
| `recording` | 録画中 |
| `finalized` | 正常に確定済み |
| `interrupted` | 途中で中断（ノードの停止や書き込みの失敗など。中断までの部分は再生・書き出しできます） |

削除処理中（内部状態 `deleting`）の録画は、一覧や取得の結果には表示されません。

| フィールド | 説明 |
|---|---|
| `id`・`stream`・`name`・`generation` | |
| `state` | 上の表を参照 |
| `started_at`・`ended_at` | |
| `retention_until` | 保持期限（`null` は無期限） |
| `legal_hold` | 法的保全（legal hold）。true の間は、削除も期限切れによる自動削除も行いません |
| `schedule` | 予約によって作成された録画の場合、その予約の ID |
| `duration_ms`・`segments`・`bytes` | インデックスから集計した値 |
| `gaps` | 欠落の一覧 `[{at, reason, dropped}]`。`reason` は `publisher_reconnect`・`queue_overflow` のいずれか |
| `thumbnail` | サムネイルがあるかどうか |

注: 現在、ノードの起動後に作成された録画の `gaps` は、ノードを再起動するまで空の一覧として表示されます（欠落はインデックスには記録されています）。

## 5.3 自動録画（配信開始で録画を始める）

ストリームの `recording.enabled` を `true` にすると、配信者が接続したときに録画を開始します。1 回の配信が 1 本の録画になります。

- 再接続の猶予時間（`--reconnect-grace`、既定 30 秒）内に再接続した場合は、**同じ録画が続きます**（§5.9）。
- 猶予時間が過ぎて配信が終了すると、録画は `finalized` になります。次に配信者が接続すると、新しい録画を作成します。
- 保持日数は `recording.retention_days` で指定します（`retention_until` = 開始時刻 + 日数）。
- `desired_state: stopped` のストリームでは、録画を開始しません。

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"lecture","recording":{"enabled":true,"retention_days":30}}'
```

## 5.4 手動で開始・停止する

```sh
# 開始する（本文は省略可能）
curl -s -X POST "$API/streams/$SID/recordings" "${H[@]}" -d '{"name":"manual-1","retention_days":30}'
# 停止する
curl -s -X POST "$API/recordings/$RID/stop" "${H[@]}"
```

| 操作 | リクエスト | 権限 | 結果 |
|---|---|---|---|
| 開始 | `POST /streams/{id}/recordings` `{name?, retention_days?}` | `stream:write` | 202 と録画（`state: recording`）、`Location` |
| 停止 | `POST /recordings/{id}/stop` | `stream:write` | 202。録画中でない場合は 409 |

- `retention_days` は 1〜36500 です。
- 配信者が接続していなくても開始できます。セグメントは、配信者が接続してから記録されます。
- ストリームが `stopped` の場合は 409 になります。同じストリームで同時に行える録画（自動・手動・予約の合計）は 8 本までです
  （超えると 403 `QUOTA_EXCEEDED`）。
- 停止は非同期で行われるため、応答の時点では `state` がまだ `recording` の場合があります。確定したかどうかは `GET /recordings/{id}` で確認してください。
- 配信が終了する（猶予時間が過ぎる）と、手動で開始した録画も一緒に確定します。

## 5.5 一覧・取得・変更・削除

| 操作 | リクエスト | 権限 |
|---|---|---|
| 一覧 | `GET /recordings?stream={id}&state={recording\|finalized\|interrupted}&limit&cursor` | `recording:read` |
| 取得 | `GET /recordings/{id}` | `recording:read` |
| 変更 | `PATCH /recordings/{id}` `{retention_until?, legal_hold?, name?}`（`If-Match` が必須） | `stream:write` |
| 削除 | `DELETE /recordings/{id}` | `recording:delete` |

- `retention_until` は RFC 3339 の日時か `null`（無期限）です。`name` は 128 文字までです。
- `legal_hold` を解除する（`true` → `false`）には、`recording:delete` の権限も必要です。
- 削除すると 202 `{"id": "...", "state": "deleting"}` が返ります。録画中の場合は、停止してから削除します。法的保全中の場合は 409 になります。
  他の録画が参照しているセグメントは削除しません（どのインデックスからも参照されなくなったものだけを削除します）。

```sh
ETAG=$(curl -s -D - -o /dev/null "$API/recordings/$RID" -H "Authorization: Bearer $TOKEN" | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH "$API/recordings/$RID" "${H[@]}" -H "If-Match: $ETAG" -d '{"legal_hold":true,"retention_until":null}'
```

## 5.6 VOD の再生（HLS）

```sh
curl -s -X POST "$API/recordings/$RID/playback-url" "${H[@]}" -d '{"ttl_secs":3600}'
```

```json
{"expires_at":"2026-11-03T02:23:37Z",
 "playback":{"hls":"http://127.0.0.1:24080/s/1793669017/hMRgjzJPEFVV0OgLFjdkww/vod/b8bc7a7d6b9ff6df2639d7c07d22177f/index.m3u8","mp4":null}}
```

- 必要な権限は `recording:read` です。`ttl_secs` の規則はライブと同じです（既定 300、上限は `--max-playback-ttl`）。`mp4` は現在常に `null` です
  （MP4 での取得は §5.7 を参照）。
- プレイリストには `#EXT-X-PLAYLIST-TYPE:VOD` と `#EXT-X-ENDLIST` が付きます。録画中は `EVENT`（`ENDLIST` なし）となり、プレイリストが伸びていきます。
- セグメントの名前は `{epoch}-{sequence}.ts` です（fMP4 では `.m4s` と `init.mp4`）。`Range`（1 区間）に対応し、`Cache-Control: max-age=3600` が付きます。
- 再接続した箇所には `#EXT-X-DISCONTINUITY` が入ります（§5.9）。

実行例（猶予時間内に 1 回再接続した録画）:

```
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:0
#EXT-X-PLAYLIST-TYPE:VOD
#EXT-X-INDEPENDENT-SEGMENTS
#EXTINF:2.000,
0-0.ts
#EXTINF:2.000,
0-1.ts
#EXTINF:2.000,
1-2.ts
#EXT-X-DISCONTINUITY
#EXTINF:2.000,
1-3.ts
…
#EXT-X-ENDLIST
```

**HLS として書き出す**: VOD のプレイリストとセグメントは、そのまま HLS の書き出しとして使えます。1 つのファイルにまとめる場合は、
たとえば `ffmpeg -i "$VOD_URL" -c copy out.ts` のように、再エンコードせずに取り出してください。クラスターでは、録画を持たないノードに
VOD のリクエストが届くと、録画を作成したノードへ 307 でリダイレクトします。

## 5.7 MP4 の書き出し

```sh
curl -s -o rec.mp4 "$API/recordings/$RID/export.mp4" -H "Authorization: Bearer $TOKEN"
```

- 必要な権限は `recording:read` です。応答には `Content-Type: video/mp4`、`Content-Disposition: attachment; filename="{id}.mp4"` が付きます。
- 再エンコードは行いません。セグメントを分解し、先頭に `moov` を置いた MP4 に組み立て直します。
- 録画中の場合は 409、空の録画も 409 になります。メモリ上で組み立てるため、セグメントの合計が 4 GiB を超える録画は 413 `PAYLOAD_TOO_LARGE` になります。
- 予約による録画は、予約の開始・終了の時刻（インデックスの `not_before_ms`・`not_after_ms`）で切り出します。開始位置は直前のキーフレームまで広げます。
  pre-roll・post-roll の部分は VOD には残りますが、MP4 には含まれません。
- 再接続によって時刻が飛んだ区間は、前の区間の終わりに続けてつなぎます（空白の時間は含まれません）。
- クラスターでは、録画を作成したノードの API にリクエストしてください（書き出しとサムネイルは、そのノードのローカルのファイルを読み込みます）。

## 5.8 サムネイル

録画が確定した後、セグメントから等間隔に最大 12 枚のキーフレームをデコードし、黒・白・単調な画面・ぶれた画面を避けて 1 枚を選びます。

```sh
curl -s -o thumb.jpg "$API/recordings/$RID/thumbnail" -H "Authorization: Bearer $TOKEN"
```

- サムネイルがない場合は 404 になります（録画の `thumbnail` が `false`）。
- デコードできるデバイスが必要です。H.264/H.265 の録画では、GPU のデコーダーがないとサムネイルは作成されません（[14. コーデックと特許](14-codecs-and-patents.md)）。

## 5.9 再接続と不連続

配信者の接続が切れ、猶予時間内に（同じキーで。プロトコルは異なってもかまいません）再接続した場合は、次のように処理します。

1. ストリームの `epoch` が 1 増えます。
2. 録画のインデックスに `gap`（`reason: publisher_reconnect`）が追加され、続くセグメントに不連続の印が付きます。
3. VOD のプレイリストには `#EXT-X-DISCONTINUITY` が入ります。録画は 1 本のまま続きます。
4. MP4 の書き出しでは、区間の間を詰めてつなぎます。

猶予時間が過ぎてから再接続した場合は別の配信として扱われ、自動録画であれば新しい録画が始まります。

## 5.10 中断と復旧

録画の途中でノードが停止した場合（強制終了・電源断など）は、次の起動時に復旧処理を行います。

- `end` のないインデックスは、書き込み途中の末尾の行を切り詰め、`end`（`interrupted`）を追加して閉じます。録画は `interrupted` になります。
  終了時刻は最後のセグメントの終わりです。それまでのセグメントは、再生・書き出しができます。
- 制御状態で `recording` のままになっていて、インデックスがない録画も `interrupted` にします。
- どのインデックスからも参照されていないセグメントは、`segments/{stream}/orphans/{run}/` に移動します（削除はしません）。
- 読み込めないインデックスがある場合、その録画には手を加えず、セグメントの隔離も行いません。
- `--verify-recordings` を指定すると、起動時に各セグメントの SHA-256 をインデックスと照合します（一致しない場合はログに出力します）。
- 稼働中にディスクへの書き込みに失敗した録画も、`interrupted` になります。

実行例（録画中に `kill -9` で停止し、再起動した場合）:

```
recovery: recording b0316a943b097502b1c041ea3810a5a5 closed as interrupted (3 segments)
```

## 5.11 保持期限

`--retention-sweep`（既定 60 秒）ごとに、`retention_until` を過ぎた `finalized`・`interrupted` の録画を削除します。
`legal_hold: true` の録画は削除しません。保持期限は、自動録画では `recording.retention_days`、手動の録画では開始時の `retention_days`、
予約による録画では予約の `retention_days` から決まります。後から `PATCH` で変更することもできます。

## 5.12 予約（スケジュール）

予約は、ストリームに対して時間帯を指定し、その時間帯の間だけ動作させる機能です。動作は `action` で指定します。

| `action` | 時間帯の間に行うこと |
|---|---|
| `record` | 録画します（録画の `schedule` に予約の ID、`name` に予約の名前が入ります） |
| `live` | 配信を受け付けます（時間帯の外では拒否します） |
| `snapshot` | `snapshot_interval_secs` ごとに静止画を保存します（`reason: schedule`） |

```sh
curl -s -X POST "$API/schedules" "${H[@]}" -d '{
  "stream": "'"$SID"'",
  "name": "monday-wednesday-lecture",
  "action": "record",
  "start_at": "2026-11-04T00:50:00Z",
  "end_at": "2026-11-04T02:30:00Z",
  "pre_roll_ms": 60000,
  "post_roll_ms": 120000,
  "recurrence": {"freq": "weekly", "weekdays": [1, 3], "timezone": "Asia/Tokyo", "count": 30},
  "retention_days": 365
}'
```

| フィールド | 既定 | 制約 | 説明 |
|---|---|---|---|
| `stream` | （必須） | テナント内のストリーム | |
| `name` | なし | | 録画の名前にも使われます |
| `action` | （必須） | `record`・`live`・`snapshot` | |
| `start_at`・`end_at` | （必須） | RFC 3339。`end_at` > `start_at`、長さは 24 時間まで、`end_at` が過去でないこと | 最初の回の時間帯 |
| `pre_roll_ms`・`post_roll_ms` | 0 | 0〜300000 | 時間帯の前後に広げる長さ |
| `grace_ms` | 60000 | 0〜3600000 | 開始時刻（`start_at - pre_roll_ms`）を過ぎていても、この時間内であれば途中から実行します。過ぎた場合は `missed` になります |
| `snapshot_interval_secs` | なし | `snapshot` の場合のみ必須、1〜3600 | |
| `recurrence` | なし | 下の表を参照 | 繰り返しの設定 |
| `retention_days` | なし | | `record` で作成した録画の保持日数 |

`recurrence` のフィールドは次のとおりです。

| フィールド | 説明 |
|---|---|
| `freq` | `daily` または `weekly` |
| `weekdays` | `weekly` の曜日（0 = 日曜 〜 6 = 土曜）。指定するとその曜日だけ実行します（`start_at` の日も、曜日が合わなければ実行しません）。空の場合は `start_at` の曜日から 7 日ごとに実行します |
| `timezone` | IANA のタイムゾーン名（例: `Asia/Tokyo`）。`--zoneinfo`（既定 `/usr/share/zoneinfo`）から読み込みます。`UTC` は常に使えます。見つからない場合は 400 |
| `until_ms` | 繰り返しの終了時刻（UTC のミリ秒。その時刻ちょうどに始まる回は含みません） |
| `count` | 実行回数の上限（最大 1000） |

- 2 回目以降も、最初の回の**現地時刻**（指定したタイムゾーンでの時刻）を保ちます。夏時間の切り替えをまたいでも、同じ時刻に始まります。
  存在しない時刻（夏時間の開始時）は後ろにずれ、2 回ある時刻（夏時間の終了時）は早い方を使います。
- 同じストリームの `record` の予約は、pre-roll・post-roll を含めて同時に 4 本までです（超えると 409 になり、`details.overlaps` に重なる予約が示されます）。
- 1 つのテナントで作成できる予約は 1000 件までです。

### 確認・参照・変更

| 操作 | リクエスト | 権限 |
|---|---|---|
| 作成 | `POST /schedules` | `stream:write` |
| 作成前の確認 | `POST /schedules/check`（本文は作成と同じ） | `stream:read` |
| 一覧 | `GET /schedules?stream=&state=&limit&cursor` | `stream:read` |
| 取得 | `GET /schedules/{id}`（`last_run` に直近の回） | `stream:read` |
| 実行履歴 | `GET /schedules/{id}/runs`（直近 100 回） | `stream:read` |
| 変更 | `PATCH /schedules/{id}`（`If-Match` が必須） | `stream:write` |
| 削除 | `DELETE /schedules/{id}` | `stream:write` |

`check` は `{"ok": true, "overlaps": [...], "shared_input": true|false}` を返します（時間帯が重なる録画の予約は、同じ入力・同じセグメントを共有します）。

`PATCH` で変更できるのは、`name`・`start_at`・`end_at`・`pre_roll_ms`・`post_roll_ms`・`grace_ms`・`snapshot_interval_secs`・
`retention_days` と、`state: "canceled"`（取り消し。それ以外の値は 400）です。実行中の予約の `start_at` は変更できません（409）。
`action`・`stream`・`recurrence` は変更できないため、変更したい場合は予約を作り直してください。

### 予約と回の状態

予約の `state` は、`pending`（次の回を待機中）・`running`・`completed`・`missed`・`canceled`・`failed` のいずれかです。
各回（run）の `state` も同じ値を取り、`reason`（`missed`・`failed` の理由）と `recording`（作成した録画の ID）を持ちます。
繰り返しの予約は、回が終わる（`completed`・`missed`・`failed`）と `pending` に戻り、すべての回が終わると `completed` になります。

### 実行の規則

- 予約の処理は 250 ms ごとに（予約が変更されたときはすぐに）実行されます。状態を保存してから、録画の開始などの処理を行います。
- `record`: 時間帯の開始（`start_at - pre_roll_ms`）で録画を始め、`end_at + post_roll_ms` で停止します。インデックスには `start_at`・`end_at` が
  `not_before_ms`・`not_after_ms` として記録され、MP4 の書き出しではこの範囲で切り出します（§5.7）。配信者がいなくても録画は始まり、
  配信者が接続してからのセグメントが記録されます。
- 回の開始時にストリームが存在しない場合、または録画を開始できない場合（`stopped`・同時録画数の上限）は、その回は `failed` になります。
- ノードが停止している間に時間帯が終わった実行中の回は、次の起動時に `completed` になり、録画は復旧処理によって `interrupted` になります。

### 注意点

- **配信が終了すると、予約による録画も確定します。** 時間帯の途中で配信者の接続が切れて猶予時間が過ぎると、その回の録画は `finalized` になり、
  同じ時間帯の中で配信者が戻っても新しい録画は始まりません。必要に応じて、手動で録画を開始してください。
- **`live` の予約が 1 つでもあると、時間帯の外の配信は拒否されます。** 判定は「取り消し（`canceled`）されていない `live` の予約があるか」で行い、
  終了した（`completed`・`missed`・`failed`）予約も対象になります。1 回限りの `live` の予約が終わった後に自由に配信できるようにしたい場合は、
  その予約を削除するか取り消してください。
- 実行中の予約を `canceled` にしても、すでに始まった録画や静止画の保存は止まりません。録画は `POST /recordings/{id}/stop` で停止してください。
