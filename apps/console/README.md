# SENCA 管理画面（apps/console）

公開の管理 API（`/api/v1`、`docs/api/openapi.yaml`）だけを使う単一ページアプリ（REQ-002）。
依存パッケージもビルドも無い（素の ES modules と CSS）。ノードが `--console-dir apps/console` で `/console/` に配る。

```
senca-node --console-dir apps/console ...   # → http://<node>/console/
tools/interop/console.sh                    # 配信の形・構文・ロジック（辞書・書式・検索・色）・ヘッドレス Chrome での操作と再生
```

ログインはユーザーのアカウント（パスワードと 2 段階認証、OIDC のシングルサインオン）か API トークン・JWT（下の「ログインとユーザー」）。
セッション・トークンの値は sessionStorage にだけ置く。
部品の見本は **`#/_styleguide`**（メニューには出さない。コマンドパレットの「スタイルガイド」からも開ける）。

## 構成

| 場所 | 中身 |
|---|---|
| `index.html` | 外枠。CSS を 10 個、`js/boot.js`（最初の描画の前に明暗と言語を `<html>` に入れる）、`js/main.js`。インラインのスクリプト・スタイルは無い（CSP） |
| `css/tokens.css` | デザイントークン（色・文字・間隔・角丸・影・動き・フォーカス）。値はここだけに置く |
| `css/base.css`・`css/components.css`・`css/shell.css`・`css/views.css`・`css/streaming.css` | 土台と補助クラス・部品・外枠・画面ごとの見た目（再生・スタイルガイド）・配信まわりの画面と、そこで足した部品 |
| `css/pipelines.css` | パイプライン（一覧・ひな形・グラフ・検査器・シーン）とスイッチャー。部品の分類の色 `--k-*` とポートの型の色 `--pt-*` もここで明暗ごとに決める |
| `css/ops.css` | 運用と設定の画面（ダッシュボード・ノードとデバイス・グリッド・監査ログ・API トークン・秘密値）。中身は `.ops-view` で包む（狭い画面では小さいボタンも 44px） |
| `css/realtime.css` | ルーム・メディアパイプラインの画面と、そこで足した部品（折りたたみ・コードの見本・用途のカード・参加者のカード・時系列） |
| `css/auth.css` | ログイン・先に済ませる手続き・アカウント・ユーザーの画面 |
| `js/main.js` | 経路の切り替え、ログインの流れ（/auth/me → pending → 外枠）、入り直し、SSE の開始、言語を変えたときの作り直し |
| `js/nav.js` | 画面の一覧（経路・メニューのグループ・アイコン・`g` のショートカットのキー・準備中） |
| `js/shell.js` | 左のメニューと上の帯（パンくず・検索・接続の状態・言語・明暗・アカウント） |
| `js/palette.js`・`js/shortcuts.js`・`js/lib/fuzzy.js` | コマンドパレット、キーボードのショートカット、あいまい検索 |
| `js/i18n/` | `t()`・`tn()` と辞書（`ja/*.js`・`en/*.js`、画面ごとのファイル） |
| `js/ui/` | 部品（下の一覧）。入口は `js/ui/index.js` |
| `js/lib/` | `dom.js`（`h()` など）・`icons.js`・`format.js`（Intl の書式）・`theme.js`・`ui.js`（エラーの表示・`load()`）・`recent.js`・`guide.js`・`bus.js`・`live.js` など |
| `js/api/` | `client.js`（fetch の包み。problem+json を `ApiError` に、401 は入り直してから送り直す、403 の pending を知らせる）・`auth.js`（/auth・/users）・`resources.js`・`sse.js`・`pipelines.js`・`ops.js`（グリッド・装置の無効化・監査ログと `auditFill()`）・`realtime.js`（ルーム・メディアパイプライン） |
| `js/lib/realtime.js` | ルームとメディアパイプラインの画面に依存しない計算（状態・設定の差分・トラックの要約・SDK のコード・用途 → 作成の本文・作られるもの・出来事の文）。`console.sh` の C で試す |
| `js/vendor/senca-sdk` | `../../../sdk-js/src` へのリンク（ブラウザー SDK をそのまま配る。写しは持たない）。コンテナイメージでは `/opt/senca/sdk-js/src` に置いてリンクを生かす |
| `js/player/` | MSE の HLS 再生（fMP4 だけ）: `m3u8.js`・`mp4.js`・`hls.js`、WebRTC の再生 `whep.js`、`widget.js`（自動 = WHEP → だめなら HLS。`poster` で止まっている間の画、大きな再生ボタン） |
| `js/pipeline/` | `model.js`（グラフ JSON と編集用のモデル・ポートの型・検査・つなげるかの下見・自動の配置・元に戻す用の写し・写しと貼り付け。画面に依存しない）・`templates.js`（ひな形）・`canvas.js`（グラフの面）・`inspector.js`（検査器）・`plan.js`（計画と問題の説明）・`scenes.js`（シーンの編集と配置の絵）・`gallery.js`（ひな形を選ぶダイアログ）・`streams.js`（stream を選ぶ・作る）・`store.js`（このブラウザに置くもの）・`editor.js`（編集の画面）・`status.js` |
| `js/views/` | 画面。`render(main, params, ctx)` を持ち、片付けの関数を返してよい（`ctx = { caps, navigate, setCrumbs, nav }`）。準備中のページは `coming-soon.js` |

