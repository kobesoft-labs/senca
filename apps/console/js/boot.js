// 最初の描画の前に明暗と言語を <html> に入れる（ちらつかないように、モジュールより先に同期で読む）。
// 同じ決まりを js/lib/theme.js と js/i18n/index.js が持つ（ここは ES modules を使わない小さな写し）。
(function () {
  var root = document.documentElement;
  var pref = 'system';
  var lang = null;
  try {
    pref = localStorage.getItem('senca.console.theme') || 'system';
    lang = localStorage.getItem('senca.console.lang');
  } catch (e) { /* 使えなければ既定 */ }
  var dark = pref === 'dark' || (pref !== 'light' && window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches);
  root.setAttribute('data-theme', dark ? 'dark' : 'light');
  root.setAttribute('data-theme-pref', pref);
  if (lang !== 'ja' && lang !== 'en') lang = /^ja\b/i.test(navigator.language || '') ? 'ja' : 'en';
  root.lang = lang;
}());
