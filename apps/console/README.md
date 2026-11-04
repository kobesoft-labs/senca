# SENCA 管理画面（apps/console）

公開の管理 API（`/api/v1`、`docs/api/openapi.yaml`）だけを使う単一ページアプリ（REQ-002）。
依存パッケージもビルドも無い（素の ES modules と CSS）。ノードが `--console-dir apps/console` で `/console/` に配る。

```
senca-node --console-dir apps/console ...   # → http://<node>/console/
tools/interop/console.sh                    # 配信の形・構文・ロジック（辞書・書式・検索・色）・ヘッドレス Chrome での操作と再生
```

ログインは API トークン（初回は `{data-dir}/control/bootstrap-admin-token`）。トークンは sessionStorage にだけ置く。
部品の見本は **`#/_styleguide`**（メニューには出さない。コマンドパレットの「スタイルガイド」からも開ける）。

## 構成

| 場所 | 中身 |
|---|---|
| `index.html` | 外枠。CSS を 5 つ、`js/boot.js`（最初の描画の前に明暗と言語を `<html>` に入れる）、`js/main.js`。インラインのスクリプト・スタイルは無い（CSP） |
| `css/tokens.css` | デザイントークン（色・文字・間隔・角丸・影・動き・フォーカス）。値はここだけに置く |
| `css/base.css`・`css/components.css`・`css/shell.css`・`css/views.css` | 土台と補助クラス・部品・外枠・画面ごとの見た目（再生・パイプライン・スイッチャー・スタイルガイド） |
| `js/main.js` | 経路の切り替え、ログイン状態、SSE の開始、言語を変えたときの作り直し |
| `js/nav.js` | 画面の一覧（経路・メニューのグループ・アイコン・`g` のショートカットのキー・準備中） |
| `js/shell.js` | 左のメニューと上の帯（パンくず・検索・接続の状態・言語・明暗・アカウント） |
| `js/palette.js`・`js/shortcuts.js`・`js/lib/fuzzy.js` | コマンドパレット、キーボードのショートカット、あいまい検索 |
| `js/i18n/` | `t()`・`tn()` と辞書（`ja/*.js`・`en/*.js`、画面ごとのファイル） |
| `js/ui/` | 部品（下の一覧）。入口は `js/ui/index.js` |
| `js/lib/` | `dom.js`（`h()` など）・`icons.js`・`format.js`（Intl の書式）・`theme.js`・`ui.js`（エラーの表示・`load()`）・`recent.js`・`guide.js`・`bus.js`・`live.js` など |
| `js/api/` | `client.js`（fetch の包み。problem+json を `ApiError` に、401 はログインへ）・`resources.js`・`sse.js`・`pipelines.js` |
| `js/player/` | MSE の HLS 再生（fMP4 だけ）: `m3u8.js`・`mp4.js`・`hls.js`、WebRTC の再生 `whep.js`、`widget.js`（自動 = WHEP → だめなら HLS） |
| `js/pipeline/model.js` | senca-pipeline のグラフ JSON と編集用のモデルの行き来・ポートの型・検査。画面に依存しない |
| `js/views/` | 画面。`render(main, params, ctx)` を持ち、片付けの関数を返してよい（`ctx = { caps, navigate, setCrumbs, nav }`）。準備中のページは `coming-soon.js` |

## デザインの決まり

- **色**: 落ち着いた中立の灰色＋アクセント 1 色（`--accent`、青）。**赤（`--live`・`--danger*`）は ON AIR・録画中と、取り消せない操作（削除など）にだけ使う**。
  意味の色は `ok`（緑）・`warn`（琥珀）・`danger`（赤）・`info`（青緑）で、それぞれ文字（`--*-text`）・淡い背景（`--*-soft`）・線や点（`--*`）がある。
  文字と背景の組はすべて WCAG AA（本文 4.5:1、アイコン・枠 3:1）。`console.sh` の C がトークンから計算して確かめる。色を足したら組も足す。