## ログインとユーザー（ADR-0017）

| 場所 | 中身 |
|---|---|
| `js/views/login.js` | ログインの画面。`GET /auth/providers` で出し分ける: SSO のボタン・パスワード・2 段階認証（6 桁・回復コード）・API トークン、利用者がいなければ最初の管理者を作る手順。`#/auth/complete?code=…`（コードを替える）・`?error=…`（理由の説明と再試行） |
| `js/views/auth-forced.js` | `pending`（`password_change`・`mfa_enrollment`）が済むまで出す画面。ログインの直後はそのときのパスワードをメモリに持ち、聞き直さない |
| `js/views/reauth.js` | 使っている途中の 401 で出す「もう一度ログイン」のダイアログ（閉じられない。ログアウトは選べる） |
| `js/views/account.js`・`users.js` | `#/account`（プロフィール・パスワード・2 段階認証・ログイン中の端末）、`#/users`（tenant:admin） |
| `js/views/auth-parts.js` | パスワードの欄（表示の切り替え・Caps Lock）・強さの目安・6 桁の欄（本物の input は 1 つ）・QR・IdP の印・回復コード・認証アプリの登録の流れ |
| `js/lib/session.js` | 今の主体（`GET /auth/me`）・`hasScope()`・行き先（return_to）。`/auth/me` の無い古いノードでは JWT の claim から推し量る |
| `js/lib/qr.js`・`password.js`・`useragent.js` | QR コードの生成（バイトモード・誤り訂正 M、外のサービスは使わない）、パスワードの強さ（長さと文字の種類だけ）と仮のパスワード、User-Agent から端末の名前 |

