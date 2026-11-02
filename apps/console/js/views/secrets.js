// 秘密値（入力・出力の鍵やパスワード、PaaS の API トークン、字幕の外部サービスの鍵）。tenant:admin が要る。
//
// 値は書くだけ: ノードは暗号化して保存し、API は値を返さない。この画面も値を保存・表示しない（送ったら欄を空にする）。
// 参照は `secret:{name}`（入力・出力の credential_ref、字幕の *_credential_ref）。使われている秘密値は消せない（409）。

import { h, replace, table, field } from '../lib/dom.js';
import { load, confirmDialog, reportError, errorBox, toast, copyField } from '../lib/ui.js';
import { secrets } from '../api/resources.js';
import { fmtTime } from '../lib/format.js';

export async function render(root) {
  const box = h('div');
  const name = h('input', { type: 'text', maxlength: 64, autocomplete: 'off', spellcheck: 'false', placeholder: '例: youtube-key' });
  const value = h('input', { type: 'password', autocomplete: 'new-password', spellcheck: 'false' });
  const errBox = h('div');

  const refresh = () => load(box, async () => {
    const { data } = await secrets.list();
    return table([
      { label: '名前', render: (x) => h('strong', null, x.name) },
      { label: '参照', render: (x) => copyField(x.ref, `${x.name} の参照`) },
      { label: '作成', render: (x) => fmtTime(x.created_at) },
      { label: '', render: (x) => h('button', { class: 'small danger', 'aria-label': `秘密値 ${x.name} を削除`, onClick: async () => {
        if (!(await confirmDialog('秘密値を削除', `「${x.name}」を削除します。入力・出力が使っていれば消せません。`, { ok: '削除' }))) return;
        try {
          await secrets.remove(x.id);
          toast('削除しました', 'ok');
          refresh();
        } catch (e) {
          if (e.status === 409) toast(`消せません: ${e.title}`, 'warn', 8000);
          else reportError(e, '削除');
        }
      } }, '削除') },
    ], data.items, { empty: '秘密値はありません', caption: '秘密値の一覧' });
  }, { title: '秘密値を読めません（tenant:admin が要ります）' });

  const create = async (ev) => {
    ev.preventDefault();
    replace(errBox);
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(name.value)) { replace(errBox, h('div', { class: 'alert error' }, '名前は英数字と . _ - の 1〜64 文字です')); return; }
    if (!value.value) { replace(errBox, h('div', { class: 'alert error' }, '値を入れてください')); return; }
    const v = value.value;
    value.value = '';
    try {
      const { data } = await secrets.create({ name: name.value, value: v });
      toast(`作りました。参照は ${data.ref}`, 'ok');
      name.value = '';
      refresh();
    } catch (e) {
      replace(errBox, errorBox(e, { title: '作れませんでした' }));
    }
  };

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, '秘密値')),
    h('div', { class: 'alert info small' }, '値は書くだけです。ノードは暗号化して保存し、API でもこの画面でも読み出せません。変えるときは新しい名前で作り直して参照を差し替えます。'),
    h('section', { class: 'panel', 'aria-labelledby': 'sec-new' }, h('h2', { id: 'sec-new' }, '秘密値を作る'),
      h('form', { class: 'form', onSubmit: create },
        field('名前', name), field('値（表示・保存しません）', value, 'RTMP の鍵、SRT のパスフレーズ、PaaS・字幕サービスの API キーなど'),
        errBox, h('div', { class: 'actions' }, h('button', { class: 'primary', type: 'submit' }, '作成')))),
    h('section', { class: 'panel', 'aria-labelledby': 'sec-list' }, h('h2', { id: 'sec-list' }, '一覧'), box));
  refresh();
  return () => { value.value = ''; };
}
