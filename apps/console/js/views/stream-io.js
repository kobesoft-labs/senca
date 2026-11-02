// 配信の詳細に置く部品: 入力（pull）・出力（push・PaaS）の管理、字幕の設定、再生 URL（HLS・WHEP・RTSP・RTMP）と字幕トラック。
// ノードの crates/senca-node/src/api/io.rs・api/streams.rs が正。

import { h, replace, table, select, field, badge, kv } from '../lib/dom.js';
import { modal, confirmDialog, toast, reportError, errorBox, copyField, load } from '../lib/ui.js';
import { inputs as inputsApi, outputs as outputsApi, secrets as secretsApi, streams } from '../api/resources.js';
import { fmtTime, fmtBytes } from '../lib/format.js';

export const INPUT_PROTOCOLS = [['rtmp_pull', 'RTMP（pull）'], ['rtsp_pull', 'RTSP（pull）'], ['srt_pull', 'SRT（pull）'], ['hls_pull', 'HLS（pull）']];
export const OUTPUT_PROTOCOLS = [['rtmp_push', 'RTMP（push）'], ['srt_push', 'SRT（push）'], ['whip_push', 'WHIP（push）'], ['paas', 'PaaS 連携']];
export const PROVIDERS = [['cloudflare_stream', 'Cloudflare Stream'], ['aws_ivs', 'Amazon IVS'], ['aws_medialive', 'AWS Elemental MediaLive']];
const URL_HINT = {
  rtmp_pull: 'rtmp://host/app/key', rtsp_pull: 'rtsp://camera/stream1', srt_pull: 'srt://host:port?streamid=…', hls_pull: 'http(s)://cdn.example/live/index.m3u8',
  rtmp_push: 'rtmp://host/app（鍵は資格情報へ）', srt_push: 'srt://host:port', whip_push: 'http(s)://host/whip/endpoint',
};
const PROVIDER_HINT = {
  cloudflare_stream: '資格情報は account_id:api_token。options: recording=off|automatic、customer_subdomain、ingest=srt|rtmps',
  aws_ivs: '資格情報は AWS のアクセスキー。options: region（必須）、channel_type、latency_mode、ingest=srt|rtmps',
  aws_medialive: '資格情報は AWS のアクセスキー。options: region（必須）、input_type、allow_cidr、pipelines、rtmp_app',
};

const stateKind = (s) => ({ active: 'ok', connecting: 'warn', starting: 'warn', backoff: 'warn', waiting_input: '', provisioning: 'warn', error: 'danger' }[s] ?? '');

/** 状態（io_status）の表示。 */
export function ioStatus(st, enabled) {
  if (!enabled) return badge('無効', '');
  if (!st) return h('span', { class: 'muted small' }, '—');
  return h('div', null,
    badge(st.state, stateKind(st.state)),
    h('div', { class: 'small muted' }, `接続 ${st.connects} 回・${fmtBytes(st.bytes)}${st.since ? `・${fmtTime(st.since)} から` : ''}`),
    st.last_error ? h('div', { class: 'small field-error' }, st.last_error) : null);
}

const parseOptions = (text) => {
  const o = {};
  for (const line of String(text || '').split('\n')) {
    const t = line.trim();
    if (!t) continue;
    const i = t.indexOf('=');
    if (i <= 0) throw new Error(`options の行「${t}」は key=value の形にしてください`);
    o[t.slice(0, i).trim()] = t.slice(i + 1).trim();
  }
  return o;
};
const showOptions = (o) => Object.entries(o || {}).map(([k, v]) => `${k}=${v}`).join('\n');

async function secretChoices() {
  try {
    const { data } = await secretsApi.list();
    return data.items.map((s) => [s.ref, s.ref]);
  } catch {
    return null; // tenant:admin が無い・古いノード
  }
}

/**
 * 入力・出力の作成と編集のダイアログ。
 * @param {'input'|'output'} kind
 * @param {object|null} cur 編集する資源（null で作成）
 */
