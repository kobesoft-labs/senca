# 5. Recording

[日本語](../ja/05-recording.md)

Record streams, play recordings back as VOD, export them as MP4 or HLS, and use schedules to drive recording, publish windows and still images by time.

- Rationale: [ADR-0005 recording format and index](../../adr/0005-recording.md), [ADR-0006 stream task](../../adr/0006-stream-hub.md)
- Related pages: [4. Ingest and playback](04-ingest-playback.md), [9. Cluster and grid](09-cluster-grid.md), [13. Operations](13-operations.md), [14. Codecs and patents](14-codecs-and-patents.md), [15. API and SDKs](15-api-and-sdks.md)
- Implementation: `crates/senca-node/src/recorder.rs`, `recovery.rs`, `export.rs`, `thumbnail.rs`, `scheduler.rs`, `api/recordings.rs`, `api/schedules.rs`, `crates/senca-record`, `crates/senca-control/src/schedule.rs`

`API`, `TOKEN` and `H` in the examples are the same as in [4. Ingest and playback](04-ingest-playback.md).

## 5.1 How it works

- Recording reuses **the same segments as live HLS** (no second multiplexing). The container is the stream's `hls.container` (TS or fMP4).
- Only while something is recording, segments are written as immutable files under `{data-dir}/segments/{stream}/{run}/`
  (temporary name → fsync → rename).
- Each recording has an append-only index, `{data-dir}/recordings/{id}/index.jsonl`, with `begin`, `init`, `segment`, `gap` and `end`
  lines. A segment is written before its index line, so every segment listed in an index is complete.
- Recordings that overlap in time share the same segment files (storage is used once).
- Recording boundaries are **segment-aligned**: a recording starts with the segment being built and stops with the segment that contains the stop time.
- Writing yields to live delivery. If the writer's queue overflows, segments are dropped and recorded as a `gap` (`queue_overflow`);
  live output never stops. The queue size is `--record-queue` (default 4096; each stream gets 1/64 of it, clamped to 16–4096).
- In a cluster, a recording's files exist only on the disk of the node that made it (they are not replicated; see [9. Cluster and grid](09-cluster-grid.md)).

## 5.2 Recording states and fields

| `state` | Meaning |
|---|---|
| `recording` | Recording in progress |
| `finalized` | Completed normally |
| `interrupted` | Partial (node stopped or a write failed; what was written can still be played and exported) |

Recordings being deleted (internal state `deleting`) are not shown in lists or lookups.

| Field | Meaning |
|---|---|
| `id`, `stream`, `name`, `generation` | |
| `state` | See the table above |
| `started_at`, `ended_at` | |
| `retention_until` | Retention deadline (`null` = forever) |
| `legal_hold` | Legal hold. While true, neither deletion nor the retention sweep removes the recording |
| `schedule` | ID of the schedule that made the recording, if any |
| `duration_ms`, `segments`, `bytes` | Totals from the index |
| `gaps` | Gaps as `[{at, reason, dropped}]`; `reason` is `publisher_reconnect` or `queue_overflow` |
| `thumbnail` | Whether a thumbnail exists |

Note: in the current implementation, `gaps` is shown as empty for recordings written since the node started, until the node is
restarted (the gaps are recorded in the index).

## 5.3 Automatic recording (starts on publish)

With `recording.enabled: true` on the stream, recording starts when a sender connects. One broadcast becomes one recording.

- Reconnects within the reconnect grace (`--reconnect-grace`, default 30 seconds) **continue the same recording** (§5.9).
- When the grace expires and the broadcast ends, the recording becomes `finalized`. The next sender starts a new recording.
- Retention comes from `recording.retention_days` (`retention_until` = start time + days).
- Streams with `desired_state: stopped` do not start recordings.

```sh
curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"lecture","recording":{"enabled":true,"retention_days":30}}'
```

## 5.4 Starting and stopping manually

```sh
# Start (the body is optional)
curl -s -X POST "$API/streams/$SID/recordings" "${H[@]}" -d '{"name":"manual-1","retention_days":30}'
# Stop
curl -s -X POST "$API/recordings/$RID/stop" "${H[@]}"
```

