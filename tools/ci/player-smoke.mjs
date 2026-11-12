#!/usr/bin/env node
// Real UI -> isolated Rust daemon -> FFmpeg WebM preview -> Chromium playback.
// Requires Node 22+, FFmpeg, a built vice binary/UI, and Chromium, Brave or Helium.
// Unexpected browser-initiated reloads are failures with CDP diagnostics, never
// retried: some Helium headless builds reload new tabs during browser startup.
import assert from 'node:assert/strict';
import {spawn, spawnSync} from 'node:child_process';
import {once} from 'node:events';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const binary = path.resolve(process.env.VICE_SMOKE_BINARY || path.join(repo, 'target/debug/vice'));
const browserBinary = process.env.VICE_SMOKE_BROWSER || ['chromium', 'chromium-browser', 'google-chrome', 'brave', 'brave-browser', 'helium-browser'].find(name => spawnSync('which', [name]).status === 0);
assert(browserBinary, 'Set VICE_SMOKE_BROWSER to a Chromium-compatible browser executable.');
await fs.access(binary);
const root = await fs.mkdtemp(path.join(os.tmpdir(), 'vice-player-smoke-'));
const screenshot = process.env.VICE_SMOKE_SCREENSHOT || path.join(os.tmpdir(), 'vice-player-smoke.png');
let daemon, browser, cdp;
let daemonLog = '', browserLog = '';
const diagnostics = [];
const original = ["auto", "original"].includes(process.env.VICE_SMOKE_MODE);
const autoPreview = original;
const forceWebm = !autoPreview && process.env.VICE_SMOKE_MODE !== "small";
const previewQuery = forceWebm ? "webm" : "small";
const activity = [];
let mainNavigations = 0;
let viewerOpened = false;
let unexpectedReload = false;
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
async function freePort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const {port} = server.address();
  await new Promise(resolve => server.close(resolve));
  return port;
}
async function until(fn, message, timeout = 30000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    if (unexpectedReload) throw new Error('Browser unexpectedly reloaded the main document during playback');
    try { const value = await fn(); if (value) return value; } catch (error) { last = error; }
    await delay(100);
  }
  throw new Error(`${message}${last ? `: ${last.message}` : ''}`);
}
class CDP {
  constructor(url) {
    this.socket = new WebSocket(url);
    this.next = 0;
    this.pending = new Map();
    this.listeners = new Map();
    this.ready = once(this.socket, 'open');
    this.socket.addEventListener('message', ({data}) => {
      const packet = JSON.parse(String(data));
      if (packet.id) {
        const pending = this.pending.get(packet.id);
        if (!pending) return;
        this.pending.delete(packet.id);
        clearTimeout(pending.timer);
        if (packet.error) pending.reject(new Error(JSON.stringify(packet.error)));
        else pending.resolve(packet.result);
      } else for (const handler of this.listeners.get(packet.method) || []) handler(packet.params);
    });
  }
  on(method, handler) {
    if (!this.listeners.has(method)) this.listeners.set(method, []);
    this.listeners.get(method).push(handler);
  }
  async send(method, params = {}) {
    await this.ready;
    const id = ++this.next;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`CDP ${method} timed out`)); }, 20000);
      this.pending.set(id, {resolve, reject, timer});
      this.socket.send(JSON.stringify({id, method, params}));
    });
  }
  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', {expression, returnByValue: true, awaitPromise: true});
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  }
  close() {
    this.socket.close();
    for (const pending of this.pending.values()) { clearTimeout(pending.timer); pending.reject(new Error('CDP closed')); }
    this.pending.clear();
  }
}
async function stop(child) {
  if (!child || child.exitCode !== null) return;
  child.kill('SIGTERM');
  await Promise.race([once(child, 'exit').catch(() => {}), delay(5000)]);
  if (child.exitCode === null) { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }
}
try {
  for (const name of ['run', 'clips', 'images', 'bin', 'tmp']) await fs.mkdir(path.join(root, name));
  const fixture = spawnSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i', `testsrc2=size=${original ? '1280x720:rate=60' : '320x180:rate=15'}`, '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000', '-t', '8', '-c:v', 'libx264', '-preset', 'ultrafast', ...(original ? ['-b:v', '45M', '-minrate', '45M', '-maxrate', '45M', '-bufsize', '90M', '-x264-params', 'nal-hrd=cbr:force-cfr=1'] : []), '-threads', '1', '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-movflags', '+faststart', '-y', path.join(root, 'clips/smoke.mp4')], {encoding: 'utf8'});
  assert.equal(fixture.status, 0, fixture.stderr);
  if (autoPreview) {
    const file = await fs.open(path.join(root, 'clips/smoke.mp4'), 'r+');
    await file.truncate(70 * 1024 * 1024);
    await file.close();
  }
  // This executable only answers capability probes and sleeps. It captures
  // no screen, audio, or input, and is never installed outside the temp tree.
  await fs.writeFile(path.join(root, 'bin/gpu-screen-recorder'), `#!/bin/sh
case "$1" in
--info) printf 'section=video_codecs\\nh264\\n'; exit 0;;
--help) printf '%s\\n' '-w -f -r -c -a -o'; exit 0;;
--list-*) exit 0;;
esac
trap 'printf partial > "$VICE_SMOKE_CLIPS/2026-replay.mp4"; sleep 2; cp "$VICE_SMOKE_FIXTURE" "$VICE_SMOKE_CLIPS/2026-replay.mp4"' USR1
trap 'exit 0' INT TERM
while :; do sleep 0.1 & wait $!; done
`, {mode: 0o755});
  const ffmpegBinary = spawnSync('which', ['ffmpeg'], {encoding: 'utf8'}).stdout.trim();
  // Slow only preview encodes to real time. Playback must start while the
  // output is still incomplete, rather than passing from a completed cache.
  await fs.writeFile(path.join(root, 'bin/ffmpeg'), `#!/bin/sh
case "$*" in
  *".vice-stream-"*) exec '${ffmpegBinary.replaceAll("'", "'\\''")}' -re "$@" ;;
  *) exec '${ffmpegBinary.replaceAll("'", "'\\''")}' "$@" ;;
esac
`, {mode: 0o755});
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  const config = `[recording]\nbackend="gsr"\ngsr_source="native"\ncapture_audio=false\n[output]\ndirectory=${JSON.stringify(path.join(root, 'clips'))}\nimage_directory=${JSON.stringify(path.join(root, 'images'))}\nauto_playlist_by_game=false\n[sharing]\nenabled=false\ncloudflare_tunnel=false\nport=${port}\n[discord]\nenabled=false\n[updates]\ncheck_on_start=false\n[hotkeys]\nclip=""\n[notifications]\nsound_volume=0.0\n`;
  await fs.writeFile(path.join(root, 'config.toml'), config);
  if (process.env.VICE_SMOKE_LIVE_TAGS === '1') {
    await fs.mkdir(path.join(root,'data'), {recursive:true});
    await fs.writeFile(path.join(root,'data/playlists.json'), JSON.stringify({version:1,dismissed_auto:[],playlists:[{
      id:'auto:live-tag',kind:'auto',name:'Live tag game',game:'Live tag game',emoji:'',icon:'',edited:true,
      color1:'#ff5722',color2:'#111111',clip_slugs:[],created_at:'2026-01-01T00:00:00'
    }]}));
  }
  const env = {...process.env, VICE_SMOKE_FIXTURE: path.join(root, 'clips/smoke.mp4'), VICE_SMOKE_CLIPS: path.join(root, 'clips'), VICE_CONFIG: path.join(root, 'config.toml'), VICE_DATA_DIR: path.join(root, 'data'), VICE_CACHE_DIR: path.join(root, 'cache'), VICE_RESOURCE_DIR: path.join(repo, 'vice'), XDG_RUNTIME_DIR: path.join(root, 'run'), TMPDIR: path.join(root, 'tmp'), PATH: `${path.join(root, 'bin')}:${process.env.PATH}`};
  delete env.INVOCATION_ID; delete env.JOURNAL_STREAM;
  daemon = spawn(binary, ['start', '--no-open-ui'], {env, detached: true, stdio: ['ignore', 'pipe', 'pipe']});
  daemon.stdout.on('data', data => { daemonLog += data; }); daemon.stderr.on('data', data => { daemonLog += data; });
  await until(async () => {
    if (daemon.exitCode !== null) throw new Error(`daemon exited ${daemon.exitCode}: ${daemonLog}`);
    const response = await fetch(`${base}/api/status`); return response.ok && (await response.json()).ready;
  }, 'isolated daemon did not become ready');
  await until(async () => {
    const response = await fetch(`${base}/api/clips/smoke`); const clip = await response.json();
    return clip.duration > 7 && clip.vcodec === 'h264';
  }, 'generated clip metadata did not become available');
  const browserPort = await freePort();
  browser = spawn(browserBinary, ['--headless', '--disable-gpu', '--disable-background-networking', '--disable-component-update', '--no-first-run', '--no-default-browser-check', ...(process.env.VICE_SMOKE_EXTENSIONS === '1' ? [] : ['--disable-extensions', '--disable-component-extensions-with-background-pages']), '--autoplay-policy=no-user-gesture-required', `--remote-debugging-port=${browserPort}`, `--user-data-dir=${path.join(root, 'browser')}`, 'about:blank'], {detached: true, stdio: ['ignore', 'pipe', 'pipe']});
  browser.stdout.on('data', data => { browserLog += data; }); browser.stderr.on('data', data => { browserLog += data; });
  await until(async () => (await fetch(`http://127.0.0.1:${browserPort}/json/version`)).ok, 'headless browser did not start');
  const target = await (await fetch(`http://127.0.0.1:${browserPort}/json/new?about:blank`, {method: 'PUT'})).json();
  cdp = new CDP(target.webSocketDebuggerUrl);
  await Promise.all([cdp.send('Page.enable'), cdp.send('Runtime.enable'), cdp.send('Network.enable'), cdp.send('Inspector.enable'), cdp.send('Log.enable')]);
  cdp.on('Runtime.exceptionThrown', value => diagnostics.push(value));
  cdp.on('Page.frameRequestedNavigation', value => activity.push({requestedNavigation:value}));
  cdp.on('Page.frameStartedNavigating', value => activity.push({startedNavigation:value}));
  cdp.on('Inspector.targetCrashed', value => activity.push({crash:value}));
  cdp.on('Page.frameNavigated', ({frame}) => {
    activity.push({navigation:frame.url,parent:frame.parentId,loader:frame.loaderId});
    if(!frame.parentId){mainNavigations++;if(viewerOpened&&mainNavigations>1)unexpectedReload=true;}
  });
  cdp.on('Log.entryAdded', ({entry})=>{if(entry.level==='error'||entry.level==='warning')activity.push({browserLog:entry});});
  cdp.on('Runtime.consoleAPICalled', event => {if(event.type==='error'||event.type==='warning')activity.push({console:event.type,args:event.args.map(v=>v.value||v.description)});});
  const requests = [];
  cdp.on('Network.requestWillBeSent', ({request,type,initiator,loaderId}) => {requests.push(request.url);if(type==='Document')activity.push({documentRequest:request.url,initiator,loader:loaderId});if(request.url.includes('/v/'))activity.push({mediaRequest:request.url});});
  cdp.on('Network.requestWillBeSentExtraInfo', value=>{if(value.headers['Sec-Fetch-Dest']==='document')activity.push({documentHeaders:value.headers});});
  cdp.on('Network.loadingFailed', value=>activity.push({requestFailed:value}));
  cdp.on('Network.responseReceived', ({response})=>{if(response.url.includes('/v/'))activity.push({mediaResponse:response.status,type:response.mimeType,url:response.url});});
  await cdp.send('Page.addScriptToEvaluateOnNewDocument', {source: `
    const originalCanPlay = HTMLMediaElement.prototype.canPlayType;
    HTMLMediaElement.prototype.canPlayType = function(type) {return ${forceWebm ? '/mp4|avc1|hvc1|hev1/i.test(type)' : 'false'} ? '' : originalCanPlay.call(this, type);};
    localStorage.setItem('vice_tutorial_shown','1');localStorage.setItem('vice_whats_new_seen','2.17.0');localStorage.setItem('vice-muted','1');
    window.__viceSmoke={bufferObserved:false};
    new MutationObserver(()=>{const bar=document.querySelector('.viewer-buffered');if(bar&&bar.getBoundingClientRect().width>0)window.__viceSmoke.bufferObserved=true;}).observe(document,{childList:true,subtree:true,attributes:true});
  `});
  // Make the waiting state reproducible even on a fast CPU and local disk.
  let thumbnailFailures = 0;
  const fetchPatterns = [{urlPattern: `*proxy=${previewQuery}*`, requestStage: 'Request'}];
  if (process.env.VICE_SMOKE_THUMBNAILS === '1') fetchPatterns.push({urlPattern: `${base}/t/*`, requestStage: 'Request'});
  await cdp.send('Fetch.enable', {patterns: fetchPatterns});
  cdp.on('Fetch.requestPaused', ({requestId, request}) => {
    if (process.env.VICE_SMOKE_THUMBNAILS === '1' && request.url.startsWith(`${base}/t/`)) {
      if (thumbnailFailures++ < 2) {
        void cdp.send('Fetch.fulfillRequest', {requestId, responseCode: 503, responseHeaders: [{name:'Cache-Control',value:'no-store'}], body:''}).catch(()=>{});
      } else void cdp.send('Fetch.continueRequest', {requestId}).catch(()=>{});
    } else void delay(800).then(() => cdp.send('Fetch.continueRequest', {requestId})).catch(() => {});
  });
  await cdp.send('Page.bringToFront');
  await cdp.send('Page.navigate', {url: base});
  await until(() => cdp.evaluate(`Boolean(document.querySelector('.clip-card .clip-duration')?.textContent && !document.querySelector('.tutorial'))`), 'clip card and duration did not render');
  if (forceWebm) assert.equal(await cdp.evaluate(`document.createElement('video').canPlayType('video/mp4; codecs="avc1.640028"')`), '');
  assert.notEqual(await cdp.evaluate(`document.createElement('video').canPlayType('video/webm; codecs="vp8, opus"')`), '');
  if (original) {
    // A large file over a link with latency must stay original and avoid any
    // conversion work. This catches the previous automatic 720p regression.
    await cdp.send('Network.emulateNetworkConditions', {offline: false, latency: 60, downloadThroughput: 12_500_000, uploadThroughput: 12_500_000});
  }
  let thumbnailRecovery = null;
  if (process.env.VICE_SMOKE_THUMBNAILS === '1') {
    await cdp.evaluate(`document.querySelector('.clip-card').scrollIntoView({block:'center'})`);
    const began = Date.now();
    await until(() => cdp.evaluate(`(()=>{const image=document.querySelector('.clip-thumb img');return image?.complete&&image.naturalWidth>0;})()`), 'thumbnail did not recover from transient busy responses', 5000);
    const dimensions = await cdp.evaluate(`(()=>{const image=document.querySelector('.clip-thumb img');return {width:image.naturalWidth,height:image.naturalHeight};})()`);
    assert(dimensions.width <= 320 && dimensions.height <= 180, `thumbnail remains oversized: ${JSON.stringify(dimensions)}`);
    assert(thumbnailFailures >= 3, 'two busy thumbnail responses should have been retried');
    thumbnailRecovery = {ms:Date.now()-began,...dimensions};
  }
  viewerOpened=true;
  const openedAt = Date.now();
  await cdp.evaluate(`document.querySelector('.clip-card .clip-thumb').click()`);
  if (!forceWebm && !autoPreview) {
    await until(() => cdp.evaluate(`Boolean(Array.from(document.querySelectorAll('.viewer-foot-btns button')).find(b=>b.textContent==='Smaller preview'))`), 'smaller preview control did not render');
    await cdp.evaluate(`window.__viceSmoke.bufferObserved=false;Array.from(document.querySelectorAll('.viewer-foot-btns button')).find(b=>b.textContent==='Smaller preview').click()`);
  }
  if (!original) await until(() => cdp.evaluate(`window.__viceSmoke.bufferObserved`), 'buffered timeline never became visible');
  assert.equal(await cdp.evaluate(`document.querySelectorAll('.video-buffering').length`), 0, 'buffering overlay must be absent');
  await until(() => cdp.evaluate(`(()=>{const v=document.querySelector('.viewer-video');return Boolean(v&&${original ? "!v.currentSrc.includes('proxy=')" : "v.currentSrc.includes('proxy=" + previewQuery + "')"}&&v.videoWidth>0&&v.currentTime>0&&!v.paused&&!v.error);})()`), 'WebM fallback never reached active video playback', 60000);
  const firstFrameMs = Date.now() - openedAt;
  const stage = await cdp.evaluate(`(()=>{const r=document.querySelector('.viewer-stage').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()`);
  await cdp.send('Input.dispatchMouseEvent', {type: 'mouseMoved', ...stage});
  await delay(400);
  assert.equal(await cdp.evaluate(`getComputedStyle(document.querySelector('.viewer-play')).opacity`), '0', 'central control must stay hidden during playback, even under the pointer');
  await cdp.evaluate(`document.querySelector('.viewer-video').pause()`);
  await until(() => cdp.evaluate(`Number(getComputedStyle(document.querySelector('.viewer-play')).opacity) > 0.99`), 'paused player did not show the central control');
  await cdp.evaluate(`document.querySelector('.viewer-video').play()`);

  if (original) {
    assert(firstFrameMs < 3000, `original playback took ${firstFrameMs} ms`);
    assert(!requests.some(url => /[?&]proxy=/.test(url)), 'original playback must not start a conversion');
  }
  if (original) {
    await until(() => cdp.evaluate(`document.querySelector('.viewer-video').currentTime > 6`), 'high-bitrate original did not sustain playback', 15000);
    assert.equal(await cdp.evaluate(`Array.from(document.querySelectorAll('video.clip-preview')).some(v => v.getAttribute('src'))`), false, 'hover players must release downloads when the viewer opens');
  }
  if (!original) {
    const initialCache = await fs.readdir(path.join(root, 'cache/proxies'));
    assert(initialCache.some(name => name.startsWith('.vice-stream-')), 'playback must begin before the encoder finishes');
    assert(!initialCache.some(name => !name.startsWith('.')), 'first playback must not wait for the completed preview');
    const fullDuration = await cdp.evaluate(`document.querySelector('.viewer-timebadge').textContent`);
    assert(fullDuration.includes('/ 8s'), `preview must show full clip duration: ${fullDuration}`);
    await until(async () => (await fs.readdir(path.join(root, 'cache/proxies'))).some(name => !name.startsWith('.')), 'preview cache never completed');
    await until(() => cdp.evaluate(`(()=>{const v=document.querySelector('.viewer-video');return v && Number.isFinite(v.duration) && v.duration > 7;})()`), 'completed preview did not expose its full seekable duration');
  }
  const playbackSource = await cdp.evaluate(`document.querySelector('.viewer-video').currentSrc`);
  const range = await fetch(playbackSource, {headers: {Range: 'bytes=0-1023'}});
  assert.equal(range.status, 206, 'completed previews must support range requests');
  const prefix = Buffer.from(await range.arrayBuffer());
  assert.equal(prefix.byteLength, 1024);
  if (forceWebm) assert.equal(prefix.subarray(0,4).toString('hex'), '1a45dfa3', 'a viewer session must not substitute original MP4 bytes for a completed WebM preview');
  await until(() => cdp.evaluate(`Boolean(document.querySelector('.viewer-buffered')?.getBoundingClientRect().width > 0)`), 'buffered timeline never rendered');
  const result = await cdp.evaluate(`(()=>{const v=document.querySelector('.viewer-video');return {source:v.currentSrc,width:v.videoWidth,height:v.videoHeight,duration:v.duration,currentTime:v.currentTime,error:v.error,bufferObserved:window.__viceSmoke.bufferObserved,bufferedColor:getComputedStyle(document.querySelector('.viewer-buffered')).backgroundColor,cardDuration:document.querySelector('.clip-card .clip-duration').textContent,banners:document.querySelector('.banners')?.innerText||''};})()`);
  assert(original ? !result.source.includes('proxy=') : result.source.includes(`proxy=${previewQuery}`));
  assert.notEqual(result.bufferedColor, 'rgba(0, 0, 0, 0)', 'buffered timeline is invisible');
  if (!original) assert(requests.some(url => url.includes(`proxy=${previewQuery}`)), 'browser never requested the selected preview');
  assert.equal(result.banners.trim(), '', `unexpected hard warning: ${result.banners}`);
  assert.equal(diagnostics.length, 0, JSON.stringify(diagnostics));
  let discordEncoderShown = null;
  if (process.env.VICE_SMOKE_DISCORD_PROGRESS === '1') {
    await cdp.evaluate(`
      window.__viceDiscordEncoder=null;
      new MutationObserver(()=>{
        const label=document.querySelector('.share-sheet')?.innerText.match(/h264_(?:nvenc|vaapi|qsv|vulkan)|libx264/);
        if(label)window.__viceDiscordEncoder=label[0];
      }).observe(document.body,{childList:true,subtree:true,characterData:true});
      Array.from(document.querySelectorAll('.viewer-foot-btns button')).find(b=>b.textContent==='Share').dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true}));
    `);
    await until(() => cdp.evaluate(`window.__viceDiscordEncoder`), 'Discord conversion never displayed its actual encoder');
    await until(() => cdp.evaluate(`Boolean(document.querySelector('.share-copy-file:not(:disabled)'))`), 'Discord conversion did not publish a completed copy', 60000);
    discordEncoderShown = await cdp.evaluate(`window.__viceDiscordEncoder`);
  }
  const viewerLayouts = [];
  if (original) {
    for (const [width, height] of [[1920, 1080], [1280, 820], [800, 600]]) {
      await cdp.send('Emulation.setDeviceMetricsOverride', {width, height, deviceScaleFactor: 1, mobile: false});
      await delay(400);
      const layout = await cdp.evaluate(`(()=>{
        const rect=selector=>{const r=document.querySelector(selector).getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height,bottom:r.bottom,right:r.right};};
        return {stack:rect('.viewer-stack'),video:rect('.viewer-stage'),controls:rect('.player-bar'),actions:rect('.viewer-foot-btns'),close:rect('.viewer-head .modal-close'),padding:parseFloat(getComputedStyle(document.querySelector('.video-viewer-scrim')).paddingLeft)};
      })()`);
      const margin = Math.max(layout.padding, (width - 880) / 4);
      assert(Math.abs(layout.stack.x - margin) < 1, `viewer should halve its old side margins at ${width}px: ${JSON.stringify(layout)}`);
      for (const key of ['stack','video','controls','actions','close']) {
        assert(layout[key].y >= layout.padding - 1 && layout[key].bottom <= height - layout.padding + 1, `${key} is clipped at ${width}x${height}: ${JSON.stringify(layout)}`);
      }
      assert(layout.video.height > 80, 'the enlarged viewer must retain visible video');
      viewerLayouts.push({width, height, margin: layout.stack.x, videoWidth: layout.video.width, videoHeight: layout.video.height});
      if (width === 1920) {
        const large = await cdp.send('Page.captureScreenshot', {format: 'png', captureBeyondViewport: false});
        await fs.writeFile('/tmp/vice-viewer-large.png', Buffer.from(large.data, 'base64'));
      }
    }
  }
  let lifecycle = null;
  if (process.env.VICE_SMOKE_LIFECYCLE === '1') {
    await cdp.evaluate(`document.querySelector('.viewer-head .modal-close').click()`);
    await until(() => cdp.evaluate(`!document.querySelector('.viewer-video')`), 'viewer did not close');
    await cdp.evaluate(`Array.from(document.querySelectorAll('.nav-item')).find(b=>b.querySelector('span')?.textContent==='All Clips').click()`);
    await cdp.evaluate(`Array.from(document.querySelectorAll('.nav-item')).find(b=>b.querySelector('span')?.textContent==='Home').click()`);
    await Promise.all(Array.from({length: 200}, (_, i) => fs.link(path.join(root, 'clips/smoke.mp4'), path.join(root, `clips/gallery-${i}.mp4`))));
    assert((await fetch(`${base}/api/library/rescan`, {method: 'POST'})).ok);
    await until(() => cdp.evaluate(`document.querySelector('.nav-count')?.textContent==='201'`), 'gallery did not refresh');
    await delay(800);
    const hiddenCards = await cdp.evaluate(`document.querySelector('.screen-keepalive[hidden]')?.querySelectorAll('.clip-card').length`);
    assert(hiddenCards <= 96, `hidden gallery expanded itself to ${hiddenCards} cards`);
    assert.equal(await cdp.evaluate(`document.querySelectorAll('.clip-preview').length`), 0, 'idle cards must not allocate video players');
    const navigationTimes = [];
    for (const label of ['Settings', 'All Clips', 'Home', 'All Clips', 'Settings', 'All Clips']) {
      const elapsed = await cdp.evaluate(`new Promise(resolve=>{const start=performance.now();Array.from(document.querySelectorAll('.nav-item')).find(b=>b.querySelector('span')?.textContent===${JSON.stringify(label)}).click();requestAnimationFrame(()=>requestAnimationFrame(()=>resolve(performance.now()-start)));})`);
      navigationTimes.push({label, ms: Math.round(elapsed)});
    }

    const galleryLayouts = [];
    for (const [width, expectedColumns] of [[1920,4],[960,2],[2560,4],[1280,2],[520,1]]) {
      await cdp.send('Emulation.setDeviceMetricsOverride', {width, height: 900, deviceScaleFactor: 1, mobile: false});
      await delay(100);
      const grid = await cdp.evaluate(`(()=>{const node=document.querySelector('.screen-keepalive .clip-grid');const card=node.querySelector('.clip-card');return {columns:getComputedStyle(node).gridTemplateColumns.split(' ').length,cardWidth:card.getBoundingClientRect().width,scrollWidth:document.documentElement.scrollWidth,viewport:innerWidth};})()`);
      assert.equal(grid.columns, expectedColumns, `gallery at ${width}px: ${JSON.stringify(grid)}`);
      assert(grid.scrollWidth <= grid.viewport, 'gallery must not introduce horizontal scrolling');
      galleryLayouts.push({width,...grid});
    }
    await cdp.send('Emulation.setDeviceMetricsOverride', {width: 1280, height: 820, deviceScaleFactor: 1, mobile: false});
    for (let i = 0; i < 5; i++) {
      await cdp.evaluate(`{const pane=document.querySelector('.frame-content');pane.scrollTop=pane.scrollHeight;pane.dispatchEvent(new Event('scroll'));}`);
      await delay(200);
    }
    await until(() => cdp.evaluate(`document.querySelectorAll('.screen-keepalive .clip-card').length === 201`), 'scrolling stopped at the first page');
    assert((await fetch(`${base}/api/trigger`, {method: 'POST'})).ok);
    await until(async () => {try {return (await fs.stat(path.join(root, 'clips/2026-replay.mp4'))).size === 7;} catch {return false;}}, 'fake replay never began writing');
    const statusStarted = Date.now();
    assert((await fetch(`${base}/api/status`)).ok);
    const statusMs = Date.now() - statusStarted;
    assert(statusMs < 500, `saving a replay blocked status for ${statusMs}ms`);
    assert((await fetch(`${base}/api/library/rescan`, {method: 'POST'})).ok);
    const during = await (await fetch(`${base}/api/clips`)).json();
    assert.equal(during.clips.length, 201, 'unfinished replay leaked into the library');
    await until(() => cdp.evaluate(`Array.from(document.querySelectorAll('.clip-name')).some(n=>n.textContent==='Vice_Clip_1')`), 'saved replay did not reach the gallery');
    const clips = (await (await fetch(`${base}/api/clips`)).json()).clips;
    const saved = clips.find(c => c.name === 'Vice_Clip_1.mp4');
    assert(saved && saved.duration > 7, 'published replay must have its finalized duration');
    await cdp.evaluate(`{const card=Array.from(document.querySelectorAll('.clip-card')).find(c=>c.querySelector('.clip-name')?.textContent==='Vice_Clip_1');card.querySelector('.clip-thumb').click();}`);
    await until(() => cdp.evaluate(`{const v=document.querySelector('.viewer-video');v&&v.currentTime>0.2&&v.videoWidth>0&&!v.error}`), 'new replay stuck buffering', 30000);
    await cdp.evaluate(`document.querySelector('.viewer-video').pause()`);
    const after = saved.video_url;
    const renamed = await (await fetch(`${base}/api/clips/${saved.slug}/rename`, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({name:' .Exact name! #1?  '})})).json();
    assert.equal(renamed.name, ' .Exact name! #1?  .mp4');
    assert.equal(renamed.slug, saved.slug);
    await delay(500);
    assert(await cdp.evaluate(`document.querySelector('.viewer-video').paused`), 'metadata/rename update restarted paused playback');
    const trimSocket = new WebSocket(`${base.replace('http:', 'ws:')}/ws`);
    await once(trimSocket, 'open');
    const trimEvents = [];
    trimSocket.addEventListener('message', event=>{const msg=JSON.parse(String(event.data));if(msg.type==='trim_progress'&&msg.slug===saved.slug)trimEvents.push(msg);});
    const trimStarted = Date.now();
    const replaced = await fetch(`${base}/api/clips/${saved.slug}/trim`, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({start:0.25,end:1.75,copy:false})});
    assert(replaced.ok, await replaced.text());
    const trimmed = await (await fetch(`${base}/api/clips/${saved.slug}`)).json();
    assert(Math.abs(trimmed.duration - 1.5) < 0.15, JSON.stringify(trimmed));
    const trimMs = Date.now() - trimStarted;
    await until(() => trimEvents.some(event=>event.phase==='uploading'&&event.progress===1), 'trim upload progress never completed');
    trimSocket.close();
    assert(trimEvents.some(event=>event.phase==='encoding'&&event.encoder), 'trim must report its actual encoder');
    const noOpStarted = Date.now();
    assert((await fetch(`${base}/api/clips/${saved.slug}/trim`, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({start:0,end:trimmed.duration,copy:false})})).ok);
    const noOpMs = Date.now() - noOpStarted;
    assert(noOpMs < 500, `unchanged trim took ${noOpMs}ms`);
    assert.notEqual(trimmed.video_url, after, 'replace must invalidate the old playback revision');
    lifecycle = {statusMs, hiddenCards, navigationTimes, galleryLayouts, galleryCards: 202, trimMs, trimEncoder:trimEvents.find(event=>event.phase==='encoding')?.encoder, noOpMs, literalName: renamed.name};
    await cdp.evaluate(`document.querySelector('.viewer-head .modal-close').click()`);
    await until(() => cdp.evaluate(`!document.querySelector('.viewer-video')`), 'viewer did not close after trim');
    await cdp.evaluate(`Array.from(document.querySelectorAll('.nav-item')).find(b=>b.querySelector('span')?.textContent==='Settings').click()`);
    await until(() => cdp.evaluate(`Boolean(document.querySelector('select[aria-label="Theme"]'))`), 'appearance selector missing');
    assert.equal(await cdp.evaluate(`document.documentElement.dataset.appearance`), 'light');
    await cdp.evaluate(`{const select=document.querySelector('select[aria-label="Theme"]');select.value='dark';select.dispatchEvent(new Event('change',{bubbles:true}));}`);
    await until(() => cdp.evaluate(`document.documentElement.dataset.appearance==='dark'&&getComputedStyle(document.documentElement).getPropertyValue('--vice-bg').trim()==='#040506'`), 'dark theme did not apply');
    await until(async () => (await (await fetch(`${base}/api/config`)).json()).ui?.theme === 'dark', 'dark theme was not persisted');
    const icon = await cdp.evaluate(`(()=>{const canvas=document.createElement('canvas');canvas.width=canvas.height=96;const ctx=canvas.getContext('2d');ctx.fillStyle='#ff5722';ctx.fillRect(0,0,96,96);return canvas.toDataURL('image/png');})()`);
    const playlistResponse = await fetch(`${base}/api/playlists`, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({name:'Artwork test',emoji:'',icon})});
    assert(playlistResponse.ok, await playlistResponse.text());
    await until(() => cdp.evaluate(`Array.from(document.querySelectorAll('.playlist-icon-image')).some(img=>img.src.startsWith('data:image/png')&&img.complete&&img.naturalWidth===96)`), 'uploaded playlist image did not render');
    await cdp.evaluate(`Array.from(document.querySelectorAll('.sidenav-playlists .nav-item')).find(b=>b.querySelector('.nav-label')?.textContent==='Artwork test').dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true,clientX:80,clientY:160}))`);
    await until(() => cdp.evaluate(`Boolean(Array.from(document.querySelectorAll('.ctx-item')).find(b=>b.textContent.includes('Edit playlist')))`), 'playlist editor action missing');
    await cdp.evaluate(`Array.from(document.querySelectorAll('.ctx-item')).find(b=>b.textContent.includes('Edit playlist')).click()`);
    await until(() => cdp.evaluate(`Boolean(document.querySelector('.npl-icon-options input[type="file"]'))`), 'playlist icon upload option missing');
    await cdp.evaluate(`(()=>{const canvas=document.createElement('canvas');canvas.width=canvas.height=128;const ctx=canvas.getContext('2d');ctx.fillStyle='#02ff40';ctx.fillRect(0,0,128,128);const bytes=Uint8Array.from(atob(canvas.toDataURL('image/png').split(',')[1]),c=>c.charCodeAt(0));const transfer=new DataTransfer();transfer.items.add(new File([bytes],'custom-icon.png',{type:'image/png'}));const input=document.querySelector('.npl-icon-options input[type="file"]');input.files=transfer.files;input.dispatchEvent(new Event('change',{bubbles:true}));})()`);
    await until(() => cdp.evaluate(`(()=>{const img=document.querySelector('.npl-preview .playlist-icon-image');return img?.complete&&img.naturalWidth===96&&document.querySelector('.modal-foot button:last-child')?.disabled===false;})()`), 'uploaded icon did not finish resizing');
    await cdp.evaluate(`document.querySelector('.modal-foot button:last-child').click()`);
    await until(() => cdp.evaluate(`!document.querySelector('.npl-icon-options')`), 'playlist icon edit did not save');
    const persistedPlaylists = (await (await fetch(`${base}/api/playlists`)).json()).playlists;
    assert(persistedPlaylists.find(p=>p.name==='Artwork test')?.icon.startsWith('data:image/png;base64,'));
    await cdp.send('Emulation.setDeviceMetricsOverride', {width: 1280, height: 820, deviceScaleFactor: 1, mobile: false});
    const darkPng = await cdp.send('Page.captureScreenshot', {format: 'png', captureBeyondViewport: false});
    await fs.writeFile('/tmp/vice-dark-theme.png', Buffer.from(darkPng.data, 'base64'));
    lifecycle.darkTheme = true; lifecycle.playlistIcon = true;
  }
  let liveTags = false;
  if (process.env.VICE_SMOKE_LIVE_TAGS === '1') {
    const sourceBeforeTags = await cdp.evaluate(`document.querySelector('.viewer-video').getAttribute('src')`);
    assert((await fetch(`${base}/api/playlists/auto%3Alive-tag/clips`, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({slug:'smoke'})})).ok);
    await until(() => cdp.evaluate(`document.querySelector('.clip-game')?.textContent==='Live tag game'&&document.querySelector('.player-titles .mono')?.textContent==='Live tag game'`), 'adding a game tag did not update the card and open viewer immediately', 2000);
    assert.equal(await cdp.evaluate(`document.querySelector('.viewer-video').getAttribute('src')`), sourceBeforeTags, 'a tag update must not reload the video');
    assert((await fetch(`${base}/api/playlists/auto%3Alive-tag/clips/smoke`, {method:'DELETE'})).ok);
    await until(() => cdp.evaluate(`Boolean(document.querySelector('.clip-game[data-untagged]'))&&document.querySelector('.player-titles .mono')?.textContent===''`), 'removing a tag left stale labels', 2000);
    assert.equal(await cdp.evaluate(`document.querySelector('.viewer-video').getAttribute('src')`), sourceBeforeTags, 'removing a tag must not reload the video');
    liveTags = true;
  }
  let keyboardSeek = false;
  if (process.env.VICE_SMOKE_KEYS === '1') {
    assert(original, 'precise arrow-key assertions use an original complete recording');
    const initial = await cdp.evaluate(`document.querySelector('.viewer-video').getAttribute('src')`);
    await cdp.evaluate(`{const video=document.querySelector('.viewer-video');video.pause();video.currentTime=1;}`);
    await until(() => cdp.evaluate(`!document.querySelector('.viewer-video').seeking`), 'setup seek did not complete');
    const press = async key => {await cdp.send('Input.dispatchKeyEvent',{type:'keyDown',key,code:key});await cdp.send('Input.dispatchKeyEvent',{type:'keyUp',key,code:key});};
    await press('ArrowRight');
    await until(() => cdp.evaluate(`Math.abs(document.querySelector('.viewer-video').currentTime-6)<0.15`), 'Right arrow did not seek five seconds');
    assert.equal(await cdp.evaluate(`document.querySelector('.viewer-video').getAttribute('src')`), initial, 'arrow key switched clips');
    assert(await cdp.evaluate(`document.querySelector('.viewer-video').paused`), 'seeking should preserve pause');
    await press('ArrowLeft');
    await until(() => cdp.evaluate(`Math.abs(document.querySelector('.viewer-video').currentTime-1)<0.15`), 'Left arrow did not seek back five seconds');
    await press('ArrowLeft');
    await until(() => cdp.evaluate(`document.querySelector('.viewer-video').currentTime<0.05`), 'Left arrow did not clamp to zero');
    await cdp.evaluate(`document.querySelector('.viewer-video').currentTime=7`);
    await until(() => cdp.evaluate(`!document.querySelector('.viewer-video').seeking`), 'end setup seek did not complete');
    await press('ArrowRight');
    await until(() => cdp.evaluate(`{const v=document.querySelector('.viewer-video');v.currentTime<=v.duration&&v.currentTime>v.duration-0.15}`), 'Right arrow did not clamp to end');
    await cdp.evaluate(`document.querySelector('.viewer-heading h2').dispatchEvent(new MouseEvent('dblclick',{bubbles:true}))`);
    await until(() => cdp.evaluate(`Boolean(document.querySelector('.viewer-rename'))`), 'rename input did not open');
    const before = await cdp.evaluate(`document.querySelector('.viewer-video').currentTime`);
    await press('ArrowLeft');
    assert.equal(await cdp.evaluate(`document.querySelector('.viewer-video').currentTime`), before, 'editing a name must not seek the video');
    await press('Escape');
    keyboardSeek = true;
  }
  let replayCache = false;
  if (process.env.VICE_SMOKE_REPLAY_CACHE === '1') {
    assert(original, 'replay cache regression uses original media');
    const source = await cdp.evaluate(`document.querySelector('.viewer-video').currentSrc`);
    const session = new URL(source).searchParams.get('playback_session');
    assert(session, 'viewer did not acquire a playback session');
    const cacheRoot = path.join(root, 'cache/playback');
    const entry = (await fs.readdir(cacheRoot, {recursive:true})).find(name => path.basename(name) === session + '.cache');
    assert(entry, 'playback cache file is missing');
    const spool = path.join(cacheRoot, entry);
    assert((await fs.stat(spool)).size > 0, 'downloaded video has no session spool');
    await cdp.evaluate(`{const v=document.querySelector('.viewer-video');v.currentTime=v.duration-0.25;v.play();}`);
    await until(() => cdp.evaluate(`document.querySelector('.viewer-video').ended`), 'clip never reached EOS');
    assert((await fs.stat(spool)).size > 0, 'EOS released the cached recording');
    assert((await fetch(`${base}/api/playback/${session}/touch`, {method:'POST'})).ok, 'EOS released the playback session');
    await cdp.evaluate(`{const v=document.querySelector('.viewer-video');v.currentTime=0;v.play();}`);
    await until(() => cdp.evaluate(`{const v=document.querySelector('.viewer-video');v.currentTime>0.1&&!v.paused&&!v.error}`), 'cached clip did not replay', 3000);
    assert.equal(await cdp.evaluate(`document.querySelector('.viewer-video').currentSrc`), source, 'replay changed the video source');
    await cdp.evaluate(`document.querySelector('.viewer-head .modal-close').click()`);
    await until(async () => {
      try {await fs.stat(spool);return false;} catch(error) {if(error.code==='ENOENT')return true;throw error;}
    }, 'closing the player did not release its spool');
    assert.equal((await fetch(`${base}/api/playback/${session}/touch`, {method:'POST'})).status, 404, 'closed session is still live');
    replayCache = true;
  }
  const png = await cdp.send('Page.captureScreenshot', {format: 'png', captureBeyondViewport: false});
  await fs.writeFile(screenshot, Buffer.from(png.data, 'base64'));
  console.log(JSON.stringify({ok: true, originalQuality: original, firstFrameMs, discordEncoderShown, viewerLayouts, lifecycle, liveTags, keyboardSeek, thumbnailRecovery, replayCache, playedBeforeConversionFinished: !original, rangeRequests: true, ...result, screenshot}, null, 2));
} catch (error) {
  console.error(error);
  if (cdp) { try { console.error('Page state:', await cdp.evaluate(`({url:location.href,visibility:document.visibilityState,thumbnail:document.querySelector('.clip-card')?.getBoundingClientRect().toJSON(),body:document.body.innerText.slice(0,3000),videos:Array.from(document.querySelectorAll('video')).map(v=>({src:v.currentSrc,width:v.videoWidth,duration:v.duration,error:v.error&&{code:v.error.code,message:v.error.message}}))})`)); } catch {} }
  console.error('Daemon log:', daemonLog.slice(-5000));
  console.error('Browser log:', browserLog.slice(-3000));
  console.error('Browser activity:', JSON.stringify(activity));
  console.error('Browser exceptions:', JSON.stringify(diagnostics));
  process.exitCode = 1;
} finally {
  cdp?.close();
  await stop(browser);
  await stop(daemon);
  await fs.rm(root, {recursive: true, force: true});
}
