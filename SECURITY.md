# Security policy

English | [日本語](#日本語)

## Supported versions

SENCA has not had a stable release yet. The current version is the pre-release **v0.0.0**. The checks done before a release are
described in [docs/dev/RELEASE.en.md](docs/dev/RELEASE.en.md).

| Version | Supported |
|---|---|
| v0.0.0 (pre-release, `main`) | Best effort |

Security fixes are made on the latest code of the default branch on a best-effort basis. There are no backports and no guaranteed response times.

## Reporting a vulnerability

**Please do not report vulnerabilities in public issues, pull requests or discussions.**

Report them privately through GitHub's private vulnerability reporting for this repository:

1. Open <https://github.com/kobesoft-labs/senca/security>.
2. Choose **Report a vulnerability** (direct link: <https://github.com/kobesoft-labs/senca/security/advisories/new>).
3. Fill in the form. The report is visible only to you and the maintainers.

### What to include

- The affected component (crate, protocol, API endpoint or app) and the commit you tested.
- The type of issue (for example memory exhaustion, panic or hang on crafted input, authentication or authorization bypass, secret leakage,
  cryptographic or protocol flaw) and its impact as you understand it.
- Steps to reproduce: a minimal input, packet capture or script, the configuration and command line used, and the observed and expected behaviour.
- Whether the issue is reachable without authentication, and from which network position.
- Any suggested fix or mitigation, and whether you plan to disclose it publicly and when.

Do not include real stream keys, tokens or other secrets in the report; replace them with placeholders.

We will acknowledge the report, work with you on the assessment and a fix, and coordinate the timing of public disclosure. Because the
project is pre-release, handling is best effort.

## Scope highlights

- **In-house protocol implementations.** TLS, DTLS, SRTP, SRT encryption and QUIC (handshakes and record layers) are implemented in this
  repository. Only the cryptographic primitives (AES, SHA, HMAC, HKDF, signatures, key exchange, random numbers) come from vetted libraries
  (ADR-0002). **These implementations have not yet been independently reviewed** (gate G8 in [docs/dev/RELEASE.en.md](docs/dev/RELEASE.en.md)), and
  reports about them are especially welcome.
- **Authorization**: JWTs, signed playback URLs, API tokens and room join tokens.
- **Cluster ownership**: mTLS between nodes and stream ownership with fencing.
- **Parsers** for untrusted input (RTMP/AMF, FLV, MPEG-TS, fMP4, HLS, HTTP, SDP, STUN, RTP/RTCP, RTSP, SRT, codec bitstreams, images):
  panics, infinite loops, unbounded allocation and similar resource exhaustion are treated as vulnerabilities (see the strict-parsing rule in
  [docs/dev/DEVELOPMENT.en.md](docs/dev/DEVELOPMENT.en.md)).
- **Secret handling**: stream keys and tokens appearing in logs, errors or debug output.
- **Codec workers** that load external codec libraries through FFI and their sandbox.

Out of scope: vulnerabilities in third-party components themselves (please report those upstream; tell us if SENCA's use of them makes
things worse), and the test tools under `tools/` that are used only against local test peers.

---

## 日本語

[English](#security-policy) | 日本語

### サポート対象のバージョン

SENCA にはまだ安定版がありません。現在のバージョンは先行公開版の **v0.0.0** です。リリース前に行う確認は
[docs/dev/RELEASE.md](docs/dev/RELEASE.md) に記載しています。

| バージョン | サポート |
|---|---|
| v0.0.0（先行公開版、`main`） | ベストエフォート |

セキュリティの修正は、既定のブランチの最新コードに対してベストエフォートで行います。過去のバージョンへのバックポートや、
応答期限の保証はありません。

### 脆弱性の報告

**脆弱性は、公開の issue・pull request・discussion に書かないでください。**

このリポジトリの GitHub の private vulnerability reporting を使って、非公開で報告してください。

1. <https://github.com/kobesoft-labs/senca/security> を開きます。
2. **Report a vulnerability** を選びます（直接のリンク: <https://github.com/kobesoft-labs/senca/security/advisories/new>）。
3. フォームに記入します。報告の内容は、報告者とメンテナーだけが閲覧できます。

#### 報告に含めてほしい情報

- 影響を受けるコンポーネント（crate・プロトコル・API のエンドポイント・アプリ）と、確認に使ったコミット。
- 問題の種類（細工した入力によるメモリーの枯渇・パニック・ハング、認証・認可の回避、秘密情報の漏えい、暗号・プロトコルの欠陥など）と、
  把握している範囲での影響。
- 再現手順: 最小限の入力・パケットキャプチャ・スクリプト、使用した設定とコマンドライン、実際の動作と期待した動作。
- 認証なしで到達できるか、どのネットワーク上の位置から到達できるか。
- 修正や緩和策の案。公開する予定があれば、その時期。

実際のストリームキーやトークンなどの秘密情報は報告に含めず、ダミーの値に置き換えてください。

報告を受け取ったら受領をお知らせし、報告者と協力して評価と修正を進め、公開の時期を調整します。先行公開の段階のため、対応は
ベストエフォートです。

### 特に重点を置く範囲

- **独自に実装したプロトコル。** TLS・DTLS・SRTP・SRT の暗号化・QUIC（ハンドシェイクとレコード層）は、このリポジトリで実装しています。
  検証済みのライブラリを使うのは暗号プリミティブ（AES・SHA・HMAC・HKDF・署名・鍵交換・乱数）だけです（ADR-0002）。**これらの実装は
  まだ独立したレビューを受けていません**（[docs/dev/RELEASE.md](docs/dev/RELEASE.md) の G8）。これらに関する報告は特に歓迎します。
- **認可**: JWT・署名付きの再生 URL・API トークン・部屋の参加トークン。
- **クラスターの所有権**: ノード間の mTLS、fencing 付きのストリームの所有権。
- 信頼できない入力を扱う**パーサー**（RTMP/AMF・FLV・MPEG-TS・fMP4・HLS・HTTP・SDP・STUN・RTP/RTCP・RTSP・SRT・コーデックの
  ビットストリーム・画像）: パニック・無限ループ・無制限のメモリー確保などの資源の枯渇は、脆弱性として扱います
  （[docs/dev/DEVELOPMENT.md](docs/dev/DEVELOPMENT.md) の「厳格に拒否する」を参照）。
- **秘密情報の扱い**: ストリームキーやトークンがログ・エラー・デバッグ出力に現れること。
- 外部のコーデックライブラリを FFI で読み込む **codec worker** と、そのサンドボックス。

対象外: サードパーティのコンポーネント自体の脆弱性（上流へ報告してください。SENCA の使い方によって影響が大きくなる場合は、
お知らせください）、およびローカルのテスト相手に対してのみ使う `tools/` のテストツール。