async function ioDialog(kind, cur, streamId) {
  const isOut = kind === 'output';
  const protos = isOut ? OUTPUT_PROTOCOLS : INPUT_PROTOCOLS;
  const proto = select(protos, cur ? cur.protocol : protos[0][0], { disabled: !!cur });
  const url = h('input', { type: 'text', value: cur ? cur.url || '' : '', spellcheck: 'false', autocomplete: 'off' });
  const secretsList = await secretChoices();
  const credSel = secretsList
    ? select([['', '（なし）'], ...secretsList, ...(cur && cur.credential_ref && !secretsList.some((x) => x[0] === cur.credential_ref) ? [[cur.credential_ref, cur.credential_ref]] : [])], cur ? cur.credential_ref || '' : '')
    : h('input', { type: 'text', value: cur ? cur.credential_ref || '' : '', placeholder: 'secret:name' });
  const provider = select(PROVIDERS, cur ? cur.provider || 'cloudflare_stream' : 'cloudflare_stream', { disabled: !!cur });
  const enabled = h('input', { type: 'checkbox', checked: cur ? cur.enabled : true });
  const options = h('textarea', { rows: 3, spellcheck: 'false', placeholder: 'key=value（1 行に 1 つ）' }, showOptions(cur && cur.options));
  const urlField = field('URL', url);
  const provField = field('提供者', provider);
  const hint = h('div', { class: 'hint' });
  const errBox = h('div');
  const sync = () => {
    const p = proto.value;
    url.placeholder = URL_HINT[p] || '';
    provField.hidden = p !== 'paas';
    urlField.hidden = p === 'paas';
    hint.textContent = p === 'paas' ? PROVIDER_HINT[provider.value] : p === 'rtsp_pull' ? 'options: allow_basic=true で Basic 認証を許す（既定は Digest だけ）' : 'URL に資格情報（user:pass@）は入れられません。鍵・パスワードは秘密値にして資格情報で参照します。';
  };
  proto.addEventListener('change', sync);
  provider.addEventListener('change', sync);
  sync();
  const body = () => {
    const o = parseOptions(options.value);
    const cred = credSel.value || null;
    if (cur) {
      const b = {};
      if (cur.protocol !== 'paas' && url.value !== (cur.url || '')) b.url = url.value;
      if (cred !== (cur.credential_ref || null)) b.credential_ref = cred;
      if (enabled.checked !== cur.enabled) b.enabled = enabled.checked;
      if (showOptions(o) !== showOptions(cur.options)) b.options = o;
      return b;
    }
    const b = { protocol: proto.value, enabled: enabled.checked, options: o };
    if (proto.value === 'paas') b.provider = provider.value;
    else b.url = url.value;
    if (cred) b.credential_ref = cred;
    return b;
  };
  return modal(`${isOut ? '出力' : '入力'}を${cur ? '編集' : '追加'}`, () => [
    h('div', { class: 'form' },
      field('方式', proto), provField, urlField,
      field('資格情報（秘密値）', credSel, secretsList ? '秘密値の画面で作ったものを選びます。値はここには出ません。' : '秘密値を一覧できません（tenant:admin が要ります）。secret:{name} を入れてください。'),
      h('label', { class: 'check' }, enabled, '有効'),
      field('options', options), hint),
    errBox,
  ], {
    wide: true,
    actions: (close) => {
      const ok = h('button', { class: 'primary', onClick: async () => {
        let b;
        try { b = body(); } catch (e) { replace(errBox, h('div', { class: 'alert error' }, e.message)); return; }
        if (cur && !Object.keys(b).length) { close(null); return; }
        ok.disabled = true;
        try {
          const api = isOut ? outputsApi : inputsApi;
          const r = cur ? await api.update(cur.id, b, cur.etag || `"${cur.generation}"`) : await api.create(streamId, b);
          close(r.data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: '保存できませんでした' }));
        } finally {
          ok.disabled = false;
        }
      } }, cur ? '保存' : '追加');
      return [h('button', { onClick: () => close(null) }, 'やめる'), ok];
    },
  });
}

