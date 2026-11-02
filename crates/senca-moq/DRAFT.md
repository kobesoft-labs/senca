# senca-moq: 固定した仕様の版と相互接続の相手

仕様 6.5「P0 で仕様本文と関連仕様を固定し、ハッシュ・取得日を保存する」の記録。MoQ は Internet-Draft
であり、RFC 準拠・将来版との互換は保証しない（実験扱い）。

## MoQ Transport

| 項目 | 値 |
|---|---|
| 版 | **draft-ietf-moq-transport-22**（2026-10-01 公開、2027-04-04 失効） |
| datatracker | https://datatracker.ietf.org/doc/draft-ietf-moq-transport/22/ |
| 本文 | https://www.ietf.org/archive/id/draft-ietf-moq-transport-22.txt |
| 取得日 | 2026-11-02（datatracker の最新版が 22 であることを API で確認） |
| SHA-256（本文 txt） | `1a725f10db526d94f915b30db9004821d86e57afcdde10789c54fee64b3056b5` |
| ALPN（native QUIC） | `moqt-22`（6.2） |
| WebTransport | `wt-available-protocols` に `"moqt-22"`、応答 `wt-protocol: "moqt-22"` |
| 可変長整数 | draft-22 8.1 の「先頭の 1 の数」形式（QUIC varint とは別、`src/varint.rs`） |

版の交渉は ALPN（WebTransport では `wt-available-protocols`）だけで行い、未知の版は推測せずに拒否する。

- native QUIC: TLS の ALPN が一致しなければ `no_application_protocol`（QUIC の CRYPTO_ERROR 0x178）で閉じる。
- WebTransport: `wt-available-protocols` に `moqt-22` が無ければ 400 で拒否する。ヘッダーが無い要求は
  既定で拒否し、`MoqServer::allow_missing_wt_protocol` を立てたときだけ受け付ける。

## WebTransport over HTTP/3（senca-h3）

| 版 | 本文の SHA-256 | 備考 |
|---|---|---|
| draft-ietf-webtrans-http3-02（2021-10-25） | `d4940a366a69752890adb32a0734bfeb43d3fea8262000e23e64424112694b45` | Chrome 156 の既定（SETTINGS_ENABLE_WEBTRANSPORT 0x2b603742） |
| draft-ietf-webtrans-http3-07（2023-06） | `0b27274d426dce43b3459b271d9c83a0df13009ce959a0b8e8eb337c97b19bdf` | SETTINGS_WEBTRANSPORT_MAX_SESSIONS 0xc671706a、Chrome は機能フラグ `EnableWebTransportDraft07` |

datatracker の最新は draft-16（2026-07）だが、Chrome（Chromium の QUICHE、2026-10-30 の main、
`net/quic/dedicated_web_transport_http3_client.cc`）が話すのは draft-02 と draft-07 だけなので、この二つを
実装し、両者が広告したうち新しい方を使う（QUICHE と同じ規則）。stream（0x41・0x54）・datagram
（Quarter Stream ID）・capsule（CLOSE 0x2843、DRAIN 0x78ae）の形式は両版で共通の範囲で実装した。

## 相互接続の相手

| 実装 | 版 | 結果 |
|---|---|---|
| **moq-dev/moq**（`moq` 0.14.2、`moq-relay` 0.17.2、`moq-net` 0.3.10、commit 8e53bf32、2026-11-02） | moq-transport-18/20/22 | 採用。cargo で手元に作れる。native QUIC（quinn）と WebTransport（web-transport-quinn）の client・relay、hang の catalog と fMP4 の取り込みがある |
| cloudflare/moq-rs（moq-transport 0.16.5） | draft-11〜14・16 | draft-22 に対応しないので使わない |
| Meta moxygen | — | C++ の build が重く、今回は試していない |

`tools/interop/quic_moq.sh` の C 節で次を確かめる（2026-11-02 時点ですべて合格）。

- moq-dev の publisher（`moq import fmp4`、native QUIC・WebTransport の両方）→ senca relay →
  senca の subscriber（catalog・video）・moq-dev の `moq fetch`（group 指定・最新、native・WebTransport）
