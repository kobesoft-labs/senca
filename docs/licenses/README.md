# ライセンス・特許の台帳

ADR-0004・REQ-007・仕様 9.5 の台帳。**法務確認の対象**であり、ここに書いたことは法的な助言ではない。
一次資料は取得日を書き、変わりうるものは採用時に版を固定する。

## 製品のソースコード

- SENCA: Apache-2.0。
- Rust の依存: ADR-0002 の一覧（MIT / Apache-2.0 / BSD / ISC）。配布物の SBOM と依存のライセンスの照合は M14 のリリース手順で行う。

## 管理画面に同梱するもの（apps/console）

| もの | 用途 | ライセンス | 入手 | 備考 |
|---|---|---|---|---|
| Lucide のアイコンの形（lucide-static 1.67.0 の一部、97 個） | 管理画面のアイコン（`apps/console/js/lib/icons.js` に SVG の形を写して同梱） | ISC（Feather 由来のものは MIT も） | npm の `lucide-static`（取得日 2026-11-03） | 全文は [lucide-LICENSE.txt](lucide-LICENSE.txt)。ほかに外部のフォント・CDN は使わない |

## 実行時に読み込む外部コーデック（製品に同梱しない・codec worker が dlopen）

| ライブラリ | 用途 | ライセンス | 入手 | 備考 |
|---|---|---|---|---|
| NVIDIA Video Codec SDK（libnvidia-encode・libnvcuvid・libcuda） | H.264/H.265/AV1 の符号化、H.264/H.265/AV1/VP9 の復号 | NVIDIA のドライバーの使用許諾 | ホストのドライバー | ヘッダーは nv-codec-headers（MIT）から生成したバインディングを同梱 |
| Intel oneVPL（libvpl） | 同上（Quick Sync） | MIT（dispatcher）、実装は Intel のドライバー | ホスト | |
| AMD AMF（libamfrt64） | 同上 | MIT（ヘッダー）、実装は AMD のドライバー | ホスト | |
| VA-API（libva） | 復号（H.264） | MIT | ホスト | 符号化は未実装 |
| dav1d | AV1 復号 | BSD-2-Clause | ディストリビューションのパッケージ | |
| SVT-AV1 | AV1 符号化 | BSD-3-Clause＋AOM の特許許諾 | 同上 | |
| libvpx | VP8/VP9 | BSD-3-Clause＋WebM の特許許諾 | 同上 | |
| libopus | Opus | BSD-3-Clause＋特許許諾（IETF の宣言） | 同上 | |

**同梱・読み込みしないもの**: libavcodec、OpenH264、x264、x265、fdk-aac（H.264/H.265/AAC のソフトウェア実装。ADR-0004）。

## 方式の特許（法務確認の一覧）

| 方式 | 状況 | SENCA の扱い | 確認事項 |
|---|---|---|---|
| H.264 / H.265 | 特許プール（Via LA の AVC/HEVC、Access Advance など） | 符号化・復号はハードウェアだけ。ビットストリームの解析・形式変換・パケット化は自前 | ハードウェアの購入で済まない配信・コンテンツの料金の有無（プールの条件、提供地域） |
| AAC | 特許プール（Via LA） | パススルーだけ（符号化・復号しない） | パススルーの扱い |
| AV1 / VP8 / VP9 / Opus | 開発元はロイヤリティフリーとして提供。第三者のプール（Sisvel）が権利を主張 | REQ-007 の「ロイヤリティフリーとして提供されている技術」として扱う。「特許がない」とは表示しない | 製品の表示と提供地域 |
| JPEG（基本方式）・PNG | 基本方式は失効済み・無料とされる | 自前の符号化・復号 | なし（念のため確認） |

## 一次資料（取得日 2026-11-02、版は採用時に固定）

- AOMedia Patent License 1.0: https://aomedia.org/license/patent-license/
- WebM（VP8/VP9）の特許許諾: https://www.webmproject.org/license/additional/
- Opus のライセンス: https://opus-codec.org/license/
- Via LA の AVC/H.264・HEVC/H.265・AAC のプログラム: https://www.via-la.com/
- Access Advance の HEVC Advance: https://accessadvance.com/
- NVIDIA Video Codec SDK のライセンス: https://developer.nvidia.com/video-codec-sdk