/** 入力または出力の一覧と操作。 */
export function ioSection(kind, streamId) {
  const isOut = kind === 'output';
  const api = isOut ? outputsApi : inputsApi;
  const box = h('div');
  const what = isOut ? '出力' : '入力';
  const reload = () => load(box, async () => {
    const { data } = await api.list(streamId);
    return table([
      { label: '方式', render: (x) => [h('code', null, x.protocol), x.provider ? h('div', { class: 'small' }, x.provider) : null] },
      { label: 'URL・資源', render: (x) => [x.url ? h('code', { class: 'break' }, x.url) : null, x.provider_resource ? h('div', { class: 'small muted' }, `資源: ${x.provider_resource}`) : null] },
      { label: '資格情報', render: (x) => (x.credential_ref ? h('code', null, x.credential_ref) : '—') },
      { label: '状態', render: (x) => ioStatus(x.status, x.enabled) },
      { label: '', render: (x) => h('div', { class: 'row' },
        h('button', { class: 'small', onClick: async () => { if (await ioDialog(kind, x, streamId)) { toast(`${what}を保存しました`, 'ok'); reload(); } } }, '編集'),
        h('button', { class: 'small', onClick: async () => {
          try { await api.update(x.id, { enabled: !x.enabled }, `"${x.generation}"`); reload(); } catch (e) { reportError(e, x.enabled ? '無効にする' : '有効にする'); reload(); }
        } }, x.enabled ? '無効にする' : '有効にする'),
        h('button', { class: 'small danger', 'aria-label': `${what} ${x.protocol} を削除`, onClick: async () => {
          if (!(await confirmDialog(`${what}を削除`, `${x.protocol} ${x.url || x.provider || ''} を削除します。${x.protocol === 'paas' ? 'SENCA が外部に作った資源も片付けます。' : ''}`, { ok: '削除' }))) return;
          try { await api.remove(x.id); toast('削除しました', 'ok'); reload(); } catch (e) { reportError(e, '削除'); }
        } }, '削除')) },
    ], data.items, { empty: `${what}はありません`, caption: `${what}の一覧` });
  });
  const el = h('div', null,
    h('div', { class: 'toolbar' },
      h('button', { onClick: async () => { if (await ioDialog(kind, null, streamId)) { toast(`${what}を追加しました`, 'ok'); reload(); } } }, `＋ ${what}を追加`),
      h('button', { class: 'small', onClick: () => reload() }, '再読み込み')),
    box);
  reload();
  return { el, reload };
}

// ---------------------------------------------------------------- 字幕

export const ASR_PROVIDERS = [['openai', 'OpenAI 互換（/audio/transcriptions）'], ['whisper_cpp', 'whisper.cpp のサーバー']];
export const TRANSLATE_PROVIDERS = [['', '（翻訳しない）'], ['anthropic', 'Anthropic'], ['deepl', 'DeepL'], ['openai', 'OpenAI 互換']];

