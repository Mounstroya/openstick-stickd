// Takes the README screenshots from the simulated-data server in src/capturas.rs.
// Usually run through ./capture.sh. Needs puppeteer-core and Chromium:
//   CHROMIUM=/usr/bin/chromium node capture.js <out-dir> [base-port]
const puppeteer = require('puppeteer-core');
const fs = require('fs');
const os = require('os');
const path = require('path');

const OUT = process.argv[2] || __dirname;
const P = +(process.argv[3] || 8740);
const url = (off, p = '/') => `http://127.0.0.1:${P + off}${p}`;
const SCHEME = process.env.SCHEME || 'dark';

const UA_ANDROID = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Mobile Safari/537.36';
// iOS Captive Network Assistant: WebKit without "Safari" in the User-Agent
const UA_IOS_CNA = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148';
const MOVIL = { width: 390, height: 844, deviceScaleFactor: 2, isMobile: true, hasTouch: true };
const ESCRITORIO = { width: 1100, height: 760, deviceScaleFactor: 2 };

const lanzar = () => puppeteer.launch({
  executablePath: process.env.CHROMIUM || '/usr/bin/chromium',
  headless: true,
  userDataDir: fs.mkdtempSync(path.join(os.tmpdir(), 'stickd-capturas-')),
  args: ['--no-sandbox', '--no-first-run', '--hide-scrollbars', '--lang=es-MX'],
  env: { ...process.env, LANG: 'es_MX.UTF-8', LANGUAGE: 'es_MX:es' },
});

async function pagina(browser, { vp = MOVIL, ua = UA_ANDROID, scheme = SCHEME } = {}) {
  const pg = await browser.newPage();
  await pg.setViewport(vp);
  await pg.setUserAgent(ua);
  await pg.emulateMediaFeatures([{ name: 'prefers-color-scheme', value: scheme }]);
  return pg;
}

async function foto(pg, nombre, opts = {}) {
  await new Promise((r) => setTimeout(r, 400));
  await pg.screenshot({ path: path.join(OUT, nombre), ...opts });
  console.log('ok', nombre);
}

async function simple(browser, nombre, u, o = {}) {
  const pg = await pagina(browser, o);
  if (o.cookie) await pg.setCookie({ url: u, ...o.cookie });
  await pg.goto(u, { waitUntil: 'networkidle0' });
  if (o.prep) await o.prep(pg);
  await foto(pg, nombre, o.shot || {});
  await pg.close();
}

// "Compartir archivos" between two real browsers (WebRTC), captured mid-transfer.
// The sender reads the files a bit slower than the disk would, so the moment is easy to catch.
async function compartir(nombre, nombreEnvio) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stickd-archivos-'));
  const archivos = [['presentacion-feria.pdf', 12.4e6], ['video-demo-stand.mp4', 820e6], ['fotos-del-evento.zip', 356e6]]
    .map(([n, s]) => { const p = path.join(dir, n); fs.closeSync(fs.openSync(p, 'w')); fs.truncateSync(p, Math.round(s)); return p; });
  const [bE, bR] = await Promise.all([lanzar(), lanzar()]);
  try {
    const e = await pagina(bE);
    await e.evaluateOnNewDocument(() => {
      const leer = FileReader.prototype.readAsArrayBuffer;
      FileReader.prototype.readAsArrayBuffer = function (b) { setTimeout(() => leer.call(this, b), 30); };
    });
    await e.goto(url(6), { waitUntil: 'networkidle0' });
    const input = await e.$('#archivos');
    await input.uploadFile(...archivos);
    await e.waitForFunction(() => document.getElementById('e-codigo').textContent.length > 3);
    await e.waitForFunction(() => document.getElementById('e-qr').complete);
    if (nombreEnvio) await foto(e, nombreEnvio);
    const codigo = await e.$eval('#e-codigo', (x) => x.textContent);
    const r = await pagina(bR);
    await r.goto(url(6, '/r/' + codigo), { waitUntil: 'domcontentloaded' });
    await r.waitForSelector('#r-aceptar', { visible: true, timeout: 30000 });
    await r.click('#r-aceptar');
    await r.waitForFunction(() => parseFloat(document.getElementById('r-barra').style.width) >= 42 && document.getElementById('r-p2').textContent, { timeout: 120000, polling: 50 });
    await r.screenshot({ path: path.join(OUT, nombre) });
    console.log('ok', nombre);
  } finally {
    await bE.close(); await bR.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

(async () => {
  const b = await lanzar();
  try {
    // guest pages (phone)
    await simple(b, 'portfolio.png', url(1));
    await simple(b, 'portfolio-link.png', url(2));
    await simple(b, 'card.png', url(3));
    await simple(b, 'card-light.png', url(3), { scheme: 'light' });
    await simple(b, 'mailbox.png', url(4), {
      prep: async (pg) => {
        await pg.type('textarea', 'Hola Yael, te dejo las fotos del stand para el resumen del evento.');
        await pg.type('input[name=apodo]', 'Sofía');
        const f = path.join(os.tmpdir(), 'fotos-stand.jpg');
        fs.writeFileSync(f, Buffer.alloc(2_400_000));
        await (await pg.$('input[type=file]')).uploadFile(f);
        await pg.evaluate(() => { document.activeElement.blur(); window.scrollTo(0, 150); });
      },
    });
    await simple(b, 'chat.png', url(5), { cookie: { name: 'apodo', value: 'Sof%C3%ADa' }, prep: (pg) => pg.evaluate(() => window.scrollTo(0, document.body.scrollHeight)) });
    await simple(b, 'share.png', url(6));
    await simple(b, 'share-iphone.png', url(6), { ua: UA_IOS_CNA });
    await simple(b, 'guest-internet.png', url(1, '/_stick/internet'));
    // admin panel
    await simple(b, 'admin-phone.png', url(0));
    await simple(b, 'admin-desktop.png', url(0), { vp: ESCRITORIO });
    for (const [n, id] of [['admin-guests.png', 'invitados'], ['admin-traffic.png', 'afluencia']]) {
      const pg = await pagina(b, { vp: ESCRITORIO });
      await pg.goto(url(0, '/?abrir=' + id), { waitUntil: 'networkidle0' });
      await foto(await pg.$('#' + (id === 'invitados' ? id : 'v-' + id)), n);
      await pg.close();
    }
  } finally {
    await b.close();
  }
  await compartir('share-transfer.png', 'share-send.png');
})().catch((e) => { console.error(e); process.exit(1); });
