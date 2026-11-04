// ja の辞書。画面ごとのファイル（鍵は画面の名前で始める: streams.・recordings. など）をまとめる。
// 画面を足すときは ja と en の両方に同じ名前のファイルを足し、ここに 1 行ずつ足す。
import core from './core.js';
import shell from './shell.js';
import styleguide from './styleguide.js';
import dashboard from './dashboard.js';
import streams from './streams.js';
import recordings from './recordings.js';
import schedules from './schedules.js';
import send from './send.js';
import io from './io.js';
import player from './player.js';
import pipelines from './pipelines.js';
import production from './production.js';
import nodes from './nodes.js';
import tokens from './tokens.js';
import secrets from './secrets.js';
import activity from './activity.js';
import grid from './grid.js';
import audit from './audit.js';

export default {
  ...core,
  ...shell,
  ...styleguide,
  ...dashboard,
  ...streams,
  ...recordings,
  ...schedules,
  ...send,
  ...io,
  ...player,
  ...pipelines,
  ...production,
  ...nodes,
  ...tokens,
  ...secrets,
  ...activity,
  ...grid,
  ...audit,
};
