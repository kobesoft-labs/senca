// 準備中のページ（メニューにはあるが、まだ画面が無いもの）。js/nav.js で soon: true の項目がここへ来る。

import { h } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { emptyState, pageHeader } from '../ui/index.js';
import { guideUrl } from '../lib/guide.js';
import { icon } from '../lib/icons.js';
import { navItem } from '../nav.js';

const INFO = {
  rooms: { api: '/api/v1/rooms', guide: '10-rooms.md' },
  'media-pipelines': { api: '/api/v1/media-pipelines', guide: '11-media-pipelines.md' },
  grid: { api: '/api/v1/grid', guide: '09-cluster-grid.md' },
  audit: { api: null, guide: '13-operations.md' },
  users: { api: null, guide: '12-security.md' },
};

export async function render(root, _params, { nav }) {
  const item = navItem(nav);
  const info = INFO[nav] || { api: null, guide: 'README.md' };
  const title = t(`nav.${nav}`);
  root.append(
    pageHeader({ title, badges: [h('span', { class: 'tag' }, t('nav.soon'))] }),
    h('section', { class: 'panel coming-soon', 'data-testid': 'coming-soon' },
      emptyState({
        icon: item ? item.icon : 'rocket',
        title: t('soon.title', { page: title }),
        body: info.api ? t('soon.body', { api: info.api }) : t('soon.bodyNoApi'),
        actions: [
          h('a', { class: 'btn primary', href: guideUrl(info.guide), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('soon.guide'), icon('external')),
          h('a', { class: 'btn', href: '#/' }, t('soon.back')),
        ],
      })));
  return null;
}
