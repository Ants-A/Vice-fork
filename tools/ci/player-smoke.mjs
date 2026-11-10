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
  await fs.writeFile(path.join(root, 'bin/gpu-screen-recorder'), '#!/bin/sh\ncase "$1" in\n--info) printf "section=video_codecs\\nh264\\n"; exit 0;;\n--help) printf "-w -f -r -c -a -o\\n"; exit 0;;\n--list-*) exit 0;;\nesac\ntrap "exit 0" INT TERM\nwhile :; do sleep 1 & wait $!; done\n', {mode: 0o755});
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
  const env = {...process.env, VICE_CONFIG: path.join(root, 'config.toml'), VICE_DATA_DIR: path.join(root, 'data'), VICE_CACHE_DIR: path.join(root, 'cache'), VICE_RESOURCE_DIR: path.join(repo, 'vice'), XDG_RUNTIME_DIR: path.join(root, 'run'), TMPDIR: path.join(root, 'tmp'), PATH: `${path.join(root, 'bin')}:${process.env.PATH}`};
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
    new MutationObserver(()=>{const bar=document.querySelector('.video-buffering progress');if(bar&&bar.getBoundingClientRect().width>0)window.__viceSmoke.bufferObserved=true;}).observe(document,{childList:true,subtree:true,attributes:true});
  `});
  // Make the waiting state reproducible even on a fast CPU and local disk.
  await cdp.send('Fetch.enable', {patterns: [{urlPattern: `*proxy=${previewQuery}*`, requestStage: 'Request'}]});
  cdp.on('Fetch.requestPaused', ({requestId}) => { void delay(800).then(() => cdp.send('Fetch.continueRequest', {requestId})).catch(() => {}); });
  await cdp.send('Page.navigate', {url: base});
  await until(() => cdp.evaluate(`Boolean(document.querySelector('.clip-card .clip-duration')?.textContent && !document.querySelector('.tutorial'))`), 'clip card and duration did not render');
  if (forceWebm) assert.equal(await cdp.evaluate(`document.createElement('video').canPlayType('video/mp4; codecs="avc1.640028"')`), '');
  assert.notEqual(await cdp.evaluate(`document.createElement('video').canPlayType('video/webm; codecs="vp8, opus"')`), '');
  if (original) {
    // A large file over a link with latency must stay original and avoid any
    // conversion work. This catches the previous automatic 720p regression.
    await cdp.send('Network.emulateNetworkConditions', {offline: false, latency: 60, downloadThroughput: 12_500_000, uploadThroughput: 12_500_000});
  }
  viewerOpened=true;
  const openedAt = Date.now();
  await cdp.evaluate(`document.querySelector('.clip-card .clip-thumb').click()`);
  if (!forceWebm && !autoPreview) {
    await until(() => cdp.evaluate(`Boolean(Array.from(document.querySelectorAll('.viewer-foot-btns button')).find(b=>b.textContent==='Smaller preview'))`), 'smaller preview control did not render');
    await cdp.evaluate(`window.__viceSmoke.bufferObserved=false;Array.from(document.querySelectorAll('.viewer-foot-btns button')).find(b=>b.textContent==='Smaller preview').click()`);
  }
  if (!original) await until(() => cdp.evaluate(`window.__viceSmoke.bufferObserved`), 'waiting state never displayed its buffering progress');
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
  const range = await fetch(`${base}/v/smoke${original ? '' : '?proxy=' + previewQuery}`, {headers: {Range: 'bytes=0-1023'}});
  assert.equal(range.status, 206, 'completed previews must support range requests');
  assert.equal((await range.arrayBuffer()).byteLength, 1024);
  await until(() => cdp.evaluate(`Boolean(document.querySelector('.viewer-buffered')?.getBoundingClientRect().width > 0)`), 'buffered timeline never rendered');
  const result = await cdp.evaluate(`(()=>{const v=document.querySelector('.viewer-video');return {source:v.currentSrc,width:v.videoWidth,height:v.videoHeight,duration:v.duration,currentTime:v.currentTime,error:v.error,bufferObserved:window.__viceSmoke.bufferObserved,bufferedColor:getComputedStyle(document.querySelector('.viewer-buffered')).backgroundColor,cardDuration:document.querySelector('.clip-card .clip-duration').textContent,banners:document.querySelector('.banners')?.innerText||''};})()`);
  assert(original ? !result.source.includes('proxy=') : result.source.includes(`proxy=${previewQuery}`));
  assert.notEqual(result.bufferedColor, 'rgba(0, 0, 0, 0)', 'buffered timeline is invisible');
  if (!original) assert(requests.some(url => url.includes(`proxy=${previewQuery}`)), 'browser never requested the selected preview');
  assert.equal(result.banners.trim(), '', `unexpected hard warning: ${result.banners}`);
  assert.equal(diagnostics.length, 0, JSON.stringify(diagnostics));
  const png = await cdp.send('Page.captureScreenshot', {format: 'png', captureBeyondViewport: false});
  await fs.writeFile(screenshot, Buffer.from(png.data, 'base64'));
  console.log(JSON.stringify({ok: true, originalQuality: original, firstFrameMs, playedBeforeConversionFinished: !original, rangeRequests: true, ...result, screenshot}, null, 2));
} catch (error) {
  console.error(error);
  if (cdp) { try { console.error('Page state:', await cdp.evaluate(`({url:location.href,body:document.body.innerText.slice(0,3000),videos:Array.from(document.querySelectorAll('video')).map(v=>({src:v.currentSrc,width:v.videoWidth,duration:v.duration,error:v.error&&{code:v.error.code,message:v.error.message}}))})`)); } catch {} }
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
