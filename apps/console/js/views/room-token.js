// 参加トークンの発行（POST /rooms/{id}/tokens）。役割・名前・期限・権限を選んで発行し、
// トークン・接続先と、そのまま試せる SDK のコード・アプリのサーバーで発行する curl を出す。
// トークンは画面にだけ出し、保存しない（閉じたら消える）。

import { h, replace, select } from '../lib/dom.js';
import { errorBox } from '../lib/ui.js';
import { t } from '../i18n/index.js';
import { modal, formField, segmented, copyField, alert, codeBlock, tabs } from '../ui/index.js';
import { rooms } from '../api/realtime.js';
import { BASE } from '../api/client.js';
import { fmtTime, fmtRelative } from '../lib/format.js';
import { tokenBody, sdkSnippet, tokenCurl, signalingUrl, splitList } from '../lib/realtime.js';
import { disclosure } from './room-form.js';

export const TTLS = [[600, 'rooms.token.ttl10m'], [3600, 'rooms.token.ttl1h'], [28800, 'rooms.token.ttl8h'], [86400, 'rooms.token.ttl24h']];

/** 発行のダイアログ。発行したら RoomToken を返す（閉じただけなら undefined）。 */
export function tokenDialog(room) {
  const role = segmented({
    options: [['sendrecv', t('rooms.role.sendrecv')], ['sendonly', t('rooms.role.sendonly')], ['recvonly', t('rooms.role.recvonly')]],
    value: 'sendrecv', label: t('rooms.token.role'), testid: 'token-role',
  });
  const name = h('input', { type: 'text', maxlength: 100, placeholder: t('rooms.token.namePlaceholder'), 'data-testid': 'token-name' });
  const clientId = h('input', { type: 'text', maxlength: 255, spellcheck: 'false', autocomplete: 'off', placeholder: 'alice', 'data-testid': 'token-client-id' });
  const ttl = select(TTLS.map(([v, k]) => [String(v), t(k)]), '3600', { 'data-testid': 'token-ttl' });
  const pubAudio = h('input', { type: 'checkbox', checked: true });
  const pubVideo = h('input', { type: 'checkbox', checked: true });
  const subscribe = h('input', { type: 'checkbox', checked: true });
  const data = h('input', { type: 'text', value: '#*', spellcheck: 'false' });
  const errBox = h('div');
  const body = h('div', { class: 'form' },
    formField({ label: t('rooms.token.role'), input: role, hint: t('rooms.token.roleHint') }),
    h('div', { class: 'cols' },
      formField({ label: t('rooms.token.name'), input: name, optional: true, hint: t('rooms.token.nameHint') }),
      formField({ label: 'client_id', input: clientId, optional: true, hint: t('rooms.token.clientIdHint') })),
    formField({ label: t('rooms.token.ttl'), input: ttl, hint: t('rooms.token.ttlHint') }),
    disclosure(t('rooms.token.permissions'), t('rooms.token.permissionsDesc'),
      h('fieldset', null, h('legend', null, t('rooms.token.publish')),
        h('div', { class: 'row' }, h('label', { class: 'check' }, pubVideo, t('rooms.kind.video')), h('label', { class: 'check' }, pubAudio, t('rooms.kind.audio')))),
      h('label', { class: 'check' }, subscribe, t('rooms.token.subscribe')),
      formField({ label: t('rooms.token.data'), input: data, hint: t('rooms.token.dataHint') })),
    errBox);
  const result = h('div', { hidden: true, 'data-testid': 'token-result' });
  let issued;

  const showResult = (tok, req) => {
    const url = signalingUrl(tok.signaling_url);
    const apiBase = `${location.origin}${BASE}`;
    replace(result,
      alert('ok', t('rooms.token.issuedBody', { time: fmtTime(tok.expires_at), rel: fmtRelative(tok.expires_at) }), { title: t('rooms.token.issued') }),
      h('div', { class: 'form mt-4' },
        h('div', { class: 'field' }, h('div', { class: 'label' }, t('rooms.token.token')), copyField(tok.token || '', t('rooms.token.token'), { secret: true, testid: 'token-value' }),
          h('div', { class: 'hint' }, t('rooms.token.tokenHint'))),
        h('div', { class: 'field' }, h('div', { class: 'label' }, t('rooms.token.signaling')), copyField(url || '', t('rooms.token.signaling'), { testid: 'token-signaling' }),
          tok.signaling_url ? null : h('div', { class: 'hint' }, t('rooms.token.signalingGuess'))),
        tabs({
          label: t('rooms.token.howTo'), param: null,
          tabs: [
            { id: 'sdk', label: t('rooms.token.sdk'), icon: 'code', render: () => h('div', { class: 'stack mt-2' }, h('p', { class: 'small muted' }, t('rooms.token.sdkHint')), codeBlock(sdkSnippet({ url, room: room.id, token: tok.token, role: role.value }), { label: t('rooms.token.sdk'), lang: 'js', testid: 'token-sdk' })) },
            { id: 'server', label: t('rooms.token.server'), icon: 'key', render: () => h('div', { class: 'stack mt-2' }, h('p', { class: 'small muted' }, t('rooms.token.serverHint')), codeBlock(tokenCurl({ apiBase, room: room.id, body: req }), { label: t('rooms.token.server'), lang: 'sh' })) },
          ],
        })));
    body.hidden = true;
    result.hidden = false;
  };

  return modal(t('rooms.token.title', { room: room.id }), () => [body, result], {
    wide: true, icon: 'user-plus', testid: 'token-dialog',
    actions: (close) => {
      const cancel = h('button', { type: 'button', onClick: () => close(issued) }, t('common.cancel'));
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'token-issue', onClick: async () => {
        const publish = [pubAudio.checked && 'audio', pubVideo.checked && 'video'].filter(Boolean);
        const req = tokenBody({ role: role.value, clientId: clientId.value, name: name.value, ttl: Number(ttl.value), publish, subscribe: subscribe.checked, data: splitList(data.value) });
        ok.disabled = true;
        replace(errBox);
        try {
          const { data: tok } = await rooms.token(room.id, req);
          issued = tok;
          showResult(tok, req);
          ok.hidden = true;
          cancel.textContent = t('common.close');
          cancel.classList.add('primary');
          cancel.focus();
        } catch (e) {
          replace(errBox, errorBox(e, { title: t('rooms.token.failed') }));
        } finally {
          ok.disabled = false;
        }
      } }, t('rooms.token.issue'));
      return [cancel, ok];
    },
  });
}
