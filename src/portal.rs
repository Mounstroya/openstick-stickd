//! Portal web en el puerto 80. Accesible desde el hotspot (192.168.4.x), el cable USB
//! (192.168.5.x), el túnel WireGuard (10.x) y localhost. Desde la LAN de casa NO.

use crate::util::{form, html, json};
use crate::{net, save_mode, Mode, Request, State};
use std::io::Read;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

fn allowed(ip: Option<IpAddr>) -> bool {
    match ip {
        Some(IpAddr::V4(v)) => {
            let o = v.octets();
            v.is_loopback() || (o[0] == 192 && o[1] == 168 && (o[2] == 4 || o[2] == 5)) || o[0] == 10
        }
        Some(IpAddr::V6(v)) => v.is_loopback(),
        None => false,
    }
}

pub fn spawn(state: Arc<Mutex<State>>) {
    std::thread::spawn(move || {
        let server = match Server::http("0.0.0.0:80") {
            Ok(s) => s,
            Err(e) => {
                crate::util::log(&format!("portal: no pude abrir el puerto 80: {e}"));
                return;
            }
        };
        for mut req in server.incoming_requests() {
            let ip = req.remote_addr().map(|a| a.ip());
            if !allowed(ip) {
                let _ = req.respond(Response::from_string("prohibido").with_status_code(403));
                continue;
            }
            let mut body = String::new();
            if *req.method() == Method::Post {
                let _ = req.as_reader().take(4096).read_to_string(&mut body);
            }
            let url = req.url().split('?').next().unwrap_or("/").to_string();
            let resp = match (req.method(), url.as_str()) {
                (Method::Get, "/api/estado") => {
                    Response::from_string(status_json(&state)).with_header(hdr("Content-Type", "application/json; charset=utf-8"))
                }
                (Method::Post, path) => {
                    let msg = handle_post(&state, path, &form(&body));
                    page(&state, &msg)
                }
                _ => page(&state, ""),
            };
            let _ = req.respond(resp);
        }
    });
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn field<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("")
}

fn handle_post(state: &Arc<Mutex<State>>, path: &str, f: &[(String, String)]) -> String {
    match path {
        "/modo" => match Mode::parse(field(f, "modo")) {
            Some(m) => {
                save_mode(m);
                let mut s = state.lock().unwrap();
                s.mode = m;
                s.request = Some(Request::ApplyMode);
                format!("Modo cambiado a «{}».", m.as_str())
            }
            None => "Modo no válido.".into(),
        },
        "/wifi" => {
            let ssid = if !field(f, "ssid_manual").is_empty() { field(f, "ssid_manual") } else { field(f, "ssid") };
            let psk = field(f, "psk");
            if ssid.is_empty() || ssid.len() > 32 {
                return "Falta el nombre de la red (máx. 32 caracteres).".into();
            }
            if !psk.is_empty() && (psk.len() < 8 || psk.len() > 63) {
                return "La contraseña WiFi debe tener de 8 a 63 caracteres.".into();
            }
            match net::add_wifi(ssid, psk) {
                Ok(name) => {
                    let mut s = state.lock().unwrap();
                    if s.config_mode {
                        s.request = Some(Request::ExitConfig);
                    }
                    format!("Red guardada como «{}». El stick saldrá del modo configuración y se conectará; este WiFi se apagará.", html(&name))
                }
                Err(e) => format!("No se pudo guardar: {}", html(&e)),
            }
        }
        "/olvidar" => {
            let name = field(f, "con");
            if net::forget_wifi(name) { format!("Red «{}» olvidada.", html(name)) } else { "No se pudo olvidar esa red.".into() }
        }
        "/config" => {
            let mut s = state.lock().unwrap();
            match field(f, "accion") {
                "entrar" => {
                    s.request = Some(Request::EnterConfig);
                    "Entrando a modo configuración: se apagará el WiFi cliente y aparecerá la red «Openstick».".into()
                }
                "salir" => {
                    s.request = Some(Request::ExitConfig);
                    "Saliendo de modo configuración.".into()
                }
                _ => "Acción no válida.".into(),
            }
        }
        _ => "Ruta desconocida.".into(),
    }
}

