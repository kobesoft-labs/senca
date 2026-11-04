// 配信の詳細に置く部品: 入力（pull）・出力（push・PaaS）の管理、字幕の設定、再生 URL（HLS・WHEP・RTSP・RTMP）と字幕トラック。
// ノードの crates/senca-node/src/api/io.rs・api/streams.rs が正。

import { h, replace, select } from '../lib/dom.js';
import { modal, confirmDialog, toast, reportError, errorBox, copyField, load } from '../lib/ui.js';
import { inputs as inputsApi, outputs as outputsApi, secrets as secretsApi, streams } from '../api/resources.js';
import { fmtTime, fmtBytes } from '../lib/format.js';
import { t } from '../i18n/index.js';
import { button, iconButton, badge, alert, emptyState, dataTable, kvList, formField } from '../ui/index.js';

export const INPUT_PROTOCOLS = [['rtmp_pull', 'RTMP（pull）'], ['rtsp_pull', 'RTSP（pull）'], ['srt_pull', 'SRT（pull）'], ['hls_pull', 'HLS（pull）']];
export const OUTPUT_PROTOCOLS = [['rtmp_push', 'RTMP（push）'], ['srt_push', 'SRT（push）'], ['whip_push', 'WHIP（push）'], ['paas', 'PaaS']];
export const PROVIDERS = [['cloudflare_stream', 'Cloudflare Stream'], ['aws_ivs', 'Amazon IVS'], ['aws_medialive', 'AWS Elemental MediaLive']];
const URL_HINT = {
  rtmp_pull: 'rtmp://host/app/key', rtsp_pull: 'rtsp://camera/stream1', srt_pull: 'srt://host:port?streamid=…', hls_pull: 'http(s)://cdn.example/live/index.m3u8',
  rtmp_push: 'rtmp://host/app', srt_push: 'srt://host:port', whip_push: 'http(s)://host/whip/endpoint',
};

const STATE_KIND = { active: 'ok', connecting: 'warn', starting: 'warn', backoff: 'warn', waiting_input: null, provisioning: 'warn', error: 'danger' };

/** 状態（io_status）の表示。 */
export function ioStatus(st, enabled) {
  if (!enabled) return badge(t('io.disabled'), 'outline');
  if (!st) return h('span', { class: 'muted small' }, '—');
  const known = st.state in STATE_KIND;
  return h('div', null,
    badge(known ? t(`io.state.${st.state}`) : st.state, STATE_KIND[st.state], { dot: true, title: st.state }),
    h('div', { class: 'small muted' }, t('io.statusLine', { n: st.connects, bytes: fmtBytes(st.bytes) }), st.since ? t('io.since', { time: fmtTime(st.since) }) : ''),
    st.last_error ? h('div', { class: 'small field-error' }, st.last_error) : null);
}