- 起動: `#/auth/complete` なら先に扱う（コードは URL からすぐ消す）→ 値が無ければログインの画面 → `GET /auth/me` → `pending` があれば手続き → `GET /capabilities` → 外枠。
- 行き先: ハッシュはログインの間も変えないので、ログインの後に開こうとしていたページが出る。OIDC は `return_to=/console/#/…` で戻す。ログアウトは `#/` に戻す。
- 401: `client.js` が `setReauthHandler` の関数（`main.js`）を待ち、入り直せたら同じ要求を 1 回だけ送り直す（401 の要求は処理されていないので POST でも重ならない）。画面の入力はそのまま残る。
- メニューの項目に `scope` を書くと、その権限が無いと分かっている主体には出さない（今は「ユーザー」だけ）。
- 赤は使わない: パスワードの強さの「弱い」は琥珀、失敗の待ち時間は warn。QR コードだけは読み取りのため SVG の中で白地に黒を固定する。
- 試験: `tools/interop/console.sh` の C（QR を `tools/interop/qr_decode.py` で読み戻す・パスワードの強さ・User-Agent）と D の「ログインとユーザー」の区切り
  （`tools/interop/console_auth.cjs`。試験用の IdP `oidc_mock.py` と 2 台のノードを使う）。

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
| `modal(title, build, {actions, wide, icon, kind, testid, beforeClose})` | `<dialog>` の showModal（フォーカスは中に閉じる、Escape・×・背景で閉じる、開いた要素へ戻る）。`beforeClose()` が false なら Escape・×・背景では閉じない（一度だけ見せる値を写す前など） |
| `confirmDialog(title, message, {ok, danger, consequences, requireText})` | message に「何が起きるか」を書く。取り消せない削除は `requireText`（名前を入れるまで押せない） |
| `toast(msg, kind, {duration, action})` | `action: {label, onClick}` で「元に戻す」「再試行」。`reportError(e, what, {retry})` は失敗を知らせる |
| `attachMenu(button, items)`・`tooltip(el, text, {kbd})`・`attachPopover(button, content)` | メニュー（↑↓・Escape）、ツールチップ、説明の箱。ダイアログの中でも使える（top layer） |
| `helpHint({title, body, guide})` | 「?」の説明。`guide` は手引きのページ（`'04-ingest-playback.md'`）。言語に合わせて `docs/guide/{ja,en}/` を開く |
| `formField({label, input, hint, required, optional, error, help})` | ラベル・説明・必須（見た目だけ。読み上げは `aria-required`）・その場の検査（`el.setError(msg)`） |
| `toggle({label, description, checked, onChange})` | すぐ効く設定（`role="switch"`）。送信して効くものはチェックボックス |
| `segmented({options, value, onChange, label})` | 2〜5 個から 1 つ（`radiogroup`） |
| `searchBox({placeholder, onInput, shortcut})` | `shortcut: true` で `/` で入れるページの検索になる |
| `copyField(value, label, {secret})` | 写すボタン付き。`secret` は伏せて表示し、目のボタンで見せる |
| `checklist({id, title, steps})` | はじめの手順。`steps[].done` は実際の状態から計算して渡す（`optional` で「任意」の印）。閉じたらこのブラウザでは出さない。`resetChecklist(id)` で開き直す |
| `choiceCards({label, options, value, onChange, compact})` | 説明つきの大きな選択肢（中身は本物のラジオ）。はじめての人に「どれを選ぶか」を聞くとき（配信の送り方・同時配信の送り先） |
| `sparkline({values, width, height, label})` | 直近の値の動き（ビットレートなど）。数は横に文字で出し、線は傾向だけ |
| `listNav(container, {rows, open})` | 一覧のキーボード操作（J・K・↑↓・Enter）と、行のどこを押しても開く。片付けの関数を返す |
| `codeBlock(text, {label, lang, testid})` | コードの見本（SDK の呼び方・curl）。等幅で横へ流し、右上にコピー。label はコピーの通知と読み上げの名前 |

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
  一覧では `listNav()` で J・K（↑↓）・Enter、作成は `onPageKey('n', …)`。
- 生で変わる中身（参加者・実行の状態）は見えている間だけ数秒ごとに読み、SSE でもすぐ読み直す。描き直すときはフォーカスのあった要素の `data-testid` へ戻す（押そうとしているボタンを奪わない）。
- 設定の多い作成は、よく使う項目だけを先に出し、残りは折りたたみ（`room-form.js` の `disclosure()`、`<details>`）に入れる。誤りは欄の `setError()` で出し、畳んだ中の誤りなら開く。
- 用途から作るもの（メディアパイプライン）は、用途のカード（`useCards()`）→ その用途の設定だけ → 右に「作られるもの・行き先」と直すところを入力に合わせて出す。直すところがあれば作成のボタンを押せなくする。

## 配信まわりの画面（配信・録画・予約・ブラウザから送る）

- 共通の道具は `js/lib/streaming.js`: 状態の読み分け（`streamState`）、受信の要約（`liveSummary`）、ビットレートの履歴（`recordHealth`・`healthSamples`。
  API に履歴が無いので、このタブで読んだ値を 5 分ぶんメモリに溜める）、今の画・代表画の小さな画像（`liveThumb`・`recordingThumb`、使い回しと失敗の間引き）、
  URL の `?…` の読み書き（`hashParams`・`setHashParams`。画面を作り直さない）。
- 一覧の絞り込み・検索・表示の切り替えは URL に残す（`#/streams?state=live&q=…`、`#/recordings?stream=…&state=…&from=…&to=…`、`#/schedules?view=all`）。
  詳細のタブは `?tab=`（`tabs()`）。コマンドパレットの操作はこの URL を開くだけにする。
- 狭い画面の一覧は「題の列（`primary`）＋操作」の小さなカードにする。ほかの列には `class: 'hide-sm'` を付け、題の列の中の `.sc-mobile` に要点（状態・数）を並べる。
- 配信の作成は `views/stream-create.js`（名前と送り方だけを聞き、詳しい設定は畳む）。作ったら `#/streams/{id}?setup=rtmp|srt|whip|pull` で
  「送り手をつなぐ」を出す。publish の開始は SSE のイベントにならないので、待っている間は 2 秒ごとに読み直して「待っています → 受信しています」を切り替える。
