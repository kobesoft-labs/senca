// User-Agent からブラウザーと OS のおおまかな名前を読む（セッションの一覧で「どの端末か」を出すため）。
// 細かな版や機種は出さない。分からなければ null。画面に依存しない。

const BROWSERS = [
  [/\bEdg(?:e|A|iOS)?\/(\d+)/, 'Edge'],
  [/\bOPR\/(\d+)/, 'Opera'],
  [/\bSamsungBrowser\/(\d+)/, 'Samsung Internet'],
  [/\bFirefox\/(\d+)|\bFxiOS\/(\d+)/, 'Firefox'],
  [/\bCriOS\/(\d+)/, 'Chrome'],
  [/\b(?:Headless)?Chrome\/(\d+)/, 'Chrome'],
  [/\bVersion\/(\d+)[^ ]* (?:Mobile\/\S+ )?Safari\//, 'Safari'],
  [/^curl\//, 'curl'],
  [/\bsenca-(?:sdk|client|js)\b/i, 'SENCA SDK'],
  [/^python-requests\//, 'Python'],
  [/^node-fetch|\bundici\b|^Node\.js/, 'Node.js'],
];

const SYSTEMS = [
  [/\biPhone\b/, 'iOS', 'phone'],
  [/\biPad\b/, 'iPadOS', 'tablet'],
  [/\bAndroid\b.*\bMobile\b/, 'Android', 'phone'],
  [/\bAndroid\b/, 'Android', 'tablet'],
  [/\bCrOS\b/, 'ChromeOS', 'desktop'],
  [/\bWindows\b/, 'Windows', 'desktop'],
  [/\bMac OS X\b|\bMacintosh\b/, 'macOS', 'desktop'],
  [/\bLinux\b/, 'Linux', 'desktop'],
];

/**
 * @param {string|null} ua
 * @returns {{browser: string|null, os: string|null, kind: 'phone'|'tablet'|'desktop'|'tool'|null}}
 */
export function parseUserAgent(ua) {
  if (!ua) return { browser: null, os: null, kind: null };
  const b = BROWSERS.find(([re]) => re.test(ua));
  const s = SYSTEMS.find(([re]) => re.test(ua));
  const browser = b ? b[1] : null;
  const tool = ['curl', 'SENCA SDK', 'Python', 'Node.js'].includes(browser);
  return { browser, os: s ? s[1] : null, kind: tool ? 'tool' : s ? s[2] : null };
}