- **明暗**: `<html data-theme="light|dark">`（解決した値）と `data-theme-pref="system|light|dark"`。選択は localStorage（`senca.console.theme`）。
  CSS は `data-theme` だけを見る。部分に `data-theme` を付けるとその中だけ切り替わる（スタイルガイドの左右の比較）。
- **文字**: `--fs-2xs`（11px）〜`--fs-3xl`（28px）、本文は `--fs-md`（14px）。数は `font-variant-numeric: tabular-nums`（`.num`）。日本語は `word-break: auto-phrase`。
- **間隔**: 4px の倍数（`--sp-1`〜`--sp-12`）。角丸 `--r-xs`〜`--r-xl`、影 `--shadow-1`〜`3`。動き `--dur-fast`・`--dur`・`--dur-slow`（`prefers-reduced-motion` で 0）。
- **フォーカス**: `:focus-visible` で 2px の `--focus` の輪。入力欄は枠の色と淡い輪。消さない。
- **スタイル**: HTML の文字列で `style=` を書かない（`console.sh` の B が調べる）。どうしても要るときは JS で `el.style.x = …`。新しい見た目は CSS のクラスにする。
- **狭い画面（768px 以下）**: 左のメニューは引き出し、上の帯は縮む、押せるものは 44px 以上、表はカードの並び、ページは横にはみ出さない（`console.sh` の D が 390×844 で調べる）。

## 部品（`js/ui/`）

`import { button, pageHeader, … } from '../ui/index.js'`。どれも `h()` で作った要素を返す。

| 部品 | 使い方の要点 |
|---|---|
| `button(label, {variant, size, icon, iconRight, iconOnly, kbd, onClick, testid, attrs})` | variant: `primary`（1 画面に 1 つ）・`secondary`（既定）・`ghost`・`danger`（赤い文字）・`danger-solid`（確認ダイアログの実行だけ）・`link`。onClick が Promise を返すと終わるまで読み込み中 |
| `iconButton(icon, label, opts)` | アイコンだけ。label は読み上げとツールチップになる（必須） |
| `setLoading(btn, on)` | 読み込み中の表示 |
| `badge(text, kind, {dot, icon})` | kind: `ok`・`warn`・`danger`・`info`・`accent`・`outline`・`live`・`rec` |
| `statusPill(state)` | 状態の決まった色と言葉（`status.<state>`）。`live`（ON AIR）・`recording`・`waiting`・`stopped`・`running`・`failed` など（`STATUS_KINDS`） |
| `alert(kind, body, {title, actions, small})` | `info`・`ok`・`warn`・`error`。アイコン付き |
| `emptyState({icon, title, body, actions, compact})` | 何も無い理由と、次にする 1 つの操作 |
| `skeleton({kind, rows})`・`loadingLine()` | 読み込み中。`lib/ui.js` の `load(container, fn, {kind})` は最初だけスケルトンを出し、2 回目からは前の中身のまま読み直す |
| `meter({label, value, max, kind, valueText})` | 量のメーター（`value: null` で途中が分からない動き） |
| `pageHeader({title, description, actions, badges, crumbs, help})` | ページの見出し（h1）。`crumbs` は上の帯のパンくずになり、狭い画面では「← 一覧」のリンクになる |
| `panel({title, icon, description, actions, help, flush}, ...children)` | 節。`aria-labelledby` を自動で付ける |
| `statTile({label, value, sub, icon, href, kind})`・`kvList(pairs)` | 統計のタイル、項目と値 |
| `dataTable({columns, rows, empty, caption, sort, rowKey, testid})` | 列: `{key, label, render, sort, class, primary, actions, wrap}`。見出しで並べ替え（`aria-sort`）。狭い画面ではカード（`primary` の列が題）。`el.update(rows)` |
| `pagination({page, hasPrev, hasNext, onPrev, onNext})` | カーソルのページ送り |
| `tabs({tabs, label, param})` | WAI-ARIA のタブ。選んだタブは `?tab=` に入る（作り直さない） |
| `modal(title, build, {actions, wide, icon, kind, testid})` | `<dialog>` の showModal（フォーカスは中に閉じる、Escape・×・背景で閉じる、開いた要素へ戻る） |
| `confirmDialog(title, message, {ok, danger, consequences, requireText})` | message に「何が起きるか」を書く。取り消せない削除は `requireText`（名前を入れるまで押せない） |
| `toast(msg, kind, {duration, action})` | `action: {label, onClick}` で「元に戻す」「再試行」。`reportError(e, what, {retry})` は失敗を知らせる |
| `attachMenu(button, items)`・`tooltip(el, text, {kbd})`・`attachPopover(button, content)` | メニュー（↑↓・Escape）、ツールチップ、説明の箱。ダイアログの中でも使える（top layer） |
| `helpHint({title, body, guide})` | 「?」の説明。`guide` は手引きのページ（`'04-ingest-playback.md'`）。言語に合わせて `docs/guide/{ja,en}/` を開く |
| `formField({label, input, hint, required, optional, error, help})` | ラベル・説明・必須（見た目だけ。読み上げは `aria-required`）・その場の検査（`el.setError(msg)`） |
| `toggle({label, description, checked, onChange})` | すぐ効く設定（`role="switch"`）。送信して効くものはチェックボックス |
| `segmented({options, value, onChange, label})` | 2〜5 個から 1 つ（`radiogroup`） |
| `searchBox({placeholder, onInput, shortcut})` | `shortcut: true` で `/` で入れるページの検索になる |
| `copyField(value, label, {secret})` | 写すボタン付き。`secret` は伏せて表示し、目のボタンで見せる |
| `checklist({id, title, steps})` | はじめの手順。`steps[].done` は実際の状態から計算して渡す。閉じたらこのブラウザでは出さない |