const parseOptions = (text) => {
  const o = {};
  for (const line of String(text || '').split('\n')) {
    const s = line.trim();
    if (!s) continue;
    const i = s.indexOf('=');
    if (i <= 0) throw new Error(t('io.optionsBadLine', { line: s }));
    o[s.slice(0, i).trim()] = s.slice(i + 1).trim();
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
  const protos = (isOut ? OUTPUT_PROTOCOLS : INPUT_PROTOCOLS).map(([v, l]) => [v, v === 'paas' ? t('io.paas') : l]);
  const proto = select(protos, cur ? cur.protocol : protos[0][0], { disabled: !!cur });
  const url = h('input', { type: 'text', value: cur ? cur.url || '' : '', spellcheck: 'false', autocomplete: 'off' });
  const secretsList = await secretChoices();
  const credSel = secretsList
    ? select([['', t('io.none')], ...secretsList, ...(cur && cur.credential_ref && !secretsList.some((x) => x[0] === cur.credential_ref) ? [[cur.credential_ref, cur.credential_ref]] : [])], cur ? cur.credential_ref || '' : '')
    : h('input', { type: 'text', value: cur ? cur.credential_ref || '' : '', placeholder: 'secret:name' });
  const provider = select(PROVIDERS, cur ? cur.provider || 'cloudflare_stream' : 'cloudflare_stream', { disabled: !!cur });
  const enabled = h('input', { type: 'checkbox', checked: cur ? cur.enabled : true });
  const options = h('textarea', { rows: 3, spellcheck: 'false', placeholder: t('io.optionsPlaceholder') }, showOptions(cur && cur.options));
  const urlField = formField({ label: 'URL', input: url });
  const provField = formField({ label: t('io.provider'), input: provider });
  const hint = h('div', { class: 'hint' });
  const errBox = h('div');
  const sync = () => {
    const p = proto.value;
    url.placeholder = p === 'rtmp_push' ? `${URL_HINT[p]} ${t('io.keyToCredential')}` : URL_HINT[p] || '';
    provField.hidden = p !== 'paas';
    urlField.hidden = p === 'paas';
    hint.textContent = p === 'paas' ? t(`io.providerHint.${provider.value}`) : p === 'rtsp_pull' ? t('io.rtspHint') : t('io.urlHint');
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
  const title = t(`io.dialog.${isOut ? 'output' : 'input'}.${cur ? 'edit' : 'add'}`);
  return modal(title, () => [
    h('div', { class: 'form' },
      formField({ label: t('io.protocol'), input: proto }), provField, urlField,
      formField({ label: t('io.credential'), input: credSel, hint: secretsList ? t('io.credentialHint') : t('io.credentialNoList') }),
      h('label', { class: 'check' }, enabled, t('io.enabled')),
      formField({ label: 'options', input: options }), hint),
    errBox,
  ], {
    wide: true,
    icon: isOut ? 'upload' : 'download',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', onClick: async () => {
        let b;
        try { b = body(); } catch (e) { replace(errBox, alert('error', e.message)); return; }
        if (cur && !Object.keys(b).length) { close(null); return; }
        ok.disabled = true;
        try {
          const api = isOut ? outputsApi : inputsApi;
          const r = cur ? await api.update(cur.id, b, cur.etag || `"${cur.generation}"`) : await api.create(streamId, b);
          close(r.data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: t('io.saveFailed') }));
        } finally {
          ok.disabled = false;
        }
      } }, cur ? t('common.save') : t('common.add'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

/** 入力または出力の一覧と操作。 */
export function ioSection(kind, streamId) {
  const isOut = kind === 'output';
  const api = isOut ? outputsApi : inputsApi;
  const box = h('div');
  const k = isOut ? 'output' : 'input';
  const what = t(`io.${k}`);
  const add = async () => { if (await ioDialog(kind, null, streamId)) { toast(t(`io.added.${k}`), 'ok'); reload(); } };
  const reload = () => load(box, async () => {
    const { data } = await api.list(streamId);
    if (!data.items.length) {
      return emptyState({
        compact: true, icon: isOut ? 'upload' : 'download', title: t(`io.empty.${k}`), body: t(`io.emptyBody.${k}`),
        actions: [button(t(`io.add.${k}`), { icon: 'plus', size: 'sm', onClick: add })],
      });
    }
    return dataTable({
      caption: t(`io.caption.${k}`),
      rows: data.items,
      columns: [
        { label: t('io.protocol'), render: (x) => [h('code', null, x.protocol), x.provider ? h('div', { class: 'small' }, x.provider) : null] },
        { label: t('io.urlResource'), wrap: true, render: (x) => [x.url ? h('code', { class: 'break' }, x.url) : null, x.provider_resource ? h('div', { class: 'small muted' }, t('io.resource', { r: x.provider_resource })) : null] },
        { label: t('io.credentialShort'), render: (x) => (x.credential_ref ? h('code', null, x.credential_ref) : '—') },
        { label: t('common.state'), wrap: true, render: (x) => ioStatus(x.status, x.enabled) },
        { label: t('common.actions'), actions: true, render: (x) => h('div', { class: 'row' },
          button(t('common.edit'), { size: 'sm', icon: 'edit', onClick: async () => { if (await ioDialog(kind, x, streamId)) { toast(t(`io.saved.${k}`), 'ok'); reload(); } } }),
          button(x.enabled ? t('io.disable') : t('io.enable'), { size: 'sm', onClick: async () => {
            try { await api.update(x.id, { enabled: !x.enabled }, `"${x.generation}"`); reload(); } catch (e) { reportError(e, x.enabled ? t('io.disable') : t('io.enable')); reload(); }
          } }),
          iconButton('trash', t('io.deleteLabel', { what, protocol: x.protocol }), { size: 'sm', variant: 'danger', onClick: async () => {
            const target = `${x.protocol} ${x.url || x.provider || ''}`.trim();
            if (!(await confirmDialog(t(`io.deleteTitle.${k}`), t('io.deleteBody', { target }), {
              ok: t('common.delete'), consequences: [x.protocol === 'paas' ? t('io.deletePaas') : t(`io.deleteConseq.${k}`)],
            }))) return;
            try { await api.remove(x.id); toast(t('common.deleted'), 'ok'); reload(); } catch (e) { reportError(e, t('common.delete')); }
          } })) },
      ],
    });
  }, { kind: 'table' });
  const el = h('div', null,
    h('div', { class: 'toolbar' },
      button(t(`io.add.${k}`), { icon: 'plus', onClick: add }),
      iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => reload() })),
    box);
  reload();
  return { el, reload };
}

