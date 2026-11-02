# ADR-0007 入力・出力・秘密値と、外へ出る接続の制限

- 状態: 採用（2026-11-02）
- 関係: 仕様 6.6・6.8・9.3・9.4・10.2、REQ-001、REQ-006

## 決定

- **入力（pull）と出力（push）を API の資源にする。** `POST /streams/{id}/inputs`（`rtmp_pull`・`rtsp_pull`・`srt_pull`・`hls_pull`）、
  `POST /streams/{id}/outputs`（`rtmp_push`・`srt_push`・`whip_push`・`paas`）。push で受ける入力（RTMP・SRT・WHIP）は stream の publish 鍵で受けるので資源にしない。
- **ノードの係が定義と実体を突き合わせる。** 定義（制御状態）の世代が変われば止めて起動し直す。失敗は指数バックオフ（1〜60 秒、揺らぎ付き）でつなぎ直す。状態（`connecting`・`active`・`backoff`・`waiting_input`・`ended`・`error`）と最後のエラーを `status` に出す。
- **pull の入力は内部の送り手として stream の hub につなぐ。** publish 鍵は使わない。同じ stream に別の送り手がいれば `Busy`。
- **VOD（ENDLIST）の HLS pull は読み終えたら止める**（繰り返さない）。
- **秘密値**（`POST /secrets`、値は書くだけ）。`credential_ref: "secret:{name}"` で参照する。値はノードの鍵（`control/secret-key`、0600）で AES-256-GCM により封じ、AAD に `tenant/id` を入れる（別の資源に写しても開けない）。制御状態・ログ・バックアップには封じた形だけが残る。使用中の秘密値は消せない（409）。
- **URL に資格情報を入れさせない**（`user:pass@` は 400）。鍵・パスワードは `credential_ref` で渡す。

## 外へ出る接続の制限（SSRF、仕様 9.4）

- 名前を解決した**後の IP** を検査する（DNS の再解決で私設アドレスへ向けられても拒否）。
- 既定で拒否: ループバック、リンクローカル（169.254.0.0/16。クラウドのメタデータを含む）、未指定、マルチキャスト、ブロードキャスト、私設（10/8・172.16/12・192.168/16・100.64/10・fc00::/7・fe80::/10）、IPv4 射影の IPv6 も同じ扱い。
- 社内のカメラなどへつなぐときは、管理者が `--egress-allow CIDR` で範囲を許す（API の利用者には許可させない）。

## SRT の受け口

- Stream ID: `publish:{stream_key}` または `#!::r={stream_key},m=publish`。再生は `play:{stream_id}?exp=..&sig=..`（署名付きの再生 URL と同じ署名）。
- パスフレーズはテナントの秘密値 `srt-passphrase`。あれば必須（平文の接続は拒否。黙って降格しない、仕様 6.8）。
- 受け口全体を止めないため、接続ごとのキューはあふれたら落とす（TS の連続性カウンターで不連続として扱われる）。

## 相互接続で分かったこと

- FFmpeg の listen モードの RTMP サーバーは、createStream で返した番号と違う stream ID で映像を送る。play では 1 本しか開かないので、どの ID でも受け取る（senca-rtmp）。
- libsrt は閉じるときに送り残しを捨てる。末尾の数フレームが欠けるのは相手の動きで、試験では末尾を比べない。