`lib/dom.js` の `table()`・`field()`・`kv()`・`badge()` は同じ見た目の簡単な版（並べ替え・必須の印は無い）。

## アイコン（`js/lib/icons.js`）

`icon(name, {label, size, spin})`。24px の格子・線 1.85・`currentColor`。`label` を渡すと `role="img"`、無ければ `aria-hidden`。
形は Lucide（ISC。`docs/licenses/README.md` と `docs/licenses/lucide-LICENSE.txt`）から写した。名前の一覧はスタイルガイドにある。
足すときは Lucide の同じ名前の SVG から `PATHS` に 1 行足す（外から読み込まない）。

## 言葉（`js/i18n/`）

- `t('streams.create')`、置き換えは `t('copy.done', { label })`（`{label}`）。英語の単数・複数は `tn('key', n)`（`key.one`・`key.other`）。
- 辞書は画面ごとのファイル（`ja/streams.js` と `en/streams.js`）。鍵は画面の名前で始める。共通の言葉は `core.js`（`common.*`・`status.*`・`errors.*` など）、外枠は `shell.js`（`nav.*`・`palette.*`・`login.*`）。
  画面を足すときは ja と en に同じ名前のファイルを足し、両方の `index.js` に 1 行足す。
- ja と en の鍵・`{params}` が同じこと、ソースの `t('…')` の鍵がすべてあることを `console.sh` の C が調べる（鍵は文字列のまま書く。組み立てた鍵は調べられないので、使うときは辞書にすべて置く）。
  実行中に見つからなかった鍵は `window.__sencaI18nMissing()` に溜まり、D が空であることを確かめる。
- 既定は `navigator.language`（ja で始まれば日本語、ほかは英語）。切り替えは上の帯・ログイン画面・コマンドパレットで、localStorage（`senca.console.lang`）に残る。切り替えると外枠と今の画面を作り直す。
- 日付・数・長さ・大きさは `lib/format.js`（`fmtTime`・`fmtDate`・`fmtClock`・`fmtRelative`・`fmtNumber`・`fmtPercent`・`fmtBytes`・`fmtBps`・`fmtDuration`・`fmtDurationText`・`weekdayNames`）。`toLocaleString('ja-JP')` などを直に使わない。
- 日本語はです・ます（文のとき）、一般的な用語（「コピー」「クラスター」など）。英語も自然に。API のエラーは `describeError()` が言語に合わせた前置き＋ノードの説明（problem の title、ノードの言葉のまま）にする。