// ---------------------------------------------------------------- 字幕

export const ASR_PROVIDERS = [['openai', 'openai'], ['whisper_cpp', 'whisper_cpp']];
export const TRANSLATE_PROVIDERS = [['', ''], ['anthropic', 'Anthropic'], ['deepl', 'DeepL'], ['openai', 'openai']];

/** 字幕の設定（stream.captions）を編集するフォーム。value() は PATCH の captions（null で止める）。 */
export function captionsForm(cur, secretsList) {
  const c = cur || {};
  const on = h('input', { type: 'checkbox', checked: !!cur });
  const credInput = (v) => (secretsList
    ? select([['', t('io.none')], ...secretsList, ...(v && !secretsList.some((x) => x[0] === v) ? [[v, v]] : [])], v || '')
    : h('input', { type: 'text', value: v || '', placeholder: 'secret:name' }));
  const asrProvider = select([['openai', t('io.cc.asrOpenai')], ['whisper_cpp', t('io.cc.asrWhisper')]], c.asr_provider || 'openai');
  const asrEndpoint = h('input', { type: 'text', value: c.asr_endpoint || '', placeholder: t('io.cc.asrEndpointPh'), spellcheck: 'false' });
  const asrCred = credInput(c.asr_credential_ref);
  const asrModel = h('input', { type: 'text', value: c.asr_model || '', placeholder: 'whisper-1' });
  const language = h('input', { type: 'text', value: c.language || 'ja', maxlength: 8 });
  language.style.width = '96px';
  const trProvider = select([['', t('io.cc.noTranslate')], ['anthropic', 'Anthropic'], ['deepl', 'DeepL'], ['openai', t('io.cc.trOpenai')]], c.translate_provider || '');
  const trEndpoint = h('input', { type: 'text', value: c.translate_endpoint || '', placeholder: t('io.cc.trEndpointPh'), spellcheck: 'false' });
  const trCred = credInput(c.translate_credential_ref);
  const trModel = h('input', { type: 'text', value: c.translate_model || '' });
  const targets = h('input', { type: 'text', value: (c.targets || []).join(', '), placeholder: 'en, ko, zh' });
  const trBox = h('fieldset', null, h('legend', null, t('io.cc.translation')),
    formField({ label: t('io.cc.trProvider'), input: trProvider }), formField({ label: t('io.cc.endpoint'), input: trEndpoint }),
    formField({ label: t('io.cc.credential'), input: trCred }), formField({ label: t('io.cc.model'), input: trModel }),
    formField({ label: t('io.cc.targets'), input: targets }));
  const body = h('div', { class: 'form' },
    alert('warn', t('io.cc.noticeBody'), { title: t('io.cc.noticeTitle'), role: 'note' }),
    h('label', { class: 'check' }, on, t('io.cc.enable')),
    h('fieldset', null, h('legend', null, t('io.cc.asr')),
      formField({ label: t('io.provider'), input: asrProvider }), formField({ label: t('io.cc.endpoint'), input: asrEndpoint }),
      formField({ label: t('io.credential'), input: asrCred, hint: t('io.cc.credHint') }),
      formField({ label: t('io.cc.model'), input: asrModel }), formField({ label: t('io.cc.language'), input: language })),
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
    if (!asrEndpoint.value.trim()) throw new Error(t('io.cc.errAsrUrl'));
    const v = { asr_provider: asrProvider.value, asr_endpoint: asrEndpoint.value.trim(), language: language.value.trim() || 'ja' };
    if (asrCred.value) v.asr_credential_ref = asrCred.value;
    if (asrModel.value.trim()) v.asr_model = asrModel.value.trim();
    if (trProvider.value) {
      if (!trEndpoint.value.trim()) throw new Error(t('io.cc.errTrUrl'));
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
      c ? kvList([
        [t('io.cc.asr'), [h('code', null, c.asr_provider), ' → ', h('code', { class: 'break' }, c.asr_endpoint), c.asr_model ? ` (${c.asr_model})` : '']],
        [t('io.credentialShort'), c.asr_credential_ref ? h('code', null, c.asr_credential_ref) : t('common.no')],
        [t('io.cc.languageShort'), c.language],
        [t('io.cc.translation'), c.translate_provider ? [h('code', null, c.translate_provider), ' → ', h('code', { class: 'break' }, c.translate_endpoint || ''), ` · ${(c.targets || []).join(', ') || t('io.cc.noTargets')}`] : t('io.cc.noTranslateShort')],
      ]) : h('p', { class: 'muted' }, t('io.cc.off')),
      c ? alert('info', t('io.cc.activeNote'), { small: true }) : null,
      h('div', { class: 'toolbar' }, button(t('io.cc.edit'), { icon: 'captions', onClick: edit })));
  };
  const edit = async () => {
    const st = getStream();
    const form = captionsForm(st.captions, await secretChoices());
    const errBox = h('div');
    const saved = await modal(t('io.cc.title'), () => [form.el, errBox], {
      wide: true,
      icon: 'captions',
      actions: (close) => {
        const ok = h('button', { type: 'button', class: 'primary', onClick: async () => {
          let v;
          try { v = form.value(); } catch (e) { replace(errBox, alert('error', e.message)); return; }
          ok.disabled = true;
          try {
            const cur = getStream();
            const r = await streams.update(cur.id, { captions: v }, getEtag());
            close(r);
          } catch (e) {
            replace(errBox, errorBox(e, { title: t('io.saveFailed') }));
          } finally { ok.disabled = false; }
        } }, t('common.save'));
        return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
      },
    });
    if (saved) {
      onSaved(saved);
      toast(saved.data.captions ? t('io.cc.saved') : t('io.cc.stopped'), 'ok');
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
  const box = h('div', null, h('p', { class: 'muted small' }, t('io.pb.lead')));
  const subs = h('div');
  const ttl = select([['300', t('io.pb.ttl5m')], ['3600', t('io.pb.ttl1h')], ['86400', t('io.pb.ttl1d')]], '3600', { 'aria-label': t('io.pb.ttl') });
  ttl.style.width = 'auto';
  const urlField = (label, value, aria, hint) => h('div', { class: 'field' }, h('div', { class: 'label' }, label), copyField(value, aria), hint ? h('div', { class: 'hint' }, hint) : null);
  const issue = async () => {
    try {
      const { data } = await streams.playbackUrl(streamId, Number(ttl.value));
      const p = data.playback;
      replace(box,
        h('p', { class: 'small muted' }, t('io.pb.expires', { time: fmtTime(data.expires_at) })),
        h('div', { class: 'form' },
          urlField('HLS', p.hls, t('io.pb.hlsUrl')),
          p.whep ? urlField('WebRTC（WHEP）', p.whep, t('io.pb.whepUrl'), t('io.pb.whepHint')) : null,
          p.rtsp ? urlField('RTSP', p.rtsp, t('io.pb.rtspUrl')) : null,
          p.rtmp ? urlField(t('io.pb.rtmpPlay'), p.rtmp, t('io.pb.rtmpUrl')) : null));
      checkSubs(p.hls);
    } catch (e) {
      reportError(e, t('io.pb.issue'));
    }
  };
  const checkSubs = async (hls) => {
    const master = hls.replace(/index\.m3u8(\?.*)?$/, 'master.m3u8');
    replace(subs, h('span', { class: 'muted small' }, t('io.pb.checking')));
    try {
      const r = await fetch(master, { cache: 'no-store', credentials: 'omit' });
      if (!r.ok) {
        replace(subs, h('p', { class: 'muted small', 'data-subtitles': '0' }, t('io.pb.noTracksHttp', { status: r.status })));
        return;
      }
      const list = parseSubtitleMedia(await r.text(), master);
      replace(subs,
        urlField(t('io.pb.master'), master, t('io.pb.masterUrl')),
        list.length ? dataTable({
          caption: t('io.pb.tracks'),
          rows: list,
          columns: [
            { label: t('io.cc.languageShort'), render: (x) => h('code', null, x.language) },
            { label: t('common.name'), render: (x) => x.name },
            { label: t('io.pb.default'), render: (x) => (x.default ? '○' : '') },
            { label: t('io.pb.playlist'), wrap: true, render: (x) => (x.uri ? copyField(x.uri, t('io.pb.trackUrl', { lang: x.language })) : '—') },
          ],
        }) : h('p', { class: 'muted small', 'data-subtitles': '0' }, t('io.pb.noTracksYet')));
      if (list.length) subs.dataset.subtitles = String(list.length);
    } catch (e) {
      replace(subs, h('p', { class: 'muted small' }, t('io.pb.checkFailed', { reason: e.message })));
    }
  };
  const el = h('div', null,
    h('div', { class: 'toolbar' }, button(t('io.pb.issue'), { icon: 'link', onClick: issue }), ttl),
    box, h('h3', null, t('io.pb.tracks')), subs);
  return { el };
}