- ストリームキーはメモリ（`lib/secrets.js`）で詳細へ渡し、画面を離れたら捨てる。「このブラウザから送る」へは `handOffKey()` で渡す（保存しない）。
- 同時配信（`views/stream-restream.js`）の送り先のひな形は rtmp_push の URL を埋めるだけ。鍵はその場で秘密値（`POST /secrets`）にして `credential_ref` で参照する。
- 予約の繰り返しはこの画面が展開して時間割に並べる（`occurrences()`。選んだタイムゾーンの壁時計・曜日・回数・終わりの日）。時刻はこの端末のタイムゾーンで出し、
  予約のタイムゾーンが違えばそちらの時刻も添える（`whenText()`）。
- 見た目は `css/streaming.css`。

## 運用と設定の画面

- **ダッシュボード**（`views/dashboard.js`）: はじめの手順（`getting-started`。配信がある・配信中か録画がある・録画がある・トークンが 2 つ以上、などから進み具合を決める。
  「見てみる」は手順のボタンを押したことを localStorage に覚える）→ 様子の帯 → 確認が要ること（それぞれに直接の操作）→ 配信中（今の画は 15 秒ごと。作れなければ 5 分待つ）→
  録画中・予約 → 最近の出来事（SSE）・このノードでできること。`#/?onboarding=1` で手順を開き直す（コマンドパレット）。
  入力・出力の失敗は、配信ごとの一覧を読んで探す（`IO_SCAN_MAX` 件まで。配信中のものから）。
- **名前の解決**（`lib/names.js`）: 監査ログ・グリッドで ID を名前にする。一覧を 30 秒使い回す。`input:`・`output:` は `GET /inputs/{id}` で持ち主の配信を引く。
- **監査ログの操作**（`lib/audit-actions.js`）: `audit.action.<操作>` の言葉・分類（絞り込みとアイコン）・資源のページ。知らない操作は「分類: 残り」か ID のまま出す（`auth.login` など、後から増えるもの）。
  ページの項目が `limit` より少なくても `next_cursor` が返るので、`auditFill()` で見える分を埋め、下の端が見えたら続きを読む。条件は URL（`#/audit?action=stream.&range=24h`）。
- **グリッド**: 状態ごとの数がそのまま絞り込み。ノードの列（予約の量）と「配置できていない」列、一覧。理由は `grid.reason.*`・`grid.reject.*` の文にする。
- **一覧のキーボード**: 運用の画面も `ui/listnav.js` の `listNav()` を使う（ノードのカード・グリッドのカード・監査ログの行・トークンと秘密値の表）。項目には `data-key` を付ける。
- 確認のダイアログ: drain・装置の無効化のように戻せる操作は `danger: false`。トークンの失効は、強い権限（tenant:admin・node:admin）のものだけ名前の入力を求める。秘密値の削除は名前の入力を求める。
- 秘密値の「値を変える」: 使われていなければ消して同じ名前で作り直す。使われていれば新しい名前で作り、入力・出力の `credential_ref` を付け替え（If-Match）、古いものを消す（字幕の参照は手で）。

## 再生（`js/player/hls.js`）の決まりと限界

- fMP4 の HLS だけ。初期化セグメントからコーデック文字列を作り、SourceBuffer に順に追加する。
- ライブは端の 3 本前から始め、遅れすぎたら端へ寄せる。不連続（送り手の再接続）は `timestampOffset` でつなぐ。
- 幅・高さが 0 の AV1 の初期化セグメント（今のノードの Enhanced RTMP 由来）は、キーフレームの sequence header から大きさを読んで補正する。
- MPEG-TS は扱わない（Safari のネイティブ再生へ。ほかのブラウザは URL を出して VLC などを案内）。
- LL-HLS の part・ABR（複数の variant）・暗号化・字幕は使わない。

## パイプラインとスイッチャー

