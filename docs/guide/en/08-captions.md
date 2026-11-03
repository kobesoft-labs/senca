# 8. Captions and translation

[日本語](../ja/08-captions.md)

Send a live stream's audio to a speech recognition service, turn the result into captions, optionally translate them, and publish
per-language WebVTT subtitles in HLS.

- Rationale: [ADR-0011 captions and translation](../../dev/adr/0011-captions.md), [ADR-0004 codecs and patents](../../dev/adr/0004-codecs-patents.md) (audio is not decoded)
- Related pages: [4. Ingest and playback](04-ingest-playback.md), [6. Inputs and outputs](06-inputs-outputs.md) (secrets, egress restrictions), [11. Media pipelines](11-media-pipelines.md) (captions for room participants), [12. Security](12-security.md)

`API`, `TOKEN` and `H` in the examples are the same as in [4. Ingest and playback](04-ingest-playback.md).

## 8.1 How it works

```
audio AUs ─▶ repackaged into 5 s windows (1 s overlap) ─▶ speech recognition ─▶ merged captions (cues)
              (no decoding: AAC → m4a, Opus → Ogg)                                  │
                                                         ┌──────────────────────────┴───────────┐
                                                         ▼                                      ▼
                                              WebVTT in the source language        batched translation → WebVTT per language
                                                         └──────────── HLS subtitles (sub_{lang}.m3u8) + master.m3u8
```

- **Nothing is sent anywhere unless configured.** Audio and text go only to the destinations set in the stream's `captions`.
- Audio is **not decoded**. AAC is repackaged as audio-only MP4 (`.m4a`) and Opus as Ogg Opus (`.ogg`); the remote service decodes it.
  Other audio codecs (G.711, …) cannot be captioned.
- The captioning task starts when a sender connects and stops when the broadcast ends (the reconnect grace expires). Changes to
  `captions` during a broadcast take effect from the next broadcast.
- In a cluster it runs on the node that received the sender (the owner). Relay mirrors do not produce captions. The caption files live
  in the owner node's memory; on a non-owner node serving from a mirror, that node fetches the caption files (`master.m3u8`,
  `sub_{lang}.m3u8`, `sub_{lang}_{n}.vtt`) from the owner node and returns them (playlists are reused for 1 second, caption segments
  for 30 seconds). Captions work with a playback URL on any node.

## 8.2 Settings (the stream's `captions`)

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"openai","value":"sk-..."}'
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"anthropic","value":"sk-ant-..."}'

curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" -d '{
  "captions": {
    "asr_provider": "openai",
    "asr_endpoint": "https://api.openai.com/v1",
    "asr_credential_ref": "secret:openai",
    "language": "ja",
    "translate_provider": "anthropic",
    "translate_endpoint": "https://api.anthropic.com",
    "translate_credential_ref": "secret:anthropic",
    "targets": ["en", "zh"]
  }
}'
```

| Field | Default | Validation | Meaning |
|---|---|---|---|
| `asr_provider` | (required) | `openai`, `whisper_cpp` | Speech recognition type |
| `asr_endpoint` | (required) | `http://` or `https://` URL | Base URL of the service (may include a path) |
| `asr_credential_ref` | none | `secret:{name}` of the tenant | API key (used by `openai`) |
| `asr_model` | `whisper-1` | | Model name for `openai` |
| `language` | `ja` | 1–8 letters, digits or `-` | Spoken language (ISO 639-1). Passed to recognition; the language of the source captions |
| `translate_provider` | none | `anthropic`, `deepl`, `openai` | Translate when set |
| `translate_endpoint` | none | Required when `translate_provider` is set | Base URL of the translation service |
| `translate_credential_ref` | none | `secret:{name}` | API key (required for `anthropic` and `deepl`) |
| `translate_model` | `anthropic`: `claude-haiku-5-5`; `openai`: `gpt-4o-mini` | | Not used by `deepl` |
| `targets` | `[]` | Up to 8, same format as `language` | Target languages |

- `captions: null` turns captions off.
- Referencing a missing secret is 400. Secrets referenced by captions cannot be deleted (409). Creating secrets requires `tenant:admin` ([6. Inputs and outputs](06-inputs-outputs.md)).
- Configuration errors such as a missing `translate_credential_ref` for `anthropic`/`deepl` or a secret that cannot be opened are not
  caught by the API; when the broadcast starts, the captioning task logs the reason and stops (the broadcast continues).

## 8.3 Requests sent

The following paths are appended to `asr_endpoint` / `translate_endpoint` and sent with POST.

| Kind | Path | Authentication | Body |
|---|---|---|---|
| `openai` (recognition) | `{asr_endpoint}/audio/transcriptions` | `Authorization: Bearer {key}` (if a key is set) | multipart: `file`, `model`, `response_format=verbose_json`, `timestamp_granularities[]=segment`, `language`, `prompt` (last 200 characters of the previous result) |
| `whisper_cpp` (recognition) | `{asr_endpoint}/inference` | none | multipart: `file`, `response_format=verbose_json`, `language`, `prompt` |
| `anthropic` (translation) | `{translate_endpoint}/v1/messages` | `x-api-key`, `anthropic-version: 2023-06-01` | JSON |
| `deepl` (translation) | `{translate_endpoint}/v2/translate` | `Authorization: DeepL-Auth-Key {key}` | JSON |
| `openai` (translation) | `{translate_endpoint}/chat/completions` | `Authorization: Bearer {key}` (if a key is set) | JSON |