## 画面を作るときの決まり

- 経路は `js/nav.js` の `ROUTES` と `NAV_GROUPS` に足す（`key` は `g` の次のキー、`soon: true` は準備中のページ）。メニューの名前は `nav.<id>`、グループは `navGroup.<id>`。
- ページの最初は `pageHeader()`（h1 は 1 つ）。詳細ページは `crumbs: [{ label: t('nav.streams'), href: '#/streams' }, { label: name }]` を渡し、`rememberRecent({kind, id, label, href})` で最近開いたものに入れる（コマンドパレットに出る）。
- 一覧が空なら `emptyState()` で理由と主の操作を出す。作成は `#/<一覧>?new=1` で開けるようにし（コマンドパレットの操作が使う）、一覧では `onPageKey('n', …)`（`js/shortcuts.js`）で N に割り当てる。
- 削除などの確認は結果を書き（`consequences`）、取り返しのつかないものは `requireText`。
- はじめての人が迷う項目には `helpHint()`。手引きにある言葉と同じ言葉を使う。
- 試験で選ぶ要素には `data-testid`（`testid`）を付ける。見える文字や役割で選ぶ試験は、言語を変えると壊れるので増やさない。
- 1 画面の primary のボタンは 1 つ。アイコンだけのボタンには必ず label（ツールチップと読み上げ）。
- ショートカットは入力中・ダイアログ中は効かない（`isTyping()`）。Ctrl/⌘+K（パレット）・`?`（一覧）・`/`（ページの検索）・`g` → キー（移動）・`[`（メニューを畳む）。

## 再生（`js/player/hls.js`）の決まりと限界

- fMP4 の HLS だけ。初期化セグメントからコーデック文字列を作り、SourceBuffer に順に追加する。
- ライブは端の 3 本前から始め、遅れすぎたら端へ寄せる。不連続（送り手の再接続）は `timestampOffset` でつなぐ。
- 幅・高さが 0 の AV1 の初期化セグメント（今のノードの Enhanced RTMP 由来）は、キーフレームの sequence header から大きさを読んで補正する。
- MPEG-TS は扱わない（Safari のネイティブ再生へ。ほかのブラウザは URL を出して VLC などを案内）。
- LL-HLS の part・ABR（複数の variant）・暗号化・字幕は使わない。

## パイプラインとスイッチャー

- グラフは `crates/senca-pipeline/src/model.rs` の JSON（知らない項目は拒否される）。部品の位置はグラフに入れられないので、
  このブラウザの localStorage（`senca.console.pipeline-layout.v1`、パイプラインの id ごと）に置く。保存前の下書きも localStorage。
- 保存 = `POST /pipelines`（止めた状態で作る）か `PATCH`（If-Match）。検証 = `POST /pipelines/{id}/validate` に編集中のグラフを送る
  （保存前のグラフも検証できる）。問題は problem+json の `details.issues`（`{code, node, message}`）を一覧にし、部品を赤くする。
- 開始・停止 = `PATCH {desired_state}`。スイッチャーは CUT・TAKE・FADE を `take`（トランジション付き）、シーンのクリックを
  `set_preview`、ダブルクリックを `switch_program` に。T バーは途中の位置を送れないので、端まで動かした時間のトランジションで take。
- ノードは status（`event: status`）とメーター（`event: meters`）を同じ `status` 欄に入れることがあるので、`event` で読み分ける。

## WebRTC

- 再生（WHEP）: `playback-url` の `playback.whep`（署名付き）へ offer を POST、201 の answer を入れる。止めると Location を DELETE。
  配信前・運べないコーデック（AAC だけ・H.265）は 422 → 自動なら HLS に戻す。
- 送信（WHIP、`#/send`）: カメラ・マイクか画面 → `ingest.whip_url` へ Bearer の stream key で POST。鍵は保存しない。
