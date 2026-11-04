// パイプラインの入力・出力に使う stream を選ぶ部品（検査器とひな形の作成で使う）。
//
// - 選ぶ欄: 配信中のものを先に（コーデックと大きさを添えて）、ほかは後に。今の値が一覧に無ければそのまま残す。
// - 新しい stream を作るダイアログ（出力用。AV1・VP9 を HLS で出せるように fMP4 で作る）。

import { h } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { modal, toast, reportError } from '../lib/ui.js';
import { formField, segmented } from '../ui/index.js';
import { streams as api } from '../api/resources.js';
import { describeTracks } from './model.js';

export const NEW_STREAM = '__new__';

/**
 * stream を選ぶ <select>。kind = 'input' なら配信中のものを先に出す。
 * @param {object[]} list GET /streams の items
 * @param {string} value
 * @param {{kind?: 'input'|'output', allowNew?: boolean, attrs?: object}} [o]
 */
export function streamSelect(list, value, { kind = 'input', allowNew = false, attrs = {} } = {}) {
  const live = list.filter((x) => x.observed_state === 'live');
  const other = list.filter((x) => x.observed_state !== 'live');
  const opt = (x) => h('option', { value: x.id, selected: x.id === value },
    `${x.name}${x.observed_state === 'live' ? ` — ${t('pipelines.stream.liveNow')}${x.live && x.live.tracks ? ` · ${describeTracks(x.live.tracks)}` : ''}` : ''}${x.codec_profile === 'royalty_free' ? ` · ${t('pipelines.stream.rf')}` : ''}`);
  const groups = kind === 'input'
    ? [[t('pipelines.stream.groupLive'), live], [t('pipelines.stream.groupOther'), other]]
    : [[t('pipelines.stream.groupIdle'), other], [t('pipelines.stream.groupLive'), live]];
  return h('select', attrs,
    h('option', { value: '', selected: !value }, t('pipelines.stream.choose')),
    ...groups.filter(([, xs]) => xs.length).map(([label, xs]) => h('optgroup', { label }, xs.map(opt))),
    value && !list.some((x) => x.id === value) ? h('option', { value, selected: true }, t('pipelines.stream.unknown', { id: value.slice(0, 8) })) : null,
    allowNew ? h('option', { value: NEW_STREAM }, t('pipelines.stream.createNew')) : null);
}

/**
 * 新しい stream を作る（出力用）。作れたら stream を返す。
 * @param {{suggest?: string, royaltyFree?: boolean}} [o]
 */
export async function createStreamDialog({ suggest = '', royaltyFree = true } = {}) {
  const name = h('input', { type: 'text', value: suggest, maxlength: 64, autocomplete: 'off', spellcheck: 'false', 'data-testid': 'new-stream-name' });
  const profile = segmented({ options: [['royalty_free', t('pipelines.stream.profileRf')], ['any', t('pipelines.stream.profileAny')]], value: royaltyFree ? 'royalty_free' : 'any', label: t('pipelines.stream.profile') });
  const nameField = formField({ label: t('pipelines.stream.name'), input: name, required: true, hint: t('pipelines.stream.nameHint') });
  let created = null;
  const submit = async (close) => {
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(name.value)) { nameField.setError(t('pipelines.stream.nameInvalid')); name.focus(); return; }
    try {
      const { data } = await api.create({ name: name.value, codec_profile: profile.value, hls: { container: 'fmp4' } });
      created = data;
      toast(t('pipelines.stream.created', { name: data.name }), 'ok');
      close(true);
    } catch (e) {
      if (e.status === 409) nameField.setError(t('pipelines.stream.nameTaken'));
      else reportError(e, t('pipelines.stream.create'));
    }
  };
  await modal(t('pipelines.stream.createTitle'), (close) => {
    name.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); submit(close); } });
    name.addEventListener('input', () => nameField.setError(null));
    return [
      h('p', { class: 'muted small' }, t('pipelines.stream.createLead')),
      h('div', { class: 'form' }, nameField, h('div', { class: 'field' }, h('span', { class: 'label' }, t('pipelines.stream.profile')), profile, h('div', { class: 'hint' }, t('pipelines.stream.profileHint')))),
    ];
  }, {
    icon: 'streams', testid: 'new-stream-dialog',
    actions: (close) => [
      h('button', { type: 'button', class: 'btn', onClick: () => close(false) }, t('common.cancel')),
      h('button', { type: 'button', class: 'btn primary', 'data-testid': 'new-stream-submit', onClick: () => submit(close) }, t('pipelines.stream.create')),
    ],
  });
  return created;
}