So write `asr_endpoint` as `https://api.openai.com/v1` (for OpenAI-compatible servers, up to their `/v1`), or `http://whisper.internal:8080`
for a whisper.cpp server; `translate_endpoint` as `https://api.anthropic.com` for Anthropic, `https://api.deepl.com` for DeepL
(`https://api-free.deepl.com` for the Free plan), or `https://api.openai.com/v1` for OpenAI.

Each request times out after 60 seconds. API keys never appear in logs or errors.

**Egress restrictions**: like inputs and outputs, destinations are checked by resolved IP. To use a server on a private address, such
as an on-premises whisper.cpp, allow the range with `--egress-allow` ([6. Inputs and outputs](06-inputs-outputs.md)).

Example (recognize with an on-premises whisper.cpp, translate to English with DeepL):

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"deepl","value":"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx:fx"}'
curl -s -X POST "$API/streams" "${H[@]}" -d '{
  "name": "lecture",
  "captions": {"asr_provider": "whisper_cpp", "asr_endpoint": "http://10.0.0.5:8080", "language": "ja",
               "translate_provider": "deepl", "translate_endpoint": "https://api-free.deepl.com",
               "translate_credential_ref": "secret:deepl", "targets": ["en"]}
}'
# The node must be started with --egress-allow 10.0.0.5/32
```

## 8.4 Audio chunking and recognition

- A 5-second window slides with 1 second of overlap. Boundaries are aligned to access units, and each chunk carries its starting
  media time (used to map recognition timestamps to stream time).
- At a gap (discontinuity, timestamp jump) the window is cut and a new one starts.
- Recognition runs one request at a time, in a task separate from ingest. If recognition falls behind real time and its queue
  (2 chunks) overflows, **the oldest chunks are dropped** (captions have holes; the broadcast never stops).
- Overlapping results are deduplicated into a single caption track, split into cues of 0.7–7 seconds with up to 64 columns per line
  (accounting for East Asian display width) and 2 lines.
- Translation batches finalized cues (up to 6 cues, 1500 characters, or 1.5 seconds) and includes the previous 3 cues as context.
  Translation failures are logged and those translations are missing.

## 8.5 HLS subtitles

Captions appear in **live HLS**. The following files appear under the same signed path as the live playback URL.

| File | Content |
|---|---|
| `master.m3u8` | Multivariant playlist: one `EXT-X-MEDIA:TYPE=SUBTITLES` per language (group `subs`, the `language` rendition is `DEFAULT`) and `EXT-X-STREAM-INF:BANDWIDTH=3000000,SUBTITLES="subs"` pointing at the video `index.m3u8` |
| `sub_{lang}.m3u8` | Per-language subtitle media playlist |
| `sub_{lang}_{n}.vtt` | Subtitle segments, numbered and cut like the video segments, with `X-TIMESTAMP-MAP` to align with the TS/fMP4 timeline |

- To see captions, replace `index.m3u8` in the playback URL with `master.m3u8` (the file name is not signed, so the same URL works).
- `master.m3u8` appears when the first video segment closes after the captioning task has started; until then it returns 404.
- Up to 256 subtitle segments are kept in memory.

```sh
HLS=$(curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" | python3 -c "import json,sys;print(json.load(sys.stdin)['playback']['hls'])")
MASTER=${HLS%index.m3u8}master.m3u8
curl -s "$MASTER"
curl -s "${HLS%index.m3u8}sub_en.m3u8"
```

Captions are not available in:

- Recordings (VOD) and MP4 exports; captions are not stored with recordings.
- WebRTC (WHEP) and RTMP/SRT/RTSP play.

## 8.6 Latency

SENCA uses a **captions-may-lag** model; video is never delayed.

- A caption appears after its audio has filled a window (up to 5 seconds) and recognition (and translation) has returned. Recognition
  usually takes a few seconds, so captions trail speech by several seconds up to a dozen or so.
- If, when a caption arrives, the video segment for its time has already closed, the caption's start is moved to the time of the open
  segment (keeping its length or 0.5 seconds, whichever is longer), so players show it with the current segment.
- Delaying video to keep it in sync with captions is not supported yet.

## 8.7 Troubleshooting

The node log shows lines starting with `captions {stream}: ...`.

| Log | Cause |
|---|---|
| `asr failed: ...` | Wrong recognition endpoint, authentication, or timeout; captions are missing for that span |
| `translation failed: ...` | Translation failure |
| `... は許されていない宛先です` ("not an allowed destination") | The destination is a private address (`--egress-allow`) |
| `翻訳の資格情報がありません` ("no translation credentials") and similar | No key for `anthropic`/`deepl`; the captioning task does not start |
| Nothing at all | The audio is neither AAC nor Opus, or `captions` was set during the broadcast (takes effect next time) |
