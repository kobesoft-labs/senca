# 5. 録画

[English](../en/05-recording.md)

stream を録画し、VOD として再生し、MP4・HLS として持ち出す。予約（スケジュール）で時刻を決めて録画・配信の受け付け・静止画を動かす。

- 設計の理由: [ADR-0005 録画の形式と索引](../../dev/adr/0005-recording.md)、[ADR-0006 stream タスク](../../dev/adr/0006-stream-hub.md)
- 関係するページ: [4. 取り込みと再生](04-ingest-playback.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[13. 運用](13-operations.md)、[14. コーデックと特許](14-codecs-and-patents.md)、[15. API と SDK](15-api-and-sdks.md)
- 実装: `crates/senca-node/src/recorder.rs`・`recovery.rs`・`export.rs`・`thumbnail.rs`・`scheduler.rs`・`api/recordings.rs`・`api/schedules.rs`、`crates/senca-record`、`crates/senca-control/src/schedule.rs`

例の `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じ。

## 5.1 しくみ

- 録画は**ライブ HLS と同じセグメント**をそのまま使う（別に多重化しない）。入れ物は stream の `hls.container`（TS か fMP4）。
- 誰かが録画しているときだけ、セグメントを `{data-dir}/segments/{stream}/{run}/` に不変のファイルとして書く（一時名 → fsync → rename）。
- 録画ごとに追記だけの索引 `{data-dir}/recordings/{id}/index.jsonl` を持つ。行は `begin`・`init`・`segment`・`gap`・`end`。
  セグメントを書いてから索引に追記するので、索引にあるセグメントは常に完全。
- 同じ時間の録画が複数あれば、同じセグメントのファイルを共有する（容量は 1 本分）。
- 録画の境目は**セグメント単位**。開始は「今作っているセグメント」から、停止は「停止時刻を含むセグメント」まで。
- 書き込みはライブより後回し。書き手へのキューがあふれたらセグメントを落とし、`gap`（`queue_overflow`）として記録する。ライブは止めない。
  キューの長さは `--record-queue`（既定 4096。stream ごとにはその 1/64、16〜4096）。
- クラスタでは、録画のファイルは録画を作ったノードのディスクにだけある（複製しない。[9. クラスタとグリッド](09-cluster-grid.md)）。

## 5.2 録画の状態とフィールド

| `state` | 意味 |
|---|---|
| `recording` | 録画中 |
| `finalized` | 正常に確定 |
| `interrupted` | 途中まで（ノードの停止・書き込みの失敗。残った分は再生・書き出しできる） |

削除中（内部の `deleting`）の録画は一覧・取得に出ない。

| フィールド | 意味 |
|---|---|
| `id`・`stream`・`name`・`generation` | |
| `state` | 上の表 |
| `started_at`・`ended_at` | |
| `retention_until` | 保持の期限（`null` は無期限） |
| `legal_hold` | 法的保全。真の間は削除も期限切れの掃除もしない |
| `schedule` | 予約が作った録画なら予約の ID |
| `duration_ms`・`segments`・`bytes` | 索引から集計 |
| `gaps` | 欠落の一覧 `[{at, reason, dropped}]`。`reason` は `publisher_reconnect`・`queue_overflow` |
| `thumbnail` | サムネイルがあるか |

注: 今の実装では、ノードの起動後に書いた録画の `gaps` はノードを再起動するまで空で表示される（索引には記録されている）。

## 5.3 自動録画（publish で始まる）

stream の `recording.enabled: true` で、送り手がつながると録画を始める。1 つの配信が 1 つの録画になる。

- 再接続の猶予（`--reconnect-grace`、既定 30 秒）の間につなぎ直しても**同じ録画が続く**（§5.9）。
- 猶予が切れて配信が終わると録画は `finalized` になる。次に送り手がつながると新しい録画を作る。
- 保持日数は `recording.retention_days`（`retention_until` = 開始時刻 + 日数）。
- `desired_state: stopped` の stream では録画を始めない。

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"lecture","recording":{"enabled":true,"retention_days":30}}'
```

## 5.4 手で始める・止める

```sh
# 始める（本文は省略できる）
curl -s -X POST "$API/streams/$SID/recordings" "${H[@]}" -d '{"name":"manual-1","retention_days":30}'
# 止める
curl -s -X POST "$API/recordings/$RID/stop" "${H[@]}"
```

| 操作 | 要求 | 権限 | 結果 |
|---|---|---|---|
| 開始 | `POST /streams/{id}/recordings` `{name?, retention_days?}` | `stream:write` | 202 と録画（`state: recording`）、`Location` |
| 停止 | `POST /recordings/{id}/stop` | `stream:write` | 202。録画中でなければ 409 |

- `retention_days` は 1〜36500。
- 送り手がいなくても始められる。セグメントは送り手が来てから記録される。
- stream が `stopped` なら 409。同じ stream の同時の録画（自動・手動・予約の合計）は 8 本まで（超えると 403 `QUOTA_EXCEEDED`）。
- 停止は非同期で、応答の時点では `state` がまだ `recording` のことがある。確定は `GET /recordings/{id}` で確かめる。
- 配信が終わる（猶予が切れる）と、手動の録画も一緒に確定する。

## 5.5 一覧・取得・変更・削除

| 操作 | 要求 | 権限 |
|---|---|---|
| 一覧 | `GET /recordings?stream={id}&state={recording\|finalized\|interrupted}&limit&cursor` | `recording:read` |
| 取得 | `GET /recordings/{id}` | `recording:read` |
| 変更 | `PATCH /recordings/{id}` `{retention_until?, legal_hold?, name?}`（`If-Match` 必須） | `stream:write` |
| 削除 | `DELETE /recordings/{id}` | `recording:delete` |

- `retention_until` は RFC 3339 か `null`（無期限）。`name` は 128 文字まで。
- `legal_hold` を外す（`true` → `false`）には `recording:delete` も要る。
- 削除は 202 `{"id": "...", "state": "deleting"}`。録画中なら止めてから消す。法的保全中は 409。
  他の録画が参照しているセグメントは消さない（どの索引からも参照されなくなったものだけ消す）。

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

- 権限は `recording:read`。`ttl_secs` の規則はライブと同じ（既定 300、上限 `--max-playback-ttl`）。`mp4` は今は常に `null`（MP4 は §5.7）。
- 再生リストは `#EXT-X-PLAYLIST-TYPE:VOD` と `#EXT-X-ENDLIST`。録画中は `EVENT`（`ENDLIST` なし）で、伸びていく。
- セグメントの名前は `{epoch}-{sequence}.ts`（fMP4 は `.m4s` と `init.mp4`）。`Range`（1 区間）に答え、`Cache-Control: max-age=3600`。
- 再接続の境目は `#EXT-X-DISCONTINUITY`（§5.9）。

実行例（猶予内に 1 回つなぎ直した録画）:

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

**HLS として持ち出す**: VOD の再生リストとセグメントがそのまま HLS の書き出しになる。ファイルにまとめるなら、例えば
`ffmpeg -i "$VOD_URL" -c copy out.ts` のように再符号化せずに取り出す。クラスタでは、録画を持たないノードに来た VOD の要求は
録画を作ったノードへ 307 で案内される。

## 5.7 MP4 の書き出し

```sh
curl -s -o rec.mp4 "$API/recordings/$RID/export.mp4" -H "Authorization: Bearer $TOKEN"
```

- 権限は `recording:read`。`Content-Type: video/mp4`、`Content-Disposition: attachment; filename="{id}.mp4"`。
- 再符号化しない。セグメントを分離し、先頭に `moov` を置いた MP4 にする。
- 録画中は 409。空の録画は 409。メモリで組み立てるので、セグメントの合計が 4 GiB を超える録画は 413 `PAYLOAD_TOO_LARGE`。
- 予約の録画は、予約の開始・終了の時刻（索引の `not_before_ms`・`not_after_ms`）で切り出す。開始は直前のキーフレームまで広げる。
  pre-roll・post-roll の分は VOD には残るが MP4 には入らない。
- 再接続で時刻が飛んだ区間は、前の区間の終わりに続けてつなぐ（空白の時間は入らない）。
- クラスタでは、録画を作ったノードの API に要求する（書き出し・サムネイルは手元のファイルを読む）。

## 5.8 サムネイル

録画が確定した後、セグメントから等間隔に最大 12 枚のキーフレームを復号し、黒・白・平坦・ぶれを避けて 1 枚を選ぶ。

```sh
curl -s -o thumb.jpg "$API/recordings/$RID/thumbnail" -H "Authorization: Bearer $TOKEN"
```

- 無ければ 404（録画の `thumbnail` が `false`）。
- 復号できる装置が要る。H.264/H.265 の録画は GPU のデコーダーが無いとサムネイルを作らない（[14. コーデックと特許](14-codecs-and-patents.md)）。

## 5.9 再接続と不連続

送り手が切れて、猶予の間に（同じ鍵で、別のプロトコルでもよい）つなぎ直したとき:

1. stream の `epoch` が 1 増える。
2. 録画の索引に `gap`（`reason: publisher_reconnect`）が入り、続くセグメントに不連続の印が付く。
3. VOD の再生リストには `#EXT-X-DISCONTINUITY` が入る。録画は 1 本のまま。
4. MP4 の書き出しでは区間を詰めてつなぐ。

猶予を過ぎてからつなぎ直したときは別の配信になり、自動録画なら新しい録画が始まる。

## 5.10 中断と回収

ノードが録画の途中で止まった（強制終了・電源断）ときは、次の起動で回収する。

- `end` の無い索引は、書きかけの末尾の行を切り詰め、`end`（`interrupted`）を足して閉じる。録画は `interrupted` になる。
  終了時刻は最後のセグメントの終わり。それまでのセグメントは再生・書き出しできる。
- 制御状態で `recording` のままで索引の無い録画も `interrupted` にする。
- どの索引からも参照されないセグメントは `segments/{stream}/orphans/{run}/` に移す（消さない）。
- 読めない索引があれば、その録画には触らず、セグメントの隔離もしない。
- `--verify-recordings` で起動時に各セグメントの SHA-256 を索引と照合する（合わなければログに出す）。
- 運転中にディスクへの書き込みが失敗した録画も `interrupted` になる。

実行例（録画中に `kill -9` して再起動）:

```
recovery: recording b0316a943b097502b1c041ea3810a5a5 closed as interrupted (3 segments)
```

## 5.11 保持期限

`--retention-sweep`（既定 60 秒）ごとに、`retention_until` を過ぎた `finalized`・`interrupted` の録画を消す。
`legal_hold: true` の録画は消さない。期限は自動録画では `recording.retention_days`、手動では開始の `retention_days`、
予約では予約の `retention_days` から決まり、あとから `PATCH` で変えられる。

## 5.12 予約（スケジュール）

予約は stream に対して時刻の窓を決め、窓の間だけ動作する。

| `action` | 窓の間にすること |
|---|---|
| `record` | 録画する（録画の `schedule` に予約の ID、`name` に予約の名前） |
| `live` | publish を受け付ける（窓の外は拒否） |
| `snapshot` | `snapshot_interval_secs` ごとに静止画を保存する（`reason: schedule`） |

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

| フィールド | 既定 | 検査 | 意味 |
|---|---|---|---|
| `stream` | （必須） | テナントの stream | |
| `name` | なし | | 録画の名前にもなる |
| `action` | （必須） | `record`・`live`・`snapshot` | |
| `start_at`・`end_at` | （必須） | RFC 3339。`end_at` > `start_at`、長さ 24 時間まで、`end_at` が過去でない | 最初の回の窓 |
| `pre_roll_ms`・`post_roll_ms` | 0 | 0〜300000 | 窓の前後に広げる |
| `grace_ms` | 60000 | 0〜3600000 | 開始（`start_at - pre_roll_ms`）を過ぎてもこの間なら途中から実行する。過ぎたら `missed` |
| `snapshot_interval_secs` | なし | `snapshot` だけで必須、1〜3600 | |
| `recurrence` | なし | 下の表 | 繰り返し |
| `retention_days` | なし | | `record` の録画の保持日数 |

`recurrence`:

| フィールド | 意味 |
|---|---|
| `freq` | `daily` か `weekly` |
| `weekdays` | `weekly` の曜日（0 = 日曜 〜 6 = 土曜）。書くとその曜日だけ（`start_at` の日も曜日が合わなければ実行しない）。空なら `start_at` の曜日から 7 日ごと |
| `timezone` | IANA のタイムゾーン名（例: `Asia/Tokyo`）。`--zoneinfo`（既定 `/usr/share/zoneinfo`）から読む。`UTC` はいつも使える。見つからなければ 400 |
| `until_ms` | 繰り返しの終わり（UTC のミリ秒、その時刻ちょうどに始まる回は含まない） |
| `count` | 回数の上限（最大 1000） |

- 2 回目以降は、最初の回の**壁時計の時刻**（そのタイムゾーンでの時刻）を保つ。夏時間の切り替えをまたいでも同じ時刻に始まる。
  存在しない時刻（夏時間の始まり）は後ろへずれ、2 回ある時刻（終わり）は早い方を使う。
- 同じ stream の `record` の予約は、pre-roll・post-roll を含めて同時に 4 本まで（超えると 409、`details.overlaps` に重なる予約）。
- テナントの予約は 1000 件まで。

### 確かめる・見る・変える

| 操作 | 要求 | 権限 |
|---|---|---|
| 作る | `POST /schedules` | `stream:write` |
| 作る前に確かめる | `POST /schedules/check`（本文は作成と同じ） | `stream:read` |
| 一覧 | `GET /schedules?stream=&state=&limit&cursor` | `stream:read` |
| 取得 | `GET /schedules/{id}`（`last_run` に直近の回） | `stream:read` |
| 実行の履歴 | `GET /schedules/{id}/runs`（直近 100 回） | `stream:read` |
| 変更 | `PATCH /schedules/{id}`（`If-Match` 必須） | `stream:write` |
| 削除 | `DELETE /schedules/{id}` | `stream:write` |

`check` は `{"ok": true, "overlaps": [...], "shared_input": true|false}` を返す（重なる録画予約は同じ入力・同じセグメントを共有する）。

`PATCH` で変えられるのは `name`・`start_at`・`end_at`・`pre_roll_ms`・`post_roll_ms`・`grace_ms`・`snapshot_interval_secs`・
`retention_days`、それと `state: "canceled"`（取り消し。他の値は 400）。実行中の予約の `start_at` は変えられない（409）。
`action`・`stream`・`recurrence` は変えられないので、作り直す。

### 予約と回の状態

予約の `state`: `pending`（次の回を待つ）・`running`・`completed`・`missed`・`canceled`・`failed`。
回（run）の `state` も同じ値で、`reason`（`missed`・`failed` の理由）と `recording`（作った録画の ID）を持つ。
繰り返しの予約は、回が終わる（`completed`・`missed`・`failed`）と `pending` に戻り、回が尽きたら `completed` になる。

### 実行の決まり

- 予約の係は 250 ms ごと（予約の変更ではすぐ）に動く。状態を保存してから副作用（録画の開始など）を起こす。
- `record`: 窓の開始（`start_at - pre_roll_ms`）で録画を始め、`end_at + post_roll_ms` で止める。索引には `start_at`・`end_at` が
  `not_before_ms`・`not_after_ms` として残り、MP4 の書き出しはそこで切り出す（§5.7）。送り手がいなくても録画は始まり、
  送り手が来てからのセグメントが入る。
- 回が始まるときに stream が無ければ `failed`、録画を始められなければ（`stopped`・同時の上限）その回は `failed`。
- ノードが止まっている間に窓が終わった実行中の回は、次の起動で `completed` になり、録画は回収で `interrupted` になる。

### 注意

- **配信が終わると予約の録画も確定する。** 窓の途中で送り手が切れて猶予が切れると、その回の録画は `finalized` になり、
  同じ窓の中で送り手が戻っても新しい録画は始まらない。必要なら手動で録画を始める。
- **`live` の予約は、ひとつでもあると窓の外の publish を拒否する。** 判定は「取り消し（`canceled`）でない `live` 予約があるか」で、
  終わった（`completed`・`missed`・`failed`）予約も数える。1 回だけの `live` 予約が終わった後に自由に publish させたいときは、
  その予約を削除するか取り消す。
- 実行中の予約を `canceled` にしても、すでに始まった録画・静止画の保存は止まらない。録画は `POST /recordings/{id}/stop` で止める。