/** 字幕の設定（stream.captions）を編集するフォーム。value() は PATCH の captions（null で止める）。 */
export function captionsForm(cur, secretsList) {
  const c = cur || {};
  const on = h('input', { type: 'checkbox', checked: !!cur });
  const credInput = (v) => (secretsList
    ? select([['', '（なし）'], ...secretsList, ...(v && !secretsList.some((x) => x[0] === v) ? [[v, v]] : [])], v || '')
    : h('input', { type: 'text', value: v || '', placeholder: 'secret:name' }));
  const asrProvider = select(ASR_PROVIDERS, c.asr_provider || 'openai');
  const asrEndpoint = h('input', { type: 'text', value: c.asr_endpoint || '', placeholder: '例: api.openai.com/v1（https の URL）', spellcheck: 'false' });
  const asrCred = credInput(c.asr_credential_ref);
  const asrModel = h('input', { type: 'text', value: c.asr_model || '', placeholder: 'whisper-1' });
  const language = h('input', { type: 'text', value: c.language || 'ja', maxlength: 8, style: { width: '80px' } });
  const trProvider = select(TRANSLATE_PROVIDERS, c.translate_provider || '');
  const trEndpoint = h('input', { type: 'text', value: c.translate_endpoint || '', placeholder: '例: api.anthropic.com（https の URL）', spellcheck: 'false' });
  const trCred = credInput(c.translate_credential_ref);
  const trModel = h('input', { type: 'text', value: c.translate_model || '' });
  const targets = h('input', { type: 'text', value: (c.targets || []).join(', '), placeholder: 'en, ko, zh' });
  const trBox = h('fieldset', null, h('legend', null, '翻訳'),
    field('翻訳の提供者', trProvider), field('送り先の URL', trEndpoint), field('資格情報', trCred), field('モデル（任意）', trModel),
    field('翻訳先の言語（ISO 639-1、カンマ区切り、8 個まで）', targets));
  const body = h('div', { class: 'form' },
    h('div', { class: 'alert warn', role: 'note' },
      h('strong', null, '外部のサービスへ送ります。'),
      ' 字幕を有効にすると、この配信の音声を音声認識の送り先へ、認識した文字を翻訳の送り先へ送ります。送り先の利用規約・データの扱い（保存・学習への利用・国外への移転）を確かめてから有効にしてください。社内の whisper.cpp のサーバーなら外へは出ません。'),
    h('label', { class: 'check' }, on, '字幕を作る（音声認識）'),
    h('fieldset', null, h('legend', null, '音声認識'),
      field('提供者', asrProvider), field('送り先の URL', asrEndpoint), field('資格情報（秘密値）', asrCred, '鍵は秘密値の画面で作り、ここで選びます'),
      field('モデル（任意）', asrModel), field('話している言語（ISO 639-1）', language)),
    trBox);
  const sync = () => {
    for (const el of body.querySelectorAll('fieldset input, fieldset select')) el.disabled = !on.checked;
    for (const el of [trEndpoint, trCred, trModel, targets]) el.disabled = !on.checked || !trProvider.value;
  };
  on.addEventListener('change', sync);
  trProvider.addEventListener('change', sync);
  sync();
  const value = () => {
    if (!on.checked) return null;
    if (!asrEndpoint.value.trim()) throw new Error('音声認識の送り先の URL がありません');
    const v = { asr_provider: asrProvider.value, asr_endpoint: asrEndpoint.value.trim(), language: language.value.trim() || 'ja' };
    if (asrCred.value) v.asr_credential_ref = asrCred.value;
    if (asrModel.value.trim()) v.asr_model = asrModel.value.trim();
    if (trProvider.value) {
      if (!trEndpoint.value.trim()) throw new Error('翻訳の送り先の URL がありません');
      v.translate_provider = trProvider.value;
      v.translate_endpoint = trEndpoint.value.trim();
      if (trCred.value) v.translate_credential_ref = trCred.value;
      if (trModel.value.trim()) v.translate_model = trModel.value.trim();
      v.targets = targets.value.split(/[,\s]+/).map((x) => x.trim()).filter(Boolean);
    }
    return v;
  };
  return { el: body, value };
}

/** 字幕の設定の表示と編集。 */
export function captionsSection(getStream, getEtag, onSaved) {
  const box = h('div');
  const renderView = () => {
    const st = getStream();
    const c = st.captions;
    replace(box,
      c ? kv([
        ['音声認識', [h('code', null, c.asr_provider), ' → ', h('code', { class: 'break' }, c.asr_endpoint), c.asr_model ? `（${c.asr_model}）` : '']],
        ['資格情報', c.asr_credential_ref ? h('code', null, c.asr_credential_ref) : 'なし'],
        ['言語', c.language],
        ['翻訳', c.translate_provider ? [h('code', null, c.translate_provider), ' → ', h('code', { class: 'break' }, c.translate_endpoint || ''), `・${(c.targets || []).join(', ') || '言語なし'}`] : 'しない'],
      ]) : h('p', { class: 'muted' }, '字幕は作っていません（外部へは何も送りません）。'),
      c ? h('div', { class: 'alert info small' }, '有効です: この配信の音声は上の送り先へ送られます。') : null,
      h('div', { class: 'toolbar' }, h('button', { onClick: edit }, '字幕の設定を編集')));
  };
  const edit = async () => {
    const st = getStream();
    const form = captionsForm(st.captions, await secretChoices());
    const errBox = h('div');
    const saved = await modal('字幕と翻訳', () => [form.el, errBox], {
      wide: true,
      actions: (close) => {
        const ok = h('button', { class: 'primary', onClick: async () => {
          let v;
          try { v = form.value(); } catch (e) { replace(errBox, h('div', { class: 'alert error' }, e.message)); return; }
          ok.disabled = true;
          try {
            const cur = getStream();
            const r = await streams.update(cur.id, { captions: v }, getEtag());
            close(r);
          } catch (e) {
            replace(errBox, errorBox(e, { title: '保存できませんでした' }));
          } finally { ok.disabled = false; }
        } }, '保存');
        return [h('button', { onClick: () => close(null) }, 'やめる'), ok];
      },
    });
    if (saved) {
      onSaved(saved);
      toast(saved.data.captions ? '字幕の設定を保存しました' : '字幕を止めました', 'ok');
    }
  };
  return { el: box, render: renderView };
}

