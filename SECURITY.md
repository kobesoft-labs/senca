# Security policy

English | [日本語](#日本語)

## Supported versions

SENCA has not had a stable release yet. The only version is the pre-release **v0.0.0**, which is **experimental**: the release gate
(24-hour fuzzing, 72-hour soak test, independent review of the cryptographic and authorization code, signed reproducible builds; see
[docs/RELEASE.en.md](docs/RELEASE.en.md)) has not been passed, and it should not be used in production.

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
  (ADR-0002). **These implementations have not yet been independently reviewed** (gate G8 in [docs/RELEASE.en.md](docs/RELEASE.en.md)), and
  reports about them are especially welcome.
- **Authorization**: JWTs, signed playback URLs, API tokens and room join tokens.
- **Cluster ownership**: mTLS between nodes and stream ownership with fencing.
- **Parsers** for untrusted input (RTMP/AMF, FLV, MPEG-TS, fMP4, HLS, HTTP, SDP, STUN, RTP/RTCP, RTSP, SRT, codec bitstreams, images):
  panics, infinite loops, unbounded allocation and similar resource exhaustion are treated as vulnerabilities (see the strict-parsing rule in
  [docs/DEVELOPMENT.en.md](docs/DEVELOPMENT.en.md)).
- **Secret handling**: stream keys and tokens appearing in logs, errors or debug output.
- **Codec workers** that load external codec libraries through FFI and their sandbox.

Out of scope: vulnerabilities in third-party components themselves (please report those upstream; tell us if SENCA's use of them makes
things worse), the test tools under `tools/` used only against local test peers, and deployments that ignore the experimental status.

---

## 日本語

[English](#security-policy) | 日本語

### 対応する版

SENCA にはまだ安定版が無い。版は先行公開の **v0.0.0** だけで、**experimental**。出荷 gate（24 時間の fuzz、72 時間の長時間試験、
暗号・認可のコードの独立レビュー、署名付きの再現可能な build。[docs/RELEASE.md](docs/RELEASE.md)）を通っておらず、本番で使わないこと。

| 版 | 対応 |
|---|---|
| v0.0.0（先行公開、`main`） | できる範囲で（best effort） |

セキュリティの修正は既定のブランチの最新のコードに、できる範囲で入れる。過去の版への移植や、応答の期限の保証は無い。

### 脆弱性の報告

**脆弱性は公開の issue・pull request・discussion に書かないこと。**

このリポジトリの GitHub の private vulnerability reporting で、非公開で報告する:

1. <https://github.com/kobesoft-labs/senca/security> を開く。
2. **Report a vulnerability** を選ぶ（直接のリンク: <https://github.com/kobesoft-labs/senca/security/advisories/new>）。
3. フォームを埋める。報告は報告者とメンテナーにだけ見える。

#### 書いてほしいこと

- 影響のある部品（crate・プロトコル・API の endpoint・アプリ）と、試した commit。
- 問題の種類（細工した入力でのメモリーの枯渇・パニック・停止、認証・認可の回避、秘密の漏れ、暗号・プロトコルの欠陥など）と、分かる範囲の影響。
- 再現の手順: 最小の入力・パケットキャプチャ・スクリプト、使った設定とコマンドライン、起きたことと期待したこと。
- 認証なしで届くか、どのネットワークの位置から届くか。
- 修正・緩和の案があれば。公開する予定があれば、その時期。

本物の stream key・トークンなどの秘密は報告に入れず、置き換えた値にすること。

報告を受け取ったら知らせ、評価と修正を報告者と一緒に進め、公開の時期を調整する。先行公開の段階なので、対応はできる範囲で行う。

### 特に見てほしいところ

- **自前のプロトコルの実装。** TLS・DTLS・SRTP・SRT の暗号・QUIC（ハンドシェイクと記録層）はこのリポジトリの実装。暗号の部品（AES・SHA・HMAC・
  HKDF・署名・鍵交換・乱数）だけが検証済みのライブラリ（ADR-0002）。**これらの実装はまだ独立したレビューを受けていない**
  （[docs/RELEASE.md](docs/RELEASE.md) の gate G8）。これらについての報告は特に歓迎する。
- **認可**: JWT・署名付きの再生 URL・API トークン・部屋の参加トークン。
- **クラスタの所有権**: ノード間の mTLS、fencing 付きの stream の所有。
- 信頼できない入力の**解析器**（RTMP/AMF・FLV・MPEG-TS・fMP4・HLS・HTTP・SDP・STUN・RTP/RTCP・RTSP・SRT・コーデックのビットストリーム・画像）:
  パニック・無限ループ・無制限の確保などの資源の枯渇は脆弱性として扱う（[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) の「厳格に拒否する」）。
- **秘密の扱い**: stream key・トークンがログ・エラー・デバッグ表示に出ること。
- 外部のコーデックのライブラリを FFI で読み込む **codec worker** とそのサンドボックス。

対象外: 第三者の部品そのものの脆弱性（上流へ報告してほしい。SENCA の使い方で悪くなるなら知らせてほしい）、手元の試験相手にだけ使う `tools/` の
試験の道具、experimental であることを無視した運用。
