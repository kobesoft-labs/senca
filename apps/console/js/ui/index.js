// 部品の入口。画面からは `import { button, pageHeader, … } from '../ui/index.js'` で使う。
// 見本は #/_styleguide（js/views/styleguide.js）、決まりは apps/console/README.md。

export { button, iconButton, setLoading, buttonGroup } from './button.js';
export { badge, statusPill, STATUS_KINDS, alert, emptyState, skeleton, loadingLine, meter } from './feedback.js';
export { breadcrumbs, pageHeader, panel, statTile, kvList } from './layout.js';
export { dataTable, pagination } from './table.js';
export { tooltip, modal, confirmDialog, attachMenu, attachPopover, helpHint, closeFloating } from './overlay.js';
export { formField, toggle, segmented, searchBox, copyText, copyField } from './form.js';
export { tabs } from './tabs.js';
export { toast } from './toast.js';
export { checklist, checklistDismissed } from './onboarding.js';
export { icon, ICON_NAMES } from '../lib/icons.js';
export { codeBlock } from './code.js';