// ---------------------------------------------------------------- 再生 URL と字幕トラック

/** master.m3u8 の字幕（EXT-X-MEDIA TYPE=SUBTITLES）を読む。 */
export function parseSubtitleMedia(text, base) {
  const out = [];
  for (const line of String(text).split(/\r?\n/)) {
    if (!line.startsWith('#EXT-X-MEDIA:')) continue;
    const attrs = {};
    for (const m of line.slice(13).matchAll(/([A-Z0-9-]+)=("[^"]*"|[^,]*)/g)) attrs[m[1]] = m[2].replace(/^"|"$/g, '');
    if (attrs.TYPE !== 'SUBTITLES') continue;
    out.push({ language: attrs.LANGUAGE || '', name: attrs.NAME || '', uri: attrs.URI ? new URL(attrs.URI, base).toString() : null, default: attrs.DEFAULT === 'YES' });
  }
  return out;
}

export function playbackSection(streamId) {
  const box = h('div', null, h('p', { class: 'muted small' }, '「再生 URL を発行」で期限付きの URL（HLS・WebRTC の WHEP・RTSP・RTMP）を作ります。'));
  const subs = h('div');
  const ttl = select([['300', '5 分'], ['3600', '1 時間'], ['86400', '1 日']], '3600', { 'aria-label': '有効期限' });
  const issue = async () => {
    try {
      const { data } = await streams.playbackUrl(streamId, Number(ttl.value));
      const p = data.playback;
      replace(box,
        h('p', { class: 'small muted' }, `期限: ${fmtTime(data.expires_at)}`),
        h('div', { class: 'form' },
          h('div', { class: 'field' }, h('strong', null, 'HLS'), copyField(p.hls, 'HLS の URL')),
          p.whep ? h('div', { class: 'field' }, h('strong', null, 'WebRTC（WHEP）'), copyField(p.whep, 'WHEP の URL'), h('div', { class: 'hint' }, 'WHEP の再生器（OBS の WHEP ソースなど）に入れます。遅延は 1 秒未満。')) : null,
          p.rtsp ? h('div', { class: 'field' }, h('strong', null, 'RTSP'), copyField(p.rtsp, 'RTSP の URL')) : null,
          p.rtmp ? h('div', { class: 'field' }, h('strong', null, 'RTMP（play）'), copyField(p.rtmp, 'RTMP の URL')) : null));
      checkSubs(p.hls);
    } catch (e) {
      reportError(e, '再生 URL');
    }
  };
  const checkSubs = async (hls) => {
    const master = hls.replace(/index\.m3u8(\?.*)?$/, 'master.m3u8');
    replace(subs, h('span', { class: 'muted small' }, '字幕トラックを確かめています…'));
    try {
      const r = await fetch(master, { cache: 'no-store', credentials: 'omit' });
      if (!r.ok) {
        replace(subs, h('p', { class: 'muted small', 'data-subtitles': '0' }, `字幕トラックはありません（master.m3u8: HTTP ${r.status}）。字幕を有効にして配信中なら、最初の字幕が出たあとに現れます。`));
        return;
      }
      const list = parseSubtitleMedia(await r.text(), master);
      replace(subs,
        h('div', { class: 'field' }, h('strong', null, '字幕つきの HLS（master.m3u8）'), copyField(master, 'master.m3u8 の URL')),
        list.length ? table([
          { label: '言語', render: (x) => h('code', null, x.language) },
          { label: '名前', render: (x) => x.name },
          { label: '既定', render: (x) => (x.default ? '○' : '') },
          { label: 'プレイリスト', render: (x) => (x.uri ? copyField(x.uri, `${x.language} の字幕`) : '—') },
        ], list, { caption: '字幕トラック' }) : h('p', { class: 'muted small', 'data-subtitles': '0' }, '字幕トラックはまだありません'));
      if (list.length) subs.dataset.subtitles = String(list.length);
    } catch (e) {
      replace(subs, h('p', { class: 'muted small' }, `字幕トラックを確かめられません: ${e.message}`));
    }
  };
  const el = h('div', null, h('div', { class: 'toolbar' }, h('button', { onClick: issue }, '再生 URL を発行'), ttl), box, h('h3', null, '字幕トラック'), subs);
  return { el };
}
