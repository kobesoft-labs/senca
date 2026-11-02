// 設定文字列の読み取りと Link ヘッダーの試験（node --test で動く。ブラウザの API は使わない部分だけ）。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ConfigError, parseIngest } from '../config.js';
import { parseIceServerLinks } from '../whip.js';

test('senca: WHIP の設定を読む', () => {
  const c = parseIngest('senca://ingest?v=1&proto=whip&url=https://h.example/whip/abc&token=eyJ.a.b&name=%E3%82%AB%E3%83%A1%E3%83%A9');
  assert.deepEqual(c, { url: 'https://h.example/whip/abc', token: 'eyJ.a.b', name: 'カメラ' });
  assert.equal(parseIngest('https://h.example/whip/x').url, 'https://h.example/whip/x');
  assert.equal(parseIngest('http://127.0.0.1:8080/whip/x').url, 'http://127.0.0.1:8080/whip/x');
  assert.ok(parseIngest('senca://ingest?v=1&proto=whip&url=https://h/whip/x&x-future=1'));
});

test('厳格に拒否する', () => {
  const bad = [
    'senca://ingest?proto=whip&url=https://h/w',
    'senca://ingest?v=2&proto=whip&url=https://h/w',
    'senca://ingest?v=1&v=1&proto=whip&url=https://h/w',
    'senca://ingest?v=1&proto=whip&url=https://h/w&bogus=1',
    'senca://ingest?v=1&proto=whip&url=http://h/w',
    'senca://ingest?v=1&proto=whip&url=https://u:p@h/w',
    'senca://ingest?v=1&proto=whip&url=https://h/w%23x',
    'senca://ingest?v=1&proto=whip&url=https://h/',
    'senca://ingest?v=1&proto=whip&url=https://h/w&token=',
    'senca://ingest?v=1&proto=whip&url=https://h/w&token=a%20b',
    'senca://ingest?v=1&proto=whip&url=https://h/w&name=%0a',
    'senca://ingest?v=1&proto=whip&url=https://h/w&name=%zz',
    'senca://ingest?v=1&proto=srt&host=h&port=1',
    'srt://h:9000',
    'rtmp://h/live/k',
    'ftp://h/x',
    'x'.repeat(3000),
  ];
  for (const s of bad) assert.throws(() => parseIngest(s), ConfigError, s);
  assert.throws(() => parseIngest('senca://ingest?v=1&proto=srt&host=h&port=1'), /SRT/);
});

test('Link ヘッダーの ICE サーバー', () => {
  const h = '<stun:stun.example.net>; rel="ice-server", <turn:turn.example.net?transport=udp>; rel="ice-server"; username="u1"; credential="p,1"; credential-type="password", <https://x>; rel="other"';
  assert.deepEqual(parseIceServerLinks(h), [
    { urls: 'stun:stun.example.net' },
    { urls: 'turn:turn.example.net?transport=udp', username: 'u1', credential: 'p,1' },
  ]);
  assert.deepEqual(parseIceServerLinks(null), []);
});
