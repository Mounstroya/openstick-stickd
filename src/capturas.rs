//! Servidor de capturas de pantalla para el README (solo existe en `cargo test`).
//!
//! Llena directorios temporales con datos SIMULADOS (todo ficticio) y sirve con `tiny_http`:
//!   :P      el panel de administración (estado del daemon simulado, sin nmcli/ip/wg)
//!   :P+1    vitrina Portafolio (sitio subido)       :P+2  Portafolio sin sitio (landing con QR)
//!   :P+3    Tarjeta                                 :P+4  Buzón
//!   :P+5    Chat local                              :P+6  Compartir archivos
//! Las páginas de invitado pasan por la misma barra de «Internet para invitados» que en el stick
//! (`/_stick/internet` incluido).
//!
//!   CAPTURAS_PUERTO=8740 cargo test capturas_servidor -- --ignored --nocapture
//!
//! `docs/screenshots/capture.sh` lo arranca, toma las capturas con puppeteer y lo detiene.

use crate::vitrina::afluencia::{Afluencia, Probe};
use crate::vitrina::buzon::Buzon;
use crate::vitrina::chat::Chat;
use crate::vitrina::compartir::Compartir;
use crate::vitrina::portafolio::Portafolio;
use crate::vitrina::tarjeta::Tarjeta;
use crate::vitrina::{GuestReq, GuestResp, Vitrina};
use crate::{captive, invitados, portal, Mode, State, Vitrinas};
use std::fs;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const DUENO: &str = "Yael Montoya";
/// Desfase de la zona horaria del proceso en minutos (`date +%z`), el mismo que usan los módulos.
/// capture.sh elige un TZ en el que ahora sea media tarde, para que las capturas se parezcan.
fn utc_min() -> i64 {
    let o = std::process::Command::new("date").arg("+%z").output().unwrap();
    let z = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let n: i64 = z[1..3].parse::<i64>().unwrap() * 60 + z[3..5].parse::<i64>().unwrap();
    if z.starts_with('-') { -n } else { n }
}

fn ahora() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn ip(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 4, n))
}