- 経路: 一覧 `#/pipelines`（`?new=1&template=<id>` でひな形のダイアログ）、編集 `#/pipelines/{id}`（下書きは `#/pipelines/pl_…`）、スイッチャー `#/production?pipeline={id}`。
- グラフは `crates/senca-pipeline/src/model.rs` の JSON（知らない項目は拒否される）。部品の位置・下書き・保存済みのものの未保存の変更・写した部品は
  グラフに入れられないので、このブラウザの localStorage に置く（`js/pipeline/store.js`）。
- 保存 = `POST /pipelines`（止めた状態で作る）か `PATCH`（If-Match。412 なら「読み直す／上書き」を選ばせる）。保存済みのものは変更のたびに 1.2 秒待って
  `POST /pipelines/{id}/validate` に編集中のグラフを送り、計画と問題（problem+json の `details.issues`）を出し直す。問題の `code` には平易な説明と直し方を添える
  （`pipelines.code.*`・`pipelines.fix.*`）。この画面の検査も同じ code を使う（`POLICY_REJECTED`・`CAPACITY_UNAVAILABLE`・`UNCONSUMED_OUTPUT`）。
- 実行の帯の primary は「次にすること」の 1 つ（下書きか未保存なら保存、保存済みで止まっていれば開始）。
- グラフの面: ポートの形はエンコード済み（菱形）とデコード済み（丸）、色は映像（青）と音声（緑）。線を引く間はつなげる点だけが光り、つなげない点では理由を出す。
  小さく表示しているときは名前だけを大きく出す（`lod-low`）。キーボードだけでも、Tab・矢印・Enter と検査器の「つなぐ元」「つなぐ先」で編集でき、
  「部品の一覧」のタブがグラフの文字の代わりになる。
- 元に戻す・やり直すは編集用のモデルの写し（`serialize`・`restore`）を 120 回分まで持つ。部品の移動はグラフを変えないので「未保存」にしない。
- スイッチャー: CUT・AUTO を `take`（トランジション付き）、シーンの押下を `set_preview`、ダブルクリックと Shift＋数字を `switch_program` に。
  T バーは途中の位置を送れないので、端から端まで動かした時間のトランジションで take（次は反対の端から）。安全装置を入れると CUT・AUTO は 2 回で確定、直接のカットは効かない。
  タリーの色はプログラム = `--live`、プレビュー = `--tally-preview`。シーンのタイルの絵は、合成の入力をたどった元の stream の静止画（`GET /streams/{id}/snapshot`、10 秒ごと）を配置どおりに描く。
- ノードは status（`event: status`）とメーター（`event: meters`）を同じ `status` 欄に入れることがあるので、`event` で読み分ける。

## WebRTC

- 再生（WHEP）: `playback-url` の `playback.whep`（署名付き）へ offer を POST、201 の answer を入れる。止めると Location を DELETE。
  配信前・運べないコーデック（AAC だけ・H.265）は 422 → 自動なら HLS に戻す。
- 送信（WHIP、`#/send`）: カメラ・マイクか画面 → `ingest.whip_url` へ Bearer の stream key で POST。鍵は保存しない。

## ルームとメディアパイプライン

- ルームの一覧の API は参加者の数を持たないので、配置済みのルームごとに `GET /rooms/{id}/participants` を読む（同時に 6 本、8 秒ごと、部屋の出来事でも）。
- 詳細の参加者は 1 人 1 枚のカード（狭い列・携帯でも読めるように表にしない）。トラックの大きさ・fps と受信の損失は、統計の `client`
  （SDK の `reportStats` で送られる getStats の要約）があるときだけ出す。層ごとのビットレートは `published[].layers`、受け取り中の層は参加者の受信の枠（`subscriptions` の mid）と統計の `rid` を合わせる。
- 出来事（`room.*` の SSE）は ID を持たず再送されないので、ページを開いてからのもの（＋ `lib/activity.js` の直近の分）だけを出す。
- 「このブラウザで参加」は `import('../vendor/senca-sdk/realtime/session.js')` を参加するときに読む。トークンは画面のログインの権限で発行し（`client_id: console-xxxx`、1 時間）、保存しない。
  `reportStats` を有効にするので、管理画面から入った参加者の大きさ・損失も詳細に出る。ページを離れたら退出してカメラを止める。
- 参加トークンの発行は、在るルームにだけ使う（無いルームに発行すると API が自動でルームを作るため）。
- メディアパイプラインの配信先の鍵をその場で入れたときは、作成の直前に秘密値（`{名前}-key`）として保存し、`credential_ref` で参照する。