| Operation | Request | Scope | Result |
|---|---|---|---|
| Start | `POST /streams/{id}/recordings` `{name?, retention_days?}` | `stream:write` | 202 with the recording (`state: recording`) and `Location` |
| Stop | `POST /recordings/{id}/stop` | `stream:write` | 202; 409 if not recording |

- `retention_days` is 1–36500.
- You can start without a sender; segments are recorded once a sender arrives.
- A `stopped` stream gets 409. At most 8 concurrent recordings per stream (automatic, manual and scheduled combined); beyond that, 403 `QUOTA_EXCEEDED`.
- Stopping is asynchronous: the response may still show `state: recording`. Check `GET /recordings/{id}` for the final state.
- When the broadcast ends (the grace expires), manual recordings are finalized too.

## 5.5 List, get, update, delete

| Operation | Request | Scope |
|---|---|---|
| List | `GET /recordings?stream={id}&state={recording\|finalized\|interrupted}&limit&cursor` | `recording:read` |
| Get | `GET /recordings/{id}` | `recording:read` |
| Update | `PATCH /recordings/{id}` `{retention_until?, legal_hold?, name?}` (`If-Match` required) | `stream:write` |
| Delete | `DELETE /recordings/{id}` | `recording:delete` |

- `retention_until` is RFC 3339 or `null` (forever). `name` is at most 128 characters.
- Releasing a legal hold (`true` → `false`) additionally requires `recording:delete`.
- Deletion returns 202 `{"id": "...", "state": "deleting"}`. A recording in progress is stopped first. Recordings under legal hold get 409.
  Segments still referenced by other recordings are kept (only segments no index refers to are deleted).

```sh
ETAG=$(curl -s -D - -o /dev/null "$API/recordings/$RID" -H "Authorization: Bearer $TOKEN" | awk -F': ' 'tolower($1)=="etag"{print $2}' | tr -d '\r')
curl -s -X PATCH "$API/recordings/$RID" "${H[@]}" -H "If-Match: $ETAG" -d '{"legal_hold":true,"retention_until":null}'
```

## 5.6 VOD playback (HLS)

```sh
curl -s -X POST "$API/recordings/$RID/playback-url" "${H[@]}" -d '{"ttl_secs":3600}'
```

```json
{"expires_at":"2026-11-03T02:23:37Z",
 "playback":{"hls":"http://127.0.0.1:24080/s/1793669017/hMRgjzJPEFVV0OgLFjdkww/vod/b8bc7a7d6b9ff6df2639d7c07d22177f/index.m3u8","mp4":null}}
```

- Requires `recording:read`. `ttl_secs` follows the same rules as live (default 300, at most `--max-playback-ttl`). `mp4` is currently always `null` (for MP4, see §5.7).
- The playlist has `#EXT-X-PLAYLIST-TYPE:VOD` and `#EXT-X-ENDLIST`. While recording, it is an `EVENT` playlist (no `ENDLIST`) that keeps growing.
- Segments are named `{epoch}-{sequence}.ts` (fMP4: `.m4s` plus `init.mp4`). Single-range `Range` requests are supported; `Cache-Control: max-age=3600`.
- Reconnect boundaries are marked with `#EXT-X-DISCONTINUITY` (§5.9).

Example (a recording with one reconnect within the grace):

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

**Exporting as HLS**: the VOD playlist and its segments are the HLS export. To collect them into a file without re-encoding, use for
example `ffmpeg -i "$VOD_URL" -c copy out.ts`. In a cluster, VOD requests reaching a node that does not hold the recording are redirected
with 307 to the node that made it.

## 5.7 MP4 export

```sh
curl -s -o rec.mp4 "$API/recordings/$RID/export.mp4" -H "Authorization: Bearer $TOKEN"
```

