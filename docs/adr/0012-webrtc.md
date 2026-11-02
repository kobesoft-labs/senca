# ADR-0012 WebRTC（WHIP・WHEP）

- 状態: 採用（2026-11-02）。公開の前に暗号の部分（DTLS-SRTP）の独立監査が要る（仕様 2.2・15.3）
- 関係: 仕様 6.3、R6-3、REQ-008、REQ-009、RFC 9725

## 決定

- ノードの WebRTC は 1 つの UDP ソケット（`--webrtc-udp`）で、STUN の USERNAME と送り元のアドレスでセッションへ振り分ける。サーバーは ICE-lite（既定）。
- **WHIP**（送る）: `POST /whip/{stream_id}`、`Authorization: Bearer {stream_key}`（stream の publish 鍵）、`application/sdp`。201・`Location`・answer。`PATCH`（trickle・ICE restart、RFC 8840）、`DELETE`。受けた AU は stream の送り手として hub へ入れる。
- **WHEP**（見る）: `POST /whep/{stream_id}?exp&sig`（署名付きの再生 URL と同じ署名）。配る stream のコーデックだけを申し出る。AAC は WebRTC で運べないので音声は出さない（ADR-0004）。
- 時刻: WebRTC の時刻は RTCP SR で壁時計に合わせ直されて飛ぶので、受け取る側で滑らかな時間軸（µs）に付け直す（映像と音声で共通の基準）。
- **HLS へ出すため、WHIP の送り手には 2 秒ごとにキーフレームを求める**（PLI）。ブラウザーはキーフレームをほとんど送らないため。欠落の後も求める。
- WHEP は、つながる前に届いた直近の GOP をためて、つながった時点で送る。H.264 の SPS/PPS は設定から毎回渡す。

## 検証

- `tools/interop/webrtc.sh`（部品と Chrome 156: WHIP・WHEP・サイマルキャスト・DataChannel・損失時の NACK/RTX）
- `tools/interop/node_webrtc.sh`（ノードと Chrome 156: WHIP → HLS、RTMP → WHEP、鍵・署名の誤り）

## 未了

- Firefox・Safari、TURN を使う経路、IPv6・NAT（AC-07）。
- 部屋（Room）の 1 対 1・多対多の API（SFU の部品はある）。WHEP でのサイマルキャストの層の選択。
- DataChannel での字幕（R6-4）。