- senca の publisher → moq-relay → senca の subscriber（native QUIC・WebTransport）・moq-dev の `moq fetch`
- 未知の版（`moqt-20`）の client の拒否

D 節では Chrome の WebTransport（QUICHE）の上で最小の MoQ subscriber（JS、このリポジトリのもの）が
senca relay から object を受け取ることを確かめる（MoQ の符号化は独立実装ではない）。

## メディアの対応づけ（SENCA の MoQ メディアプロファイル v1、実験扱い）

moq-dev の **hang**（draft-lcurley-moq-hang、moq-dev/moq の `drafts/` 2026-11-02 時点）の catalog に合わせる。

- broadcast の namespace の下の `catalog.json` track: 更新ごとに一つの group、group の中は一つの object（UTF-8 JSON）。
  圧縮（moq-flate 拡張）は使わない。
- `video`・`audio` の `renditions` の項目が track。SENCA が出すときの容器は
  `{"kind": "cmaf", "init": base64(ftyp+moov)}`、object 一つが CMAF fragment 一つ（`moof`+`mdat`）、
  group は keyframe の fragment から（GOP ごと）、subgroup 0、Object ID は group の中で 0 から。
- moq-dev の `moq import fmp4` は `legacy` 容器（frame ごとに timestamp と codec の payload）を出す。
  relay は容器を読まずに転送するので、どちらの容器も中継できる。

プレーヤーの互換はこのプロファイルの範囲でだけ扱い、transport の相互接続だけで保証しない（仕様 6.5）。

## 実装した範囲・しない範囲

実装した: SETUP（PATH・AUTHORITY・AUTHORIZATION TOKEN・MOQT_IMPLEMENTATION）、GOAWAY、
PUBLISH_NAMESPACE、SUBSCRIBE_NAMESPACE・NAMESPACE・NAMESPACE_DONE、SUBSCRIBE・SUBSCRIBE_OK、PUBLISH（出版者起点）、
PUBLISH_DONE、FETCH・FETCH_OK（cache から・上流への中継）、TRACK_STATUS（cache・上流への中継）、
REQUEST_UPDATE（新しい Request ID）、REQUEST_OK・REQUEST_ERROR、subgroup stream・datagram・fetch stream、
padding、Location Filter、優先度（購読者・出版者・群の順序を QUIC の stream の優先度へ）、
OBJECT_DELIVERY_TIMEOUT・SUBGROUP_DELIVERY_TIMEOUT（期限切れの object を送信待ちから捨て、stream をリセット）、
track alias が分からない stream・datagram の一時保管（上限つき）、SETUP の前に届いた要求の保留。

しない（明示的に拒否する・無視する）:

- SUBSCRIBE_TRACKS は NOT_SUPPORTED で拒否する。
- Range Filter（0x25〜0x29）: MAX_FILTER_RANGES を 0 として扱い、受けたら INVALID_FILTER。
- 認可トークンの alias: MAX_AUTH_TOKEN_CACHE_SIZE を 0 とし、SETUP の外の REGISTER は
  AUTH_TOKEN_CACHE_OVERFLOW、未登録の alias の参照は UNAUTHORIZED。
- FILL_PARAMETERS は読むが fill fetch stream は開かない（購読者は FETCH で埋める）。
- RENDEZVOUS_TIMEOUT は使わない（出版者がいなければすぐ DOES_NOT_EXIST）。
- RESET_STREAM_AT は使わない（QUIC 側に無い）。PUBLISH_STATE_NOTIFY は送らない。
- relay は object を丸ごと受けてから転送する（途中の object の転送はしない）。
- 制御メッセージ・データ stream の時間切れ（CONTROL_MESSAGE_TIMEOUT など）は未実装。

## relay が見るもの（仕様 6.5「区間暗号化と E2EE の区別」）

QUIC（TLS 1.3）の暗号化は区間ごと（hop-by-hop）で、relay では平文になる。relay が読む・使うのは
namespace・track 名・Track Properties・Object Properties・Group/Subgroup/Object ID・優先度・Object Status・
payload の長さで、payload は読まず・変えずに転送し、cache に payload と SHA-256 を保存する。E2EE の
payload（アプリケーションが暗号化したもの）は relay を素通りするが、relay が見るメタデータは保護されない。
