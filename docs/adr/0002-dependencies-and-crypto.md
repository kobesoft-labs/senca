# ADR-0002 依存と暗号の部品

- 状態: 採用（2026-11-02）

## 決定

本番の crate が使ってよい外部依存は次のとおり。足すときはこの表を更新する。

| 依存 | 用途 | ライセンス |
|---|---|---|
| bytes | 参照カウント付きバッファ | MIT |
| tokio | 非同期 I/O（実行ファイルだけ） | MIT |
| serde / serde_json | 制御 API・設定・索引の JSON | MIT/Apache-2.0 |
| getrandom | OS の乱数源 | MIT/Apache-2.0 |
| sha1 / sha2 / hmac / hkdf / md-5 | ハッシュ・MAC・鍵導出（RustCrypto）。hkdf はノードでも、クラスタの合言葉からノード間の認証の鍵と秘密値の鍵を包む鍵を導くのに使う。MD5 は STUN/TURN の長期資格情報の鍵（RFC 8489）と RTSP の Digest 認証だけ | MIT/Apache-2.0 |
| aes / ctr / aes-gcm / chacha20poly1305 / chacha20 | 共通鍵暗号（RustCrypto）。chacha20 は QUIC の ChaCha20 header protection（RFC 9001 5.4.4）の鍵ストリームだけ | MIT/Apache-2.0 |
| p256 / x25519-dalek / ed25519-dalek / rsa | 署名・鍵交換（RustCrypto・dalek）。JWT は ed25519-dalek 2・p256 0.13・rsa 0.9 | MIT/Apache-2.0、BSD-3 |
| rustls-webpki / rustls-pki-types | X.509 の証明書パス検証（仕様 2.1 で外部利用可）。署名の検証は RustCrypto で行う（ring・aws-lc は使わない） | ISC、MIT/Apache-2.0 |
| p384 | P-384 の証明書チェーンの検証（Let's Encrypt などの中間証明書） | MIT/Apache-2.0 |
| x509-cert / der | 自己署名証明書の生成（DTLS）と証明書の解析（RustCrypto） | MIT/Apache-2.0 |
| pbkdf2 / aes-kw | SRT の鍵導出（PBKDF2-HMAC-SHA1）と鍵の包み（RFC 3394） | MIT/Apache-2.0 |
| zeroize | 鍵の消去 | MIT/Apache-2.0 |
| libloading | 外部コーデック・ハードウェア SDK の動的読み込み（codec worker だけ） | ISC |
| libc | codec worker のサンドボックス（prctl・setrlimit・seccomp・Landlock） | MIT/Apache-2.0 |
| base64 / subtle | トークン・署名の符号化、定数時間比較 | MIT/Apache-2.0、BSD-3 |

暗号の演算（AES、ChaCha20、SHA、HMAC、HKDF、ECDSA/EdDSA/RSA、ECDH、乱数）は上の検証済みライブラリを使い、自分で書かない（仕様 2.2）。TLS 1.3・DTLS 1.2・SRTP・QUIC のハンドシェイクと記録層・鍵スケジュールは自前（仕様 2.1）で、公開前に独立監査を受ける（仕様 15.3）。

試験だけで使う依存（dev-dependencies）は制限しない。FFmpeg・ブラウザーは試験の相手としてだけ使い、製品の配送経路に入れない。
