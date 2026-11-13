// Renders one clip frame by frame through headless Chromium into an H.264 MP4, or a few stills.
// usage: node render.mjs huashu|term SPEC.json OUT.mp4 [--stills 1.0,4.5]
// `huashu` pages come from the huashu-art-motion engine (HUASHU_ENGINE), `term` replays a cast that
// record.py made. CHROMIUM names the browser; Playwright's own download is not needed.
import { chromium } from 'playwright-core';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';

const here = path.dirname(new URL(import.meta.url).pathname);
const [kind, specPath, out, flag, stillList] = process.argv.slice(2);
const sites = {
  huashu: [process.env.HUASHU_ENGINE || path.join(here, '../.claude/skills/huashu-art-motion/scripts/engine'), 'clip.html'],
  term: [here, 'term.html'],
};
if (!sites[kind] || !specPath || !out) {
  console.error('usage: node render.mjs huashu|term SPEC.json OUT.mp4 [--stills 1.0,4.5]');
  process.exit(2);
}
const [root, pageName] = sites[kind];
const spec = JSON.parse(fs.readFileSync(specPath, 'utf8'));
spec.width ??= 1920;
spec.height ??= 1080;
spec.fps ??= 30;
if (kind === 'term') {
  spec.cast = fs.readFileSync(path.resolve(path.dirname(specPath), spec.cast), 'utf8');
  const last = JSON.parse(spec.cast.trim().split('\n').at(-1));
  spec.duration ??= last[0];
}

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.json': 'application/json' };
const server = http.createServer((req, res) => {
  if (req.url === '/favicon.ico') return res.writeHead(204).end();
  const file = path.join(root, decodeURIComponent(new URL(req.url, 'http://local').pathname));
  if (!file.startsWith(root + path.sep)) return res.writeHead(403).end();
  fs.readFile(file, (err, data) => {
    if (err) return res.writeHead(404).end();
    res.writeHead(200, { 'content-type': types[path.extname(file)] || 'application/octet-stream' }).end(data);
  });
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));

const executablePath = process.env.CHROMIUM
  || ['/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome-stable'].find(p => fs.existsSync(p));
const browser = await chromium.launch({ executablePath, args: ['--disable-web-security', '--font-render-hinting=none'] });
const page = await browser.newPage({ viewport: { width: spec.width, height: spec.height }, deviceScaleFactor: 1 });
const errors = [];
page.on('console', m => { if (m.type() === 'error') { errors.push(m.text()); console.error('[page]', m.text()); } });
page.on('pageerror', e => { errors.push(String(e)); console.error('[pageerror]', String(e)); });
await page.addInitScript(`window.CLIP_SPEC = ${JSON.stringify(spec)};`);
await page.goto(`http://127.0.0.1:${server.address().port}/${pageName}?render=1`);
await page.waitForFunction('window.__ready === true || !!window.__bootFailed', null, { timeout: 120000 });
const failed = await page.evaluate('window.__bootFailed || null');
if (failed) throw new Error(`the page failed to load: ${failed}`);

const grab = async t => Buffer.from(await page.evaluate(async t => {
  await window.prepare(t);
  window.renderFrame(t);
  return window.__canvas.toDataURL('image/png').split(',')[1];
}, t), 'base64');

if (flag === '--stills') {
  fs.mkdirSync(out, { recursive: true });
  for (const s of stillList.split(',')) fs.writeFileSync(path.join(out, `t${Number(s).toFixed(2)}.png`), await grab(Number(s)));
} else {
  fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });
  const ff = spawn('ffmpeg', ['-y', '-v', 'error', '-f', 'image2pipe', '-framerate', String(spec.fps), '-c:v', 'png', '-i', '-',
    '-c:v', 'libx264', '-preset', 'slow', '-crf', '18', '-pix_fmt', 'yuv420p', '-r', String(spec.fps), '-movflags', '+faststart', out],
  { stdio: ['pipe', 'inherit', 'inherit'] });
  const done = new Promise((resolve, reject) => ff.on('close', code => (code ? reject(new Error(`ffmpeg exited ${code}`)) : resolve())));
  const frames = Math.round(spec.duration * spec.fps);
  for (let i = 0; i < frames; i++) {
    if (!ff.stdin.write(await grab(i / spec.fps))) await new Promise(resolve => ff.stdin.once('drain', resolve));
  }
  ff.stdin.end();
  await done;
}
await browser.close();
server.close();
if (errors.length) {
  console.error(`${errors.length} page errors`);
  process.exit(1);
}