fn status_json(state: &Arc<Mutex<State>>) -> String {
    let (mode, cfg, internet, uplink, laptop, sharing) = {
        let s = state.lock().unwrap();
        (s.mode, s.config_mode, s.internet, s.uplink.clone(), s.laptop.clone(), s.usb_sharing)
    };
    let l = net::lte();
    format!(
        "{{\"modo\":{},\"configuracion\":{},\"internet\":{},\"salida\":{},\"laptop\":{},\"laptop_comparte\":{},\"wifi\":{},\"lte\":{{\"estado\":{},\"senal\":{},\"operador\":{},\"tecnologia\":{},\"sim\":{}}},\"wireguard\":{},\"ips\":{}}}",
        json(mode.as_str()),
        cfg,
        internet,
        json(&salida_humana(&uplink, sharing)),
        json(laptop.as_deref().unwrap_or("")),
        sharing,
        json(&net::current_ssid()),
        json(&l.state),
        json(&l.signal),
        json(&l.operator),
        json(&l.tech),
        l.sim,
        json(&net::wireguard()),
        json(&net::addrs())
    )
}

fn salida_humana(uplink: &str, sharing: bool) -> String {
    match uplink {
        "usb0" if sharing => "USB (laptop)".into(),
        "wlan0" => "WiFi".into(),
        u if u.starts_with("wwan") || u.starts_with("rmnet") => "LTE".into(),
        "" => "ninguna".into(),
        u => u.into(),
    }
}