- Requires `recording:read`. `Content-Type: video/mp4`, `Content-Disposition: attachment; filename="{id}.mp4"`.
- No re-encoding: segments are demultiplexed and written as an MP4 with `moov` at the front.
- A recording in progress gets 409, an empty recording 409. The file is built in memory, so recordings whose segments exceed 4 GiB in total get 413 `PAYLOAD_TOO_LARGE`.
- Scheduled recordings are trimmed to the schedule's start and end (`not_before_ms` and `not_after_ms` in the index); the start is
  widened back to the preceding keyframe. Pre-roll and post-roll stay in the VOD but are not included in the MP4.
- Sections separated by reconnects are joined back-to-back (the time gap is removed).
- In a cluster, send the request to the API of the node that made the recording (export and thumbnails read local files).

## 5.8 Thumbnails

After a recording is finalized, up to 12 evenly spaced keyframes are decoded and the best one is chosen, avoiding black, white, flat and blurry frames.

```sh
curl -s -o thumb.jpg "$API/recordings/$RID/thumbnail" -H "Authorization: Bearer $TOKEN"
```

- 404 if there is none (the recording's `thumbnail` is `false`).
- A decoder is required: H.264/H.265 recordings get no thumbnail without a GPU decoder ([14. Codecs and patents](14-codecs-and-patents.md)).

## 5.9 Reconnects and discontinuities

When the sender drops and reconnects within the grace (with the same key, possibly over a different protocol):

1. The stream's `epoch` increases by 1.
2. A `gap` with `reason: publisher_reconnect` is written to the recording's index, and the following segment is marked as a discontinuity.
3. The VOD playlist gets `#EXT-X-DISCONTINUITY`. It is still one recording.
4. MP4 export joins the sections without the gap.

A reconnect after the grace is a new broadcast; with automatic recording a new recording starts.

## 5.10 Interruption and recovery

If the node stops during a recording (killed, power loss), the next start recovers:

- An index without an `end` line has its torn last line truncated and is closed with `end` (`interrupted`). The recording becomes
  `interrupted`, its end time is the end of the last segment, and everything up to that point can be played and exported.
- Recordings still `recording` in the control state but without an index also become `interrupted`.
- Segments not referenced by any index are moved to `segments/{stream}/orphans/{run}/` (not deleted).
- If an index cannot be parsed, that recording is left untouched and no segments are quarantined.
- `--verify-recordings` checks every segment's SHA-256 against the index at startup (mismatches are logged).
- A recording whose disk write fails while running also becomes `interrupted`.

Example (node killed with `kill -9` during a recording, then restarted):

```
recovery: recording b0316a943b097502b1c041ea3810a5a5 closed as interrupted (3 segments)
```

## 5.11 Retention

Every `--retention-sweep` seconds (default 60), `finalized` and `interrupted` recordings past `retention_until` are deleted.
Recordings with `legal_hold: true` are never deleted. The deadline comes from `recording.retention_days` (automatic), the start
request's `retention_days` (manual), or the schedule's `retention_days` (scheduled), and can be changed later with `PATCH`.

## 5.12 Schedules

A schedule sets a time window on a stream and acts only within the window.

| `action` | Within the window |
|---|---|
| `record` | Record (the recording's `schedule` is the schedule ID and its `name` the schedule name) |
| `live` | Accept publishing (refused outside the window) |
| `snapshot` | Save a still image every `snapshot_interval_secs` (`reason: schedule`) |

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

| Field | Default | Validation | Meaning |
|---|---|---|---|
| `stream` | (required) | A stream of the tenant | |
| `name` | none | | Also used as the recording name |
| `action` | (required) | `record`, `live`, `snapshot` | |
| `start_at`, `end_at` | (required) | RFC 3339; `end_at` > `start_at`; at most 24 hours long; `end_at` not in the past | Window of the first occurrence |
| `pre_roll_ms`, `post_roll_ms` | 0 | 0–300000 | Extend the window before and after |
| `grace_ms` | 60000 | 0–3600000 | If the start (`start_at - pre_roll_ms`) has passed by less than this, run from the middle; otherwise `missed` |
| `snapshot_interval_secs` | none | Required for `snapshot` only, 1–3600 | |
| `recurrence` | none | See below | Repetition |
| `retention_days` | none | | Retention for recordings made by `record` |

`recurrence`:

| Field | Meaning |
|---|---|
| `freq` | `daily` or `weekly` |
| `weekdays` | Days for `weekly` (0 = Sunday … 6 = Saturday). When given, only those days are used (the `start_at` day is skipped if it is not listed). When empty, every 7 days from the `start_at` day |
| `timezone` | IANA time zone name (e.g. `Asia/Tokyo`), read from `--zoneinfo` (default `/usr/share/zoneinfo`). `UTC` always works. Unknown names get 400 |
| `until_ms` | End of the repetition (UTC milliseconds; an occurrence starting exactly at this time is excluded) |
| `count` | Maximum number of occurrences (at most 1000) |

- Later occurrences keep the **wall-clock time** of the first one in that time zone, so they start at the same local time across DST
  changes. A nonexistent local time (DST start) moves forward; an ambiguous one (DST end) uses the earlier instant.
- At most 4 `record` schedules may overlap on the same stream, pre-roll and post-roll included (beyond that 409, with the overlapping schedules in `details.overlaps`).
- A tenant can have up to 1000 schedules.

### Check, inspect, change

| Operation | Request | Scope |
|---|---|---|
| Create | `POST /schedules` | `stream:write` |
| Check before creating | `POST /schedules/check` (same body as create) | `stream:read` |
| List | `GET /schedules?stream=&state=&limit&cursor` | `stream:read` |
| Get | `GET /schedules/{id}` (`last_run` holds the latest occurrence) | `stream:read` |
| Run history | `GET /schedules/{id}/runs` (last 100 occurrences) | `stream:read` |
| Update | `PATCH /schedules/{id}` (`If-Match` required) | `stream:write` |
| Delete | `DELETE /schedules/{id}` | `stream:write` |

`check` returns `{"ok": true, "overlaps": [...], "shared_input": true|false}` (overlapping record schedules share the same input and segments).

`PATCH` can change `name`, `start_at`, `end_at`, `pre_roll_ms`, `post_roll_ms`, `grace_ms`, `snapshot_interval_secs`,
`retention_days`, and `state: "canceled"` (cancel; any other value is 400). `start_at` of a running schedule cannot be changed (409).
`action`, `stream` and `recurrence` cannot be changed; create a new schedule instead.

### Schedule and run states

Schedule `state`: `pending` (waiting for the next occurrence), `running`, `completed`, `missed`, `canceled`, `failed`.
Each run (occurrence) uses the same values and has `reason` (why it was `missed` or `failed`) and `recording` (the recording it created).
A recurring schedule returns to `pending` after each occurrence ends (`completed`, `missed` or `failed`) and becomes `completed` when the occurrences are exhausted.

### Execution rules

- The scheduler runs every 250 ms (and immediately when schedules change). It saves the state before causing side effects such as starting a recording.
- `record`: recording starts at the window start (`start_at - pre_roll_ms`) and stops at `end_at + post_roll_ms`. The index keeps
  `start_at` and `end_at` as `not_before_ms` and `not_after_ms`, and MP4 export trims there (§5.7). Recording starts even without a
  sender; segments are added once a sender arrives.
- If the stream no longer exists when an occurrence starts, it is `failed`; if the recording cannot start (`stopped`, concurrency limit), that occurrence is `failed`.
- An occurrence that was running while the node was down and whose window has ended becomes `completed` at the next start, and its recording becomes `interrupted` through recovery.

### Caveats

- **When the broadcast ends, the scheduled recording is finalized too.** If the sender drops in the middle of the window and the grace
  expires, that occurrence's recording becomes `finalized`, and no new recording starts if the sender returns within the same window.
  Start a recording manually if needed.
- **A single `live` schedule makes publishing outside its windows fail.** The check is "is there any `live` schedule that is not
  `canceled`", and finished (`completed`, `missed`, `failed`) schedules count. To allow free publishing after a one-off `live`
  schedule has ended, delete or cancel it.
- Cancelling a running schedule does not stop a recording or still-image saving that has already started. Stop the recording with `POST /recordings/{id}/stop`.
