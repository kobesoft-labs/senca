# 適合台帳

仕様 16 章の台帳。標準の条項を全部は展開していない（「完全準拠」とは宣言しない）。状態の意味:
`相互接続済み`（独立実装と試験した）・`自前の試験のみ`・`未実施`・`未検証（装置なし）`。
試験の場所: `tools/interop/*.sh`（独立実装との突き合わせ）、各 crate の `tests/`。

| 機能 ID | 基準・版 | 実装 | 試験 | 状態 |
|---|---|---|---|---|
| RTMP-01 | RTMP（Adobe 仕様 1.0）、AMF0 | senca-amf0 / senca-flv / senca-rtmp | rtmp_basic.sh、node.sh A・D・H | 相互接続済み（FFmpeg 7.0.2） |
| RTMP-ERTMP-01 | Enhanced RTMP v2（Veovera） | senca-flv | node.sh G（自前の送り手） | 自前の試験のみ（FFmpeg 7.0 は AV1 の FLV を書けない） |
| TS-01 | ISO/IEC 13818-1（PAT/PMT/PES/PCR）、Opus in TS | senca-mpegts | ts_fmp4.sh | 相互接続済み |
| MP4-01 | ISO/IEC 14496-12/-15、av1C、vpcC、dOps | senca-fmp4 | ts_fmp4.sh、node.sh A・G | 相互接続済み |
| HLS-01 | RFC 8216 | senca-hls、senca-node packager | hls_playlist.sh、node.sh A・G | 相互接続済み（FFmpeg） |
| HLS-LL-01 | Apple LL-HLS（PART・PRELOAD-HINT・ブロッキング） | senca-hls、live.rs | 単体試験 | 自前の試験のみ（Apple の検証器・Safari は未実施） |
| HTTP-01 | RFC 9110 / 9112 | senca-http | http_basic.sh（curl、密輸の要求） | 相互接続済み |
| API-01 | 本仕様 10 章、OpenAPI 3.1 | senca-node api | openapi.sh（45 操作） | 自前の試験のみ |
| AUTH-JWT-01 | RFC 7519、EdDSA / ES256 / RS256 | senca-control | 単体試験 | 自前の試験のみ |
| SRT-01 | SRT（libsrt 1.4.4 / 1.5.4 互換）、AES-CTR / GCM | senca-srt、srt.rs | srt.sh、node.sh I | 相互接続済み（libsrt） |
| RTP-01 | RFC 3550、4585、5104、4588、8285、RTP ペイロード（6184・7798・7741・VP9・AV1・7587・3640） | senca-rtp | rtp_rtsp.sh | 相互接続済み（FFmpeg。AV1 は自前の試験のみ） |
| SDP-01 | RFC 8866、3264、9429（必要部分） | senca-sdp | rtp_rtsp.sh | 相互接続済み（FFmpeg・mediamtx） |
| RTSP-01 | RFC 2326、Digest（RFC 7616） | senca-rtsp、rtsp_pull.rs | rtp_rtsp.sh、node.sh I | 相互接続済み（mediamtx・FFmpeg） |
| STUN-01 | RFC 8489、RFC 5769 の試験ベクター | senca-stun | ice_turn.sh | 相互接続済み（coturn） |
| ICE-01 | RFC 8445、8838、7675 | senca-ice | 単体（模擬ネットワーク）、ice_turn.sh | 自前の試験のみ（ブラウザーは M8 で） |
| TURN-01 | RFC 8656 | senca-turn | ice_turn.sh | 相互接続済み（coturn） |
| DTLS-01 | RFC 6347、7627（EMS 必須）、5764、5705 | senca-dtls | dtls_srtp.sh | 相互接続済み（OpenSSL 3.5） |
| SRTP-01 | RFC 3711、7714 | senca-srtp | dtls_srtp.sh、RFC の試験ベクター | 相互接続済み（FFmpeg の SRTP、AEAD-GCM はベクターのみ） |
| RTC-SEC-01 | RFC 8827 | senca-dtls・senca-srtp・senca-webrtc | 未実施 | 独立監査の前（仕様 15.3） |
| RAFT-01 | Raft（Ongaro）、joint consensus、ReadIndex | senca-raft | 決定的なシミュレーション（6000 通り以上） | 自前の試験のみ |
| DIST-FENCE-01 | 本仕様 8.2 | senca-raft（FencingToken） | 未実施（ノードへの統合は M9） | 未実施 |
| CODEC-HW-01 | NVENC/NVDEC、oneVPL、AMF、VA-API | senca-codec | codec_sw.sh（装置なしの検出） | 未検証（装置なし） |
| CODEC-RF-01 | dav1d、SVT-AV1、libvpx、libopus | senca-codec | codec_sw.sh、node.sh G | 相互接続済み（FFmpeg の復号器とビット一致） |
| IMG-01 | JPEG（ISO/IEC 10918-1 基本方式）、PNG | senca-image | image.sh | 相互接続済み（FFmpeg） |
| AC-17 | codec worker の障害の隔離 | senca-codec-worker | codec_sw.sh（crash・hang・lie・oversize） | 自前の試験のみ |
| EGRESS-01 | 本仕様 9.4（SSRF） | senca-node egress | node.sh H、単体試験 | 自前の試験のみ |

## 仕様との違い（記録）

- 録画は映像・音声を 1 本のセグメントに多重化する（仕様 8.4 の track 階層ではない。ADR-0005）。
- PATCH stream は 200（仕様は 202）、DELETE stream は 204（仕様は 202）。operation resource は分散（M9）で入れる。
- H.264/H.265 の符号化・復号はハードウェアだけ（ADR-0004）。装置が無い環境では静止画・サムネイル・変換は 503。
- AAC は符号化・復号しない（パススルーだけ）。AAC の音声を WebRTC へ出すことはできない（変換計画で理由付きで拒否）。