fn page(state: &Arc<Mutex<State>>, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let (mode, cfg, scan) = {
        let s = state.lock().unwrap();
        (s.mode, s.config_mode, s.scan.clone())
    };
    let radio = |m: &str, label: &str, desc: &str| {
        format!(
            "<label class=opt><input type=radio name=modo value={m} {}><span><b>{label}</b><small>{desc}</small></span></label>",
            if mode.as_str() == m { "checked" } else { "" }
        )
    };
    let saved: String = net::saved_wifis()
        .iter()
        .map(|(n, ssid)| {
            format!(
                "<li><span>{}</span><form method=post action=/olvidar><input type=hidden name=con value=\"{}\"><button class=link>olvidar</button></form></li>",
                html(ssid),
                html(n)
            )
        })
        .collect();
    let options: String = scan
        .iter()
        .map(|(s, sig, sec)| format!("<option value=\"{0}\">{0} — {1}% {2}</option>", html(s), sig, html(sec)))
        .collect();
    let html_page = format!(
        r#"<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>OpenStick</title><style>
:root{{--bg:#0f1115;--card:#181b22;--tx:#e8eaf0;--mut:#8a90a0;--ac:#4f8cff;--ok:#3ecf8e;--bad:#ff5d5d}}
@media (prefers-color-scheme:light){{:root{{--bg:#f4f5f8;--card:#fff;--tx:#14161c;--mut:#5d6372}}}}
*{{box-sizing:border-box}}body{{margin:0;font:16px/1.45 system-ui,sans-serif;background:var(--bg);color:var(--tx)}}
main{{max-width:560px;margin:auto;padding:16px}}h1{{font-size:22px;margin:8px 0 16px}}h2{{font-size:16px;margin:0 0 10px}}
.card{{background:var(--card);border-radius:14px;padding:16px;margin-bottom:14px}}
.msg{{background:var(--ac);color:#fff;border-radius:10px;padding:10px 12px;margin-bottom:14px}}
dl{{display:grid;grid-template-columns:auto 1fr;gap:4px 12px;margin:0}}dt{{color:var(--mut)}}dd{{margin:0;word-break:break-word}}
.ok{{color:var(--ok)}}.bad{{color:var(--bad)}}pre{{margin:0;font-size:12px;white-space:pre-wrap;color:var(--mut)}}
.opt{{display:flex;gap:10px;padding:8px 0;align-items:flex-start}}.opt small{{display:block;color:var(--mut)}}
input[type=text],input[type=password],select{{width:100%;padding:10px;border-radius:8px;border:1px solid #3334;background:var(--bg);color:var(--tx);margin:4px 0 10px;font-size:16px}}
button{{background:var(--ac);color:#fff;border:0;border-radius:8px;padding:10px 16px;font-size:15px}}
button.link{{background:none;color:var(--bad);padding:0}}ul{{list-style:none;padding:0;margin:0 0 12px}}
li{{display:flex;justify-content:space-between;padding:6px 0;border-bottom:1px solid #8882}}
</style></head><body><main>
<h1>OpenStick · UZ801</h1>
{msg_html}
<section class=card><h2>Estado</h2><dl id=st><dt>Cargando…</dt><dd></dd></dl></section>
<section class=card><h2>Modo de trabajo</h2><form method=post action=/modo>
{r_auto}{r_usb}{r_casa}{r_lte}<button>Guardar modo</button></form></section>
<section class=card><h2>Redes WiFi</h2><ul>{saved}</ul>
<form method=post action=/wifi>
<label>Red detectada<select name=ssid><option value="">— elegir —</option>{options}</select></label>
<label>…o escribe el nombre<input type=text name=ssid_manual maxlength=32 autocomplete=off></label>
<label>Contraseña<input type=password name=psk maxlength=63></label>
<button>Guardar y conectar</button></form></section>
<section class=card><h2>Modo configuración</h2>
<p style="color:var(--mut);margin-top:0">También se activa con una pulsación corta del botón, o solo si pasan 2 min sin internet. Pulsación de 8 s = reiniciar.</p>
<form method=post action=/config><input type=hidden name=accion value={accion}><button>{accion_txt}</button></form></section>
</main><script>
async function f(){{try{{const r=await fetch('/api/estado');const s=await r.json();
const e=x=>String(x).replace(/[&<>"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}})[c]);
const row=(k,v)=>`<dt>${{k}}</dt><dd>${{v}}</dd>`;
document.getElementById('st').innerHTML=
row('Internet',s.internet?'<span class=ok>sí</span>':'<span class=bad>no</span>')+
row('Saliendo por',e(s.salida))+row('Modo',e(s.modo)+(s.configuracion?' · <b>configuración</b>':''))+
row('WiFi',e(s.wifi||'—'))+row('Laptop USB',s.laptop?e(s.laptop)+(s.laptop_comparte?' (comparte internet)':' (sin compartir)'):'no conectada')+
row('LTE',s.lte.sim?e(s.lte.estado)+' · '+e(s.lte.senal)+'% '+e(s.lte.tecnologia)+' '+e(s.lte.operador):'sin SIM')+
row('WireGuard',e(s.wireguard))+row('IPs','<pre>'+e(s.ips)+'</pre>');}}catch(_){{}}}}
f();setInterval(f,5000);
</script></body></html>"#,
        msg_html = if msg.is_empty() { String::new() } else { format!("<div class=msg>{msg}</div>") },
        r_auto = radio("auto", "Automático (recomendado)", "USB → WiFi → LTE → configuración, según lo que haya"),
        r_usb = radio("usb", "Solo USB", "Usa únicamente el internet que comparte la laptop"),
        r_casa = radio("casa", "Solo WiFi", "Cliente de tus redes guardadas; LTE apagado"),
        r_lte = radio("lte", "Solo LTE", "Datos de la SIM; WiFi cliente apagado"),
        saved = if saved.is_empty() { "<li>Ninguna</li>".into() } else { saved },
        options = options,
        accion = if cfg { "salir" } else { "entrar" },
        accion_txt = if cfg { "Salir de configuración" } else { "Entrar a configuración" },
    );
    Response::from_string(html_page).with_header(hdr("Content-Type", "text/html; charset=utf-8"))
}
