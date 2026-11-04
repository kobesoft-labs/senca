// 秘密値（入力・出力の鍵やパスワード、PaaS の API トークン、字幕の外部サービスの鍵）。tenant:admin が要る。
//
// 値は書くだけ: ノードは暗号化して保存し、API は値を返さない。この画面も値を保存・表示しない（送ったら欄を空にする）。
// 参照は `secret:{name}`（入力・出力の credential_ref、字幕の *_credential_ref）。使われている秘密値は消せない（409）。

import { h, replace } from '../lib/dom.js';
import { load, confirmDialog, reportError, errorBox, toast } from '../lib/ui.js';
import { secrets } from '../api/resources.js';
import { fmtTime } from '../lib/format.js';
import { t } from '../i18n/index.js';
import { pageHeader, panel, dataTable, emptyState, formField, copyField, alert, helpHint } from '../ui/index.js';
import { icon } from '../lib/icons.js';

const NAME_RE = /^[A-Za-z0-9._-]{1,64}$/;

export async function render(root) {
  const box = h('div');
  const name = h('input', { type: 'text', maxlength: 64, autocomplete: 'off', spellcheck: 'false', placeholder: t('secrets.namePlaceholder') });
  const value = h('input', { type: 'password', autocomplete: 'new-password', spellcheck: 'false' });
  const errBox = h('div');
  const nameField = formField({ label: t('common.name'), input: name, required: true, hint: t('secrets.nameHint') });
  const valueField = formField({ label: t('secrets.value'), input: value, required: true, hint: t('secrets.valueHint') });

  const remove = async (x) => {
    const ok = await confirmDialog(t('secrets.deleteTitle'), t('secrets.deleteBody', { name: x.name }), {
      ok: t('common.delete'),
      consequences: [t('secrets.deleteC1'), t('secrets.deleteC2')],
    });
    if (!ok) return;
    try {
      await secrets.remove(x.id);
      toast(t('common.deleted'), 'ok');
      refresh();
    } catch (e) {
      if (e.status === 409) toast(t('secrets.inUse', { reason: e.title }), 'warn', 8000);
      else reportError(e, t('common.delete'));
    }
  };

  const refresh = () => load(box, async () => {
    const { data } = await secrets.list();
    return dataTable({
      caption: t('secrets.caption'),
      sort: { key: 'name', dir: 'asc' },
      rowKey: (x) => x.id,
      columns: [
        { key: 'name', label: t('common.name'), primary: true, sort: (x) => x.name, render: (x) => h('strong', null, x.name) },
        { label: t('secrets.ref'), wrap: true, render: (x) => copyField(x.ref, t('secrets.refLabel', { name: x.name })) },
        { key: 'created', label: t('common.created'), sort: (x) => x.created_at, render: (x) => fmtTime(x.created_at) },
        { label: t('common.actions'), actions: true, render: (x) => h('button', {
          type: 'button', class: 'sm danger icon-only', 'aria-label': t('secrets.deleteLabel', { name: x.name }), title: t('common.delete'), onClick: () => remove(x),
        }, icon('trash')) },
      ],
      rows: data.items,
      empty: { icon: 'secrets', title: t('secrets.emptyTitle'), body: t('secrets.emptyBody') },
    });
  }, { title: t('secrets.loadFailed'), kind: 'table' });

  const create = async (ev) => {
    ev.preventDefault();
    replace(errBox);
    nameField.setError(null);
    valueField.setError(null);
    let bad = false;
    if (!NAME_RE.test(name.value)) { nameField.setError(t('secrets.nameInvalid')); bad = true; }
    if (!value.value) { valueField.setError(t('secrets.valueEmpty')); bad = true; }
    if (bad) { (NAME_RE.test(name.value) ? value : name).focus(); return; }
    const v = value.value;
    value.value = '';
    try {
      const { data } = await secrets.create({ name: name.value, value: v });
      toast(t('secrets.created', { ref: data.ref }), 'ok');
      name.value = '';
      refresh();
    } catch (e) {
      replace(errBox, errorBox(e, { title: t('secrets.createFailed') }));
    }
  };

  root.append(
    pageHeader({
      title: t('nav.secrets'),
      description: t('secrets.lead'),
      help: helpHint({ title: t('nav.secrets'), body: t('secrets.help'), guide: '06-inputs-outputs.md' }),
    }),
    alert('info', t('secrets.writeOnly')),
    panel({ title: t('secrets.new'), icon: 'plus' },
      h('form', { class: 'form', onSubmit: create },
        h('div', { class: 'cols' }, nameField, valueField),
        errBox, h('div', { class: 'actions' }, h('button', { class: 'primary', type: 'submit' }, icon('save'), h('span', null, t('common.create')))))),
    panel({ title: t('secrets.list'), icon: 'secrets' }, box));
  refresh();
  return () => { value.value = ''; };
}