fn kv(p: &[(&str, &str)]) -> Vec<(String, String)> {
    p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

/// "AAAA-MM-DD" de un día desde 1970 (algoritmo civil de H. Hinnant).
fn fecha(dia: i64) -> String {
    let z = dia + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn hoy_sistema() -> String {
    let o = std::process::Command::new("date").arg("+%F").output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Generador pseudoaleatorio fijo (las capturas salen iguales cada vez).
struct Lcg(u64);
impl Lcg {
    fn sig(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn hasta(&mut self, n: u64) -> u64 {
        self.sig() % n.max(1)
    }
}

// ───────────────────────────── PNG mínimo (foto de la tarjeta)

fn crc32(d: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in d {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn png_rgb(w: u32, h: u32, px: &[[u8; 3]]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for y in 0..h {
        raw.push(0);
        for x in 0..w {
            raw.extend_from_slice(&px[(y * w + x) as usize]);
        }
    }
    // zlib con bloques "stored" (sin comprimir)
    let mut z = vec![0x78, 0x01];
    for (i, c) in raw.chunks(65_535).enumerate() {
        let ultimo = (i + 1) * 65_535 >= raw.len();
        z.push(ultimo as u8);
        z.extend_from_slice(&(c.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(c.len() as u16)).to_le_bytes());
        z.extend_from_slice(c);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut o = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |t: &[u8], d: &[u8]| {
        o.extend_from_slice(&(d.len() as u32).to_be_bytes());
        let mut td = t.to_vec();
        td.extend_from_slice(d);
        o.extend_from_slice(&td);
        o.extend_from_slice(&crc32(&td).to_be_bytes());
    };
    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    o
}

/// Avatar generado: silueta clara sobre degradado (no es una foto de nadie).
fn avatar_png() -> Vec<u8> {
    let n = 320u32;
    let mut px = vec![[0u8; 3]; (n * n) as usize];
    let mezcla = |a: [f64; 3], b: [f64; 3], t: f64| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f64 / n as f64, y as f64 / n as f64);
            let mut c = mezcla([20.0, 150.0, 140.0], [47.0, 95.0, 225.0], (fx * 0.6 + fy * 0.4).clamp(0.0, 1.0));
            // cobertura suavizada de cabeza y hombros (4×4 submuestras)
            let mut cub = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px_ = (x as f64 + (sx as f64 + 0.5) / 4.0) / n as f64;
                    let py_ = (y as f64 + (sy as f64 + 0.5) / 4.0) / n as f64;
                    let cabeza = (px_ - 0.5).powi(2) + (py_ - 0.40).powi(2) < 0.165f64.powi(2);
                    let hombros = ((px_ - 0.5) / 0.32).powi(2) + ((py_ - 0.98) / 0.30).powi(2) < 1.0;
                    if cabeza || hombros {
                        cub += 1.0 / 16.0;
                    }
                }
            }
            c = mezcla(c, [236.0, 242.0, 250.0], cub * 0.92);
            px[(y * n + x) as usize] = [c[0] as u8, c[1] as u8, c[2] as u8];
        }
    }
    png_rgb(n, n, &px)
}

// ───────────────────────────── datos simulados por vitrina

const SITIO_INDEX: &str = r##"<!doctype html><html lang=es><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>Yael Montoya · Portafolio</title>
<link rel=stylesheet href="estilo.css"></head><body>
<header class=hero><div class=marca>YM</div><h1>Yael Montoya</h1>
<p class=rol>Sistemas embebidos · Rust · Redes</p>
<p class=intro>Hago que hardware barato haga cosas útiles: daemons pequeños, portales WiFi sin internet y herramientas que caben en 1 MB.</p>
<nav><a href="#proyectos">Proyectos</a><a href="#contacto">Contacto</a></nav></header>
<main><h2 id=proyectos>Proyectos recientes</h2>
<article><img src="img/stick.svg" alt=""><div><h3>openstick-stickd</h3><p>Un stick 4G de 10 dólares convertido en punto WiFi con portal propio, chat local y transferencia de archivos P2P.</p><p class=tags><span>Rust</span><span>Linux</span><span>WebRTC</span></p></div></article>
<article><img src="img/sensores.svg" alt=""><div><h3>Estación de clima solar</h3><p>Sensores LoRa que reportan temperatura y humedad desde la azotea; un año con la misma batería.</p><p class=tags><span>ESP32</span><span>LoRa</span></p></div></article>
<article><img src="img/mapa.svg" alt=""><div><h3>Mapa de afluencia</h3><p>Visualización anónima de cuánta gente pasa por una feria, sin cámaras ni datos personales.</p><p class=tags><span>Privacidad</span><span>Datos</span></p></div></article>
<h2 id=contacto>Contacto</h2>
<p class=contacto>¿Tienes un proyecto en mente? Escríbeme a <b>yael@example.com</b> o visita <b>example.com</b> cuando tengas datos.</p></main>
<footer>© 2026 Yael Montoya</footer></body></html>"##;

const SITIO_CSS: &str = r#"*{box-sizing:border-box}body{margin:0;font:16px/1.55 system-ui,-apple-system,sans-serif;background:#0d1321;color:#e6ebf5}
.hero{padding:40px 22px 30px;background:radial-gradient(circle at 20% 0%,#1d7f86 0,#16365f 55%,#0d1321 100%)}
.marca{width:56px;height:56px;border-radius:16px;background:#f4b860;color:#0d1321;font:800 22px/56px system-ui;text-align:center;margin-bottom:18px}
h1{font-size:34px;margin:0;letter-spacing:-.5px}.rol{margin:4px 0 14px;color:#9fd8d2;font-weight:600}.intro{margin:0 0 20px;color:#cfd6e4;max-width:34em}
nav a{display:inline-block;margin-right:10px;padding:9px 16px;border-radius:999px;background:#ffffff1c;color:#fff;text-decoration:none;font-weight:600;font-size:15px}
main{padding:8px 18px 30px;max-width:720px;margin:auto}h2{font-size:20px;margin:26px 0 12px}
article{display:flex;gap:14px;background:#151d30;border:1px solid #ffffff14;border-radius:16px;padding:14px;margin-bottom:12px}
article img{width:84px;height:84px;border-radius:12px;flex:none}h3{margin:0 0 4px;font-size:17px}article p{margin:0;color:#b8c1d3;font-size:14.5px}
.tags{margin-top:8px!important}.tags span{display:inline-block;font-size:12px;font-weight:700;color:#f4b860;background:#f4b86018;border-radius:6px;padding:2px 8px;margin:0 6px 4px 0}
.contacto{background:#151d30;border-radius:16px;padding:14px;color:#cfd6e4}footer{text-align:center;color:#6c7891;font-size:13px;padding:10px 0 24px}"#;

const IMG_STICK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 84 84"><rect width="84" height="84" fill="#1d7f86"/><rect x="14" y="30" width="46" height="24" rx="6" fill="#e6ebf5"/><rect x="60" y="35" width="12" height="14" rx="2" fill="#9fb1c9"/><circle cx="24" cy="42" r="3" fill="#3ecf8e"/><circle cx="33" cy="42" r="3" fill="#4f8cff"/><path d="M22 22c10-8 22-8 32 0M28 16c6-4 14-4 20 0" stroke="#f4b860" stroke-width="3" fill="none" stroke-linecap="round"/></svg>"##;
const IMG_SENSORES: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 84 84"><rect width="84" height="84" fill="#3a2f6b"/><circle cx="58" cy="24" r="10" fill="#f4b860"/><rect x="18" y="40" width="22" height="30" rx="4" fill="#e6ebf5"/><rect x="24" y="47" width="10" height="4" rx="2" fill="#3a2f6b"/><path d="M29 40V28" stroke="#e6ebf5" stroke-width="3"/><path d="M48 66h22M52 58h14" stroke="#9fd8d2" stroke-width="3" stroke-linecap="round"/></svg>"##;
const IMG_MAPA: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 84 84"><rect width="84" height="84" fill="#16365f"/><rect x="12" y="50" width="9" height="20" rx="2" fill="#4f8cff"/><rect x="25" y="38" width="9" height="32" rx="2" fill="#4f8cff"/><rect x="38" y="24" width="9" height="46" rx="2" fill="#f4b860"/><rect x="51" y="34" width="9" height="36" rx="2" fill="#4f8cff"/><rect x="64" y="46" width="9" height="24" rx="2" fill="#4f8cff"/></svg>"##;

fn llenar_portafolio(dir: &Path) -> Portafolio {
    let p = Portafolio::with_dir(dir.to_path_buf());
    let r = p.post("preparar", &[("sitio", ""), ("nombre", "Portafolio 2025")], &[]);
    let viejo = r.strip_prefix("OK ").unwrap().to_string();
    p.post("subir", &[("sitio", &viejo), ("ruta", "index.html")], b"<h1>Portafolio 2025</h1>");
    let r = p.post("preparar", &[("sitio", ""), ("nombre", "Portafolio 2026")], &[]);
    let id = r.strip_prefix("OK ").unwrap().to_string();
    for (ruta, datos) in [
        ("index.html", SITIO_INDEX),
        ("estilo.css", SITIO_CSS),
        ("img/stick.svg", IMG_STICK),
        ("img/sensores.svg", IMG_SENSORES),
        ("img/mapa.svg", IMG_MAPA),
    ] {
        p.post("subir", &[("sitio", &id), ("ruta", ruta)], datos.as_bytes());
    }
    p.post("usar", &[("sitio", &id)], &[]);
    p.post("url", &[("url", "https://example.com/"), ("titulo", DUENO), ("subtitulo", "Sistemas embebidos y Rust")], &[]);
    for n in 10..47 {
        visita(&p, n);
    }
    p
}

fn llenar_landing(dir: &Path) -> Portafolio {
    let p = Portafolio::with_dir(dir.to_path_buf());
    p.post("url", &[("url", "https://example.com/"), ("titulo", DUENO), ("subtitulo", "Sistemas embebidos, Rust y redes")], &[]);
    p
}

fn visita(v: &dyn Vitrina, n: u8) {
    v.guest(&GuestReq { method: "GET", path: "/", query: "", host: "192.168.4.1", user_agent: "Mozilla/5.0", content_type: "", body: b"", client_ip: ip(n) });
}

fn llenar_tarjeta(dir: &Path) -> Tarjeta {
    let t = Tarjeta::with_dir(dir.to_path_buf());
    t.admin_post(
        "guardar",
        &kv(&[
            ("nombre", "Yael"),
            ("apellido", "Montoya"),
            ("puesto", "Desarrollador de sistemas embebidos"),
            ("empresa", "Estudio independiente"),
            ("telefonos", "Móvil|+52 55 0000 0000"),
            ("email", "yael@example.com"),
            ("web", "https://example.com"),
            ("redes", "GitHub|https://example.com/github\nLinkedIn|https://example.com/linkedin"),
            ("nota", "Hago que hardware barato haga cosas útiles: Rust, Linux y redes."),
            ("color", "#1d7f86"),
        ]),
        b"",
    );
    assert_eq!(t.admin_post("foto", &[], &avatar_png()), "Foto actualizada");
    fs::write(dir.join("descargas.txt"), format!("{} 14", hoy_sistema())).unwrap();
    Tarjeta::with_dir(dir.to_path_buf())
}

fn multipart(mensaje: &str, apodo: &str, archivo: Option<(&str, &str, &[u8])>) -> (String, Vec<u8>) {
    const B: &str = "----capturasBoundary7MA4YWxk";
    let mut b = Vec::new();
    for (k, v) in [("mensaje", mensaje), ("apodo", apodo)] {
        b.extend_from_slice(format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    let (n, t, d) = archivo.unwrap_or(("", "application/octet-stream", b""));
    b.extend_from_slice(format!("--{B}\r\nContent-Disposition: form-data; name=\"archivo\"; filename=\"{n}\"\r\nContent-Type: {t}\r\n\r\n").as_bytes());
    b.extend_from_slice(d);
    b.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={B}"), b)
}

fn llenar_buzon(dir: &Path) -> Buzon {
    let bz = Buzon::with_dir(dir.to_path_buf());
    let mut pdf = b"%PDF-1.4\n% propuesta simulada\n".to_vec();
    pdf.resize(184_320, b' ');
    type Adjunto<'a> = Option<(&'a str, &'a str, &'a [u8])>;
    let msgs: [(&str, &str, Adjunto, i64); 6] = [
        ("¡Qué buena idea poner un buzón sin internet! Me encantó la charla de hoy.", "Lucía", None, 290),
        ("¿Darás el taller de Rust otra vez el próximo mes? Me apunto con dos amigos.", "Marco", None, 205),
        ("Te dejo la propuesta para el stand de la feria, la revisamos cuando puedas.", "Daniela", Some(("propuesta-stand.pdf", "application/pdf", &pdf)), 150),
        ("El café de la esquina tiene mejor señal, pero este WiFi tiene mejor conversación", "", None, 95),
        ("Gracias por explicar lo del portal cautivo. ¿Dónde puedo ver el código?", "Iván", None, 40),
        ("Me gustaría contratarte para un proyecto con sensores. Te escribo por correo.", "Sofía", None, 12),
    ];
    for (i, (texto, apodo, adj, _)) in msgs.iter().enumerate() {
        let (ct, body) = multipart(texto, apodo, *adj);
        let r = bz.guest(&GuestReq { method: "POST", path: "/enviar", query: "", host: "192.168.4.1", user_agent: "Mozilla/5.0", content_type: &ct, body: &body, client_ip: ip(20 + i as u8) });
        assert_eq!(r.headers.iter().find(|(k, _)| k == "Location").map(|x| x.1.as_str()), Some("/?r=ok"));
    }
    // horas creíbles: cada mensaje "hace N minutos"
    let t = ahora();
    let mut archivos: Vec<PathBuf> = fs::read_dir(dir.join("msgs")).unwrap().flatten().map(|e| e.path()).collect();
    archivos.sort();
    for a in archivos {
        let s = fs::read_to_string(&a).unwrap();
        let n: usize = s.split("\"n\":").nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
        let ts_viejo = s.split("\"ts\":").nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().to_string();
        let nuevo = (t - msgs[n - 1].3 * 60).to_string();
        fs::write(&a, s.replacen(&format!("\"ts\":{ts_viejo}"), &format!("\"ts\":{nuevo}"), 1)).unwrap();
    }
    fs::write(dir.join("config"), "muro=1\ncuota_mb=500\nvisto=3\n").unwrap();
    Buzon::with_dir(dir.to_path_buf())
}

fn llenar_chat(dir: &Path) -> Chat {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join("config"),
        format!("bienvenida=¡Hola! Este chat solo existe en este WiFi. Sean amables y pregunten lo que quieran.\ndueno={DUENO}\nsolo_lectura=0\n"),
    )
    .unwrap();
    let conv: [(&str, bool, &str, i64); 9] = [
        ("192.168.4.31", false, "Lucía", 21),
        ("192.168.4.44", false, "Marco", 19),
        ("", true, DUENO, 18),
        ("192.168.4.31", false, "Lucía", 15),
        ("", true, DUENO, 14),
        ("192.168.4.52", false, "Andrés", 9),
        ("192.168.4.44", false, "Marco", 6),
        ("", true, DUENO, 4),
        ("192.168.4.52", false, "Andrés", 2),
    ];
    let textos = [
        "¿Alguien sabe a qué hora empieza la demo?",
        "Creo que a las 7, junto al escenario",
        "¡Hola! La demo empieza a las 19:00 en la mesa 4. Traigan su teléfono.",
        "Gracias. ¿Se puede pasar un video por aquí?",
        "Sí: abre «Compartir archivos» en este mismo WiFi. Va directo de teléfono a teléfono.",
        "¿Esto funciona sin internet?",
        "Sí, todo corre en el stick. Lo acabo de probar",
        "Exacto, nada sale a internet. Los mensajes se quedan aquí.",
        "Muy bueno. Nos vemos en la demo",
    ];
    let t = ahora();
    let mut s = String::new();
    for (i, ((ipc, host, apodo, min), texto)) in conv.iter().zip(textos).enumerate() {
        s += &format!("{}\t{}\t{}\t{}\t{}\t{}\n", i + 1, t - min * 60, ipc, *host as u8, apodo, texto);
    }
    fs::write(dir.join("mensajes.txt"), s).unwrap();
    let c = Chat::with_dir(dir.to_path_buf());
    for n in [31, 44, 52, 60, 61] {
        visita(&c, n);
    }
    c
}

fn llenar_compartir(dir: &Path) -> Compartir {
    fs::create_dir_all(dir).unwrap();
    let hoy = hoy_sistema();
    // días anteriores a partir de la fecha del sistema
    let (y, m, d): (i64, i64, i64) = {
        let v: Vec<i64> = hoy.split('-').map(|x| x.parse().unwrap()).collect();
        (v[0], v[1], v[2])
    };
    let dia_hoy = {
        let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m - 3) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    let filas = [(3, 9, 8, 2_480_000_000u64), (2, 14, 13, 5_120_000_000), (1, 6, 6, 940_000_000), (0, 11, 10, 3_310_000_000)];
    let s: String = filas.iter().map(|(hace, i, k, b)| format!("{} {i} {k} {b}\n", fecha(dia_hoy - hace))).collect();
    fs::write(dir.join("estadisticas.txt"), s).unwrap();
    Compartir::with_dir(dir.to_path_buf())
}

/// 24 h de agregados cada 5 min con forma de día real (pico al mediodía y al anochecer).
fn llenar_afluencia(dir: &Path) -> Afluencia {
    fs::create_dir_all(dir).unwrap();
    let utc = utc_min();
    let t = ahora();
    let actual = t - t.rem_euclid(300);
    let mut rng = Lcg(0x5eed);
    let curva = |h: f64| {
        let g = |c: f64, a: f64, w: f64| a * (-((h - c) / w).powi(2)).exp();
        2.0 + g(13.5, 34.0, 2.2) + g(19.5, 26.0, 1.8) + g(9.0, 12.0, 1.3)
    };
    let mut por_dia: std::collections::BTreeMap<String, String> = Default::default();
    let mut tv = actual - 86_400 + 300;
    while tv < actual {
        let hl = ((tv + utc * 60).rem_euclid(86_400)) as f64 / 3600.0;
        let est = (curva(hl) * (0.8 + rng.hasta(40) as f64 / 100.0)).round().max(1.0) as u32;
        let fijas = (est as f64 * 0.3).round() as u32;
        let huellas = est - fijas;
        let aleatorias = huellas * 2 + rng.hasta(huellas as u64 + 2) as u32;
        let cerca = (est as f64 * 0.35).round() as u32;
        let medio = (est as f64 * 0.4).round() as u32;
        let lejos = est - cerca.min(est) - medio.min(est - cerca.min(est));
        let probes = est * 18 + rng.hasta(40) as u32;
        let dia = fecha((tv + utc * 60).div_euclid(86_400));
        *por_dia.entry(dia).or_default() += &format!("{tv}\t{fijas}\t{aleatorias}\t{huellas}\t{cerca}\t{medio}\t{lejos}\t{probes}\n");
        tv += 300;
    }
    for (dia, s) in por_dia {
        fs::write(dir.join(format!("{dia}.tsv")), s).unwrap();
    }
    let a = Afluencia::con_dir(dir.to_path_buf());
    // ventana en curso: unos cuantos teléfonos (MACs inventadas, nunca se muestran)
    let hl = ((t + utc * 60).rem_euclid(86_400)) as f64 / 3600.0;
    let n = curva(hl).round() as u64;
    let fijas = (n * 3).div_ceil(10);
    let huellas = (n - fijas).max(1);
    let mut probes = Vec::new();
    for i in 0..n {
        let fija = i < fijas;
        // los de MAC aleatoria la cambian: 2 MACs por teléfono, misma huella
        for _ in 0..if fija { 1 } else { 2 } {
            let mut mac = [0u8; 6];
            for b in mac.iter_mut() {
                *b = rng.sig() as u8;
            }
            mac[0] = if fija { mac[0] & 0xfc } else { (mac[0] & 0xfc) | 0x02 };
            let huella = if fija { 0 } else { i % huellas };
            let rssi = -45 - rng.hasta(45) as i8;
            for _ in 0..1 + rng.hasta(6) {
                probes.push(Probe { mac, rssi: Some(rssi), huella });
            }
        }
    }
    a.simular_captura(6, &probes, 48_211, 9_870);
    a
}

// ───────────────────────────── estado del daemon (simulado)

fn estado() -> Arc<Mutex<State>> {
    Arc::new(Mutex::new(State {
        mode: Mode::Vitrina("portafolio"),
        effective: Mode::Vitrina("portafolio"),
        powerbank: Some("tarjeta"),
        host_present: true,
        config_mode: false,
        config_since: None,
        request: None,
        button_held: false,
        internet: true,
        uplink: "wwan0".into(),
        laptop: None,
        usb_sharing: false,
        scan: vec![("Café Aurora".into(), 82, "WPA2".into()), ("Cowork Norte".into(), 61, "WPA2".into()), ("Biblioteca Central".into(), 38, "".into())],
    }))
}

const ESTADO_JSON: &str = r#"{"modo":"vitrina:portafolio","efectivo":"Vitrina: Portafolio","configuracion":false,"internet":true,"salida":"LTE","pc_conectada":true,"powerbank":"tarjeta","laptop":"","laptop_comparte":false,"wifi":"","lte":{"estado":"conectado","senal":"74","operador":"Operador Demo","tecnologia":"LTE","sim":true},"wireguard":"no configurado","ips":"usb0             UP             192.168.5.1/24\nwlan0            UP             192.168.4.1/24\nwwan0            UP             10.0.0.2/30","vitrinas":{}}"#;

fn redes_guardadas() -> Vec<(String, String)> {
    vec![("Café Aurora".into(), "Café Aurora".into()), ("Cowork Norte".into(), "Cowork Norte".into())]
}

// ───────────────────────────── servidores

fn leer(rq: &mut tiny_http::Request, max: usize) -> (String, String, String, String, String, String, Vec<u8>) {
    let hdr = |n: &'static str| rq.headers().iter().find(|h| h.field.equiv(n)).map(|h| h.value.as_str().to_string()).unwrap_or_default();
    let (host, ua, ct) = (hdr("Host"), hdr("User-Agent"), hdr("Content-Type"));
    let url = rq.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let (path, query) = (path.to_string(), query.to_string());
    let method = rq.method().as_str().to_string();
    let mut body = Vec::new();
    let _ = rq.as_reader().take(max as u64 + 1).read_to_end(&mut body);
    (method, path, query, host, ua, ct, body)
}

/// Igual que el servidor de invitados de captive.rs (sin sondas de portal cautivo).
fn servir_invitados(puerto: u16, vit: Vitrinas, id: &str) {
    let server = tiny_http::Server::http(("127.0.0.1", puerto)).unwrap();
    let v = {
        let g = vit.lock().unwrap();
        g.iter().find(|x| x.id() == id).cloned()
    };
    let Some(v) = v else { return };
    for mut rq in server.incoming_requests() {
        let cliente = rq.remote_addr().map(|a| a.ip()).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let (method, path, query, host, ua, ct, body) = leer(&mut rq, v.max_body());
        let r = if path == invitados::RUTA {
            invitados::pagina(cliente, &method)
        } else if body.len() > v.max_body() {
            GuestResp { status: 413, content_type: "text/plain".into(), headers: vec![], body: vec![] }
        } else {
            let g = GuestReq { method: &method, path: &path, query: &query, host: &host, user_agent: &ua, content_type: &ct, body: &body, client_ip: cliente };
            invitados::procesar_respuesta(v.guest(&g), cliente, &method)
        };
        let _ = rq.respond(captive::to_tiny(r));
    }
}

fn servir_panel(puerto: u16, st: Arc<Mutex<State>>, vit: Vitrinas) {
    let server = tiny_http::Server::http(("127.0.0.1", puerto)).unwrap();
    for mut rq in server.incoming_requests() {
        let (method, path, query, _, _, _, _) = leer(&mut rq, 8192);
        let q = crate::util::form(&query);
        let qv = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone()).unwrap_or_default();
        let r = if path == "/api/estado" {
            GuestResp::bytes("application/json; charset=utf-8", ESTADO_JSON.as_bytes().to_vec())
        } else if let (Some(rest), "GET") = (path.strip_prefix("/vitrina/"), method.as_str()) {
            let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
            let v = {
                let g = vit.lock().unwrap();
                g.iter().find(|x| x.id() == id).cloned()
            };
            v.and_then(|v| v.admin_get(sub)).unwrap_or_else(GuestResp::not_found)
        } else if method == "GET" && path == "/" {
            let abrir = qv("abrir");
            let abrir: Option<&str> = ["invitados", "portafolio", "tarjeta", "buzon", "chat", "compartir", "afluencia"].into_iter().find(|x| *x == abrir);
            GuestResp::html(portal::page_html(&st, &vit, &crate::util::html(&qv("msg")), abrir, &redes_guardadas()))
        } else {
            GuestResp::redirect("/")
        };
        let _ = rq.respond(captive::to_tiny(r));
    }
}

#[test]
#[ignore]
fn capturas_servidor() {
    let p: u16 = std::env::var("CAPTURAS_PUERTO").ok().and_then(|p| p.parse().ok()).unwrap_or(8740);
    let raiz = std::env::var("CAPTURAS_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("stickd-capturas"));
    let _ = fs::remove_dir_all(&raiz);
    fs::create_dir_all(&raiz).unwrap();

    invitados::simular(
        invitados::Config { activo: true, minutos: 30, baja_mbps: 5.0, sube_mbps: 1.0, max_disp: 5, tope_mb: 300, exigir_vista: true, horas_espera: 24 },
        "wwan0",
        &[
            (Ipv4Addr::new(192, 168, 4, 23), "Pixel-7", "", 18, 41_800_000),
            (Ipv4Addr::new(192, 168, 4, 37), "iPhone", "", 6, 112_400_000),
        ],
    );
    let vit: Vitrinas = Arc::new(Mutex::new(vec![
        Arc::new(llenar_portafolio(&raiz.join("portafolio"))),
        Arc::new(llenar_tarjeta(&raiz.join("tarjeta"))),
        Arc::new(llenar_buzon(&raiz.join("buzon"))),
        Arc::new(llenar_chat(&raiz.join("chat"))),
        Arc::new(llenar_compartir(&raiz.join("compartir"))),
        Arc::new(llenar_afluencia(&raiz.join("afluencia"))),
    ]));
    let landing: Vitrinas = Arc::new(Mutex::new(vec![Arc::new(llenar_landing(&raiz.join("landing")))]));

    let mut hilos = Vec::new();
    for (id, puerto) in [("portafolio", p + 1), ("tarjeta", p + 3), ("buzon", p + 4), ("chat", p + 5), ("compartir", p + 6)] {
        let v = vit.clone();
        hilos.push(std::thread::spawn(move || servir_invitados(puerto, v, id)));
    }
    hilos.push(std::thread::spawn(move || servir_invitados(p + 2, landing, "portafolio")));
    let st = estado();
    let v = vit.clone();
    hilos.push(std::thread::spawn(move || servir_panel(p, st, v)));
    eprintln!("capturas: listo en 127.0.0.1:{p}..{} (datos en {})", p + 6, raiz.display());
    for h in hilos {
        let _ = h.join();
    }
}
