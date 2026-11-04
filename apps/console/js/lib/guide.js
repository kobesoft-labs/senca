// 手引き（docs/guide/{ja,en}/）への外向きのリンク。管理画面が外から読み込むものではない（新しいタブで開くだけ）。
// 置き場所を変えたときはここだけを直す（tools/interop/console.sh の B は、このファイルの URL だけを許す）。

import { getLang } from '../i18n/index.js';

export const GUIDE_BASE = 'https://github.com/kobesoft-labs/senca/blob/main/docs/guide';

/** '05-recording.md#…' → 今の言語の手引きの URL。 */
export function guideUrl(page) {
  return `${GUIDE_BASE}/${getLang() === 'ja' ? 'ja' : 'en'}/${page}`;
}
