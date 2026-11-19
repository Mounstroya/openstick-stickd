use super::*;
use std::net::Ipv4Addr;

fn tmp(n: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("stickd-compartir-{n}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn ip(n: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 4, n))
}

fn req<'a>(method: &'a str, path: &'a str, query: &'a str, body: &'a [u8], ip_: IpAddr) -> GuestReq<'a> {
    GuestReq { method, path, query, host: "192.168.4.1", user_agent: "Mozilla/5.0 Chrome/125", content_type: "text/plain", body, client_ip: ip_ }
}

fn texto(r: &GuestResp) -> String {
    String::from_utf8(r.body.clone()).unwrap()
}

/// Saca el valor de una clave string de un JSON plano (suficiente para estos tests).
fn campo(j: &str, k: &str) -> String {
    let pat = format!("\"{k}\":\"");
    let i = j.find(&pat).unwrap_or_else(|| panic!("falta {k} en {j}")) + pat.len();
    j[i..].split('"').next().unwrap().to_string()
}

fn crear(p: &Compartir, n: u8) -> (String, String, String) {
    let j = texto(&p.guest(&req("POST", "/api/crear", "", b"", ip(n))));
    assert!(j.contains("\"ok\":1"), "{j}");
    (campo(&j, "c"), campo(&j, "s"), campo(&j, "t"))
}

fn unirse(p: &Compartir, c: &str, n: u8) -> String {
    let q = format!("c={c}");
    texto(&p.guest(&req("POST", "/api/unirse", &q, b"", ip(n))))
}

fn recibir(p: &Compartir, s: &str, t: &str, desde: u64, n: u8) -> String {
    let q = format!("s={s}&t={t}&desde={desde}");
    texto(&p.guest(&req("GET", "/api/recibir", &q, b"", ip(n))))
}

fn enviar(p: &Compartir, s: &str, t: &str, tipo: &str, body: &str, n: u8) -> String {
    let q = format!("s={s}&t={t}&tipo={tipo}&g=1");
    texto(&p.guest(&req("POST", "/api/enviar", &q, body.as_bytes(), ip(n))))
}

const SDP: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:0\r\n";
const CAND_MDNS: &str = "candidate:1 1 udp 2122260223 0b5c2f0e-1111-4a2b-9c3d-123456789abc.local 54321 typ host generation 0\n0\n0";

#[test]
fn codigos_y_palabras() {
    use super::palabras::*;
    let mut vistas = std::collections::HashSet::new();
    for w in PALABRAS {
        assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
        assert!(vistas.insert(fonetica(w)), "palabra que suena igual a otra: {w}");
    }
    assert!(PALABRAS.len() >= 150);
    assert_eq!(normalizar("7 Luna, PERRO–nube").as_deref(), Some("7-luna-perro-nube"));
    assert_eq!(normalizar(" 7-lúna-perro-nube ").as_deref(), Some("7-luna-perro-nube"));
    assert_eq!(normalizar("12-baca-sorro-yuvia").as_deref(), Some("12-vaca-zorro-lluvia"), "de oído");
    assert_eq!(normalizar("0-luna-perro-nube"), None);
    assert_eq!(normalizar("100-luna-perro-nube"), None);
    assert_eq!(normalizar("07-luna-perro-nube"), None);
    assert_eq!(normalizar("7-luna-perro"), None);
    assert_eq!(normalizar("7-luna-perro-xyzzy"), None);
    for _ in 0..200 {
        let c = generar();
        assert_eq!(normalizar(&c).as_deref(), Some(c.as_str()));
    }
}

#[test]
fn flujo_de_senalizacion() {
    let d = tmp("flujo");
    let p = Compartir::with_dir(d.clone());
    let (c, s, te) = crear(&p, 10);
    assert_eq!(s.len(), 32);
    assert_ne!(s, te);
    // el receptor escribe el código "a su manera"
    let j = unirse(&p, &c.replace('-', " ").to_uppercase(), 20);
    assert!(j.contains("\"ok\":1"), "{j}");
    assert_eq!(campo(&j, "s"), s, "el id de sala sale del código");
    let tr = campo(&j, "t");
    assert_ne!(tr, te);
    // el emisor ve "hola" y que hay alguien
    let r = recibir(&p, &s, &te, 0, 10);
    assert!(r.contains("\"o\":1") && r.contains("\"hola\""), "{r}");
    // oferta → receptor
    assert!(enviar(&p, &s, &te, "offer", SDP, 10).contains("\"ok\":1"));
    let r = recibir(&p, &s, &tr, 0, 20);
    assert!(r.contains("\"offer\",1,\"v=0\\u000d\\no=-"), "{r}");
    // el receptor no puede mandar "offer" ni el emisor "answer"
    assert!(enviar(&p, &s, &tr, "offer", SDP, 20).contains("invalido"));
    assert!(enviar(&p, &s, &te, "answer", SDP, 10).contains("invalido"));
    assert!(enviar(&p, &s, &tr, "answer", SDP, 20).contains("\"ok\":1"));
    // acuse: con desde=último seq la cola queda vacía
    let r = recibir(&p, &s, &te, 0, 10);
    assert!(r.contains("\"answer\""), "{r}");
    assert!(recibir(&p, &s, &te, 2, 10).contains("\"m\":[]"));
    // token equivocado o sala inexistente
    assert!(recibir(&p, &s, "00", 0, 10).contains("nosala"));
    assert!(recibir(&p, "abc", &te, 0, 10).contains("nosala"));
    // un segundo receptor no entra mientras el primero sigue vivo
    assert!(unirse(&p, &c, 30).contains("ocupada"));
    // estadísticas: solo cuenta el receptor, una vez
    let rep = |t: &str, ev: &str| {
        let q = format!("s={s}&t={t}&ev={ev}&b=52428800");
        texto(&p.guest(&req("POST", "/api/reporte", &q, b"", ip(20))))
    };
    assert!(rep(&te, "inicio").contains("invalido"));
    assert!(rep(&tr, "inicio").contains("\"ok\":1"));
    assert!(rep(&tr, "inicio").contains("\"ok\":1"));
    assert!(rep(&tr, "fin").contains("\"ok\":1"));
    assert_eq!(p.hoy_dia(), Dia { iniciadas: 1, completas: 1, bytes: 52428800 });
    assert_eq!(p.resumen(), "1 transferencia hoy");
    // persiste en disco y se relee
    let p2 = Compartir::with_dir(d.clone());
    assert_eq!(p2.hoy_dia().bytes, 52428800);
    // el receptor sale: el emisor recibe "bye" y otro puede entrar
    assert!(texto(&p.guest(&req("POST", "/api/salir", &format!("s={s}&t={tr}"), b"", ip(20)))).contains("\"ok\":1"));
    assert!(recibir(&p, &s, &te, 2, 10).contains("\"bye\""));
    assert!(unirse(&p, &c, 30).contains("\"ok\":1"));
    // el emisor sale: la sala desaparece
    assert!(texto(&p.guest(&req("POST", "/api/salir", &format!("s={s}&t={te}"), b"", ip(10)))).contains("\"ok\":1"));
    assert!(unirse(&p, &c, 31).contains("\"codigo\""));
    let _ = fs::remove_dir_all(d);
}

#[test]
fn candidatos_mdns_se_reescriben() {
    let p = Compartir::with_dir(tmp("mdns"));
    let (c, s, te) = crear(&p, 10);
    let tr = campo(&unirse(&p, &c, 20), "t");
    assert!(enviar(&p, &s, &tr, "ice", CAND_MDNS, 20).contains("\"ok\":1"));
    let r = recibir(&p, &s, &te, 0, 10);
    assert!(r.contains(".local 54321 typ host"), "{r}");
    assert!(r.contains("candidate:1 1 udp 2122260223 192.168.4.20 54321 typ host generation 0\\n0\\n0"), "{r}");
    // candidato con IP normal: sin copia
    assert!(enviar(&p, &s, &tr, "ice", "candidate:2 1 udp 1 192.168.4.20 5000 typ host\n0\n0", 20).contains("\"ok\":1"));
    let r = recibir(&p, &s, &te, 3, 10);
    assert_eq!(r.matches("candidate:").count(), 1, "{r}");
    // basura
    for malo in [
        "candidate:1 1 udp 1 x.local 99999 typ host\n0\n0",
        "hola\n0\n0",
        "candidate:1 1 sctp 1 1.2.3.4 1 typ host\n0\n0",
        "candidate:1 1 udp 1 1.2.3.4 1 typ host\n<script>\n0",
        "candidate:1 1 udp 1 1.2.3.4 1 typ host\n0\n0\nextra",
        "candidate:1 1 udp 1 1.2.3.4 1 typ host\u{7}\n0\n0",
    ] {
        assert!(enviar(&p, &s, &tr, "ice", malo, 20).contains("invalido"), "{malo:?}");
    }
    // SDP con candidatos dentro
    let sdp = format!("{SDP}a=candidate:1 1 udp 2122260223 abc.local 40000 typ host\r\n");
    assert!(enviar(&p, &s, &te, "offer", &sdp, 10).contains("\"ok\":1"));
    let r = recibir(&p, &s, &tr, 0, 20);
    assert!(r.contains("a=candidate:1 1 udp 2122260223 192.168.4.10 40000 typ host"), "{r}");
    assert!(enviar(&p, &s, &te, "offer", "no es sdp", 10).contains("invalido"));
}

#[test]
fn limites() {
    let p = Compartir::with_dir(tmp("lim"));
    // salas por IP
    for _ in 0..SALAS_POR_IP {
        crear(&p, 10);
    }
    assert!(texto(&p.guest(&req("POST", "/api/crear", "", b"", ip(10)))).contains("\"salas\""));
    // intentos de unirse por minuto (fuerza bruta del código)
    let mut bloqueado = false;
    for _ in 0..(LIM_UNIRSE + 2) {
        if unirse(&p, "5-luna-perro-nube", 50).contains("limite") {
            bloqueado = true;
        }
    }
    assert!(bloqueado);
    // SDP demasiado grande y cola llena
    let (c, s, te) = crear(&p, 11);
    let tr = campo(&unirse(&p, &c, 21), "t");
    let grande = format!("{SDP}a={}\r\n", "x".repeat(MAX_SDP));
    assert!(enviar(&p, &s, &te, "offer", &grande, 11).contains("grande"));
    let mut lleno = false;
    for _ in 0..(MAX_COLA + 2) {
        if enviar(&p, &s, &tr, "ice", "candidate:2 1 udp 1 192.168.4.21 5000 typ host\n0\n0", 21).contains("lleno") {
            lleno = true;
        }
    }
    assert!(lleno);
    // tope global
    let p = Compartir::with_dir(tmp("lim2"));
    for i in 0..MAX_SALAS {
        crear(&p, 100 + (i / SALAS_POR_IP) as u8);
    }
    assert!(texto(&p.guest(&req("POST", "/api/crear", "", b"", ip(250)))).contains("\"salas\""));
    // el admin vacía todo
    assert!(p.admin_html().contains(&format!("Sesiones activas ({MAX_SALAS})")));
    assert!(p.admin_post("vaciar", &[], b"").contains("confirmar"));
    assert!(p.admin_post("vaciar", &[("confirmar".into(), "si".into())], b"").contains("40 sesiones cerradas"));
    assert!(p.admin_html().contains("Sesiones activas (0)"));
}

#[test]
fn caducidad() {
    let p = Compartir::with_dir(tmp("cad"));
    let (c, s, _) = crear(&p, 10);
    {
        let mut st = p.lock();
        let sala = st.salas.get_mut(&s).unwrap();
        sala.creada -= TTL_ESPERA + Duration::from_secs(1);
    }
    assert!(unirse(&p, &c, 20).contains("\"codigo\""), "código caducado");
    assert!(p.lock().salas.is_empty());
}

#[test]
fn pagina_qr_y_navegadores() {
    let p = Compartir::with_dir(tmp("pag"));
    let h = texto(&p.guest(&req("GET", "/", "", b"", ip(10))));
    assert!(h.contains("Compartir archivos") && h.contains("iceServers:[]"));
    assert!(!h.to_lowercase().contains("pásalo") && !h.to_lowercase().contains("pasalo"));
    assert!(!h.contains("id=salir "), "navegador normal: sin aviso");
    // /r/<código> rellena el código (normalizado y escapado)
    let h = texto(&p.guest(&req("GET", "/r/7-Luna-perro-nube", "", b"", ip(10))));
    assert!(h.contains("data-codigo=\"7-luna-perro-nube\""));
    let h = texto(&p.guest(&req("GET", "/r/\"><script>", "", b"", ip(10))));
    assert!(h.contains("data-codigo=\"\""));
    // QR
    let r = p.guest(&req("GET", "/qr.svg", "c=7-luna-perro-nube", b"", ip(10)));
    assert_eq!(r.content_type, "image/svg+xml");
    assert!(texto(&r).starts_with("<svg"));
    assert_eq!(p.guest(&req("GET", "/qr.svg", "c=<x>", b"", ip(10))).status, 404);
    // dominio cualquiera (DNS secuestrado) → a la IP del stick
    let mut g = req("GET", "/r/7-luna-perro-nube", "", b"", ip(10));
    g.host = "captive.example.com";
    let r = p.guest(&g);
    assert_eq!(r.status, 302);
    assert_eq!(r.headers[0].1, "http://192.168.4.1/r/7-luna-perro-nube");
    // mini-navegadores de portal cautivo
    let mut g = req("GET", "/", "", b"", ip(10));
    g.user_agent = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148";
    let h = texto(&p.guest(&g));
    assert!(h.contains("id=salir ") && h.contains("Listo") && h.contains("Safari"));
    g.user_agent = "Mozilla/5.0 (Linux; Android 14; Pixel 7 Build/AP2A; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/125.0.0.0 Mobile Safari/537.36";
    let h = texto(&p.guest(&g));
    assert!(h.contains("intent://192.168.4.1/#Intent;scheme=http;package=com.android.chrome;end"));
    assert!(es_ip("192.168.4.1:8080") && es_ip("127.0.0.1") && !es_ip("foo.com") && !es_ip(""));
}

/// Servidor local para probar el flujo WebRTC real con dos navegadores:
///   COMPARTIR_PUERTO=8099 cargo test servidor_local -- --ignored --nocapture
#[test]
#[ignore]
fn servidor_local() {
    use std::io::Read as _;
    let puerto: u16 = std::env::var("COMPARTIR_PUERTO").ok().and_then(|p| p.parse().ok()).unwrap_or(8099);
    let p = Compartir::with_dir(tmp("servidor"));
    let server = tiny_http::Server::http(("0.0.0.0", puerto)).unwrap();
    eprintln!("escuchando en :{puerto}");
    for mut rq in server.incoming_requests() {
        let ipc = rq.remote_addr().map(|a| a.ip()).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let hdr = |n: &'static str| rq.headers().iter().find(|h| h.field.equiv(n)).map(|h| h.value.as_str().to_string()).unwrap_or_default();
        let (host, ua, ct) = (hdr("Host"), hdr("User-Agent"), hdr("Content-Type"));
        let url = rq.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let (path, query) = (path.to_string(), query.to_string());
        let method = rq.method().as_str().to_string();
        let mut body = Vec::new();
        let _ = rq.as_reader().take(p.max_body() as u64 + 1).read_to_end(&mut body);
        let g = GuestReq { method: &method, path: &path, query: &query, host: &host, user_agent: &ua, content_type: &ct, body: &body, client_ip: ipc };
        let r = if body.len() > p.max_body() { GuestResp { status: 413, content_type: "text/plain".into(), headers: vec![], body: vec![] } } else { p.guest(&g) };
        if path.starts_with("/api/") && std::env::var("COMPARTIR_LOG").is_ok() {
            eprintln!("{ipc} {method} {path} {} → {}", &query.chars().take(60).collect::<String>(), String::from_utf8_lossy(&r.body).chars().take(160).collect::<String>());
        }
        let _ = rq.respond(crate::captive::to_tiny(r));
    }
}
