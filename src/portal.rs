//! Panel de administración en el puerto 80. Accesible desde el cable USB (192.168.5.x), el túnel
//! WireGuard (10.x), localhost y el hotspot de CONFIGURACIÓN (192.168.4.x, con contraseña).
//! Cuando hay una VITRINA al aire, 192.168.4.x es un WiFi abierto de desconocidos: ahí el panel
//! se bloquea (además el firewall de captive.rs redirige su puerto 80 al servidor de invitados).

use crate::captive::{self, Activa};
use crate::util::{form, html, json};
use crate::{net, save_mode, save_powerbank, save_ssid, ssid_for, Mode, Request, State, Vitrinas};
use std::io::Read;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

const MAX_UPLOAD: u64 = 25 * 1024 * 1024;

fn allowed(ip: Option<IpAddr>, vitrina_al_aire: bool) -> bool {
    match ip {
        Some(IpAddr::V4(v)) => {
            let o = v.octets();
            let ap = o[0] == 192 && o[1] == 168 && o[2] == 4;
            v.is_loopback() || (o[0] == 192 && o[1] == 168 && o[2] == 5) || o[0] == 10 || (ap && !vitrina_al_aire)
        }
        Some(IpAddr::V6(v)) => v.is_loopback(),
        None => false,
    }
}

pub fn spawn(state: Arc<Mutex<State>>, vit: Vitrinas, activa: Activa) {
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
            if !allowed(ip, activa.lock().unwrap().is_some()) {
                let _ = req.respond(Response::from_string("prohibido").with_status_code(403));
                continue;
            }
            let ct = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let url = req.url().to_string();
            let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
            let (path, query) = (path.to_string(), query.to_string());
            let is_post = *req.method() == Method::Post;
            if is_post && !same_origin(&req) {
                // anti-CSRF: una página web cualquiera no puede mandar formularios al panel
                let _ = req.respond(Response::from_string("origen no permitido").with_status_code(403));
                continue;
            }
            let mut body = Vec::new();
            if is_post {
                let lim = if path.starts_with("/vitrina/") { MAX_UPLOAD } else { 8192 };
                let _ = req.as_reader().take(lim).read_to_end(&mut body);
            }

            let resp = if path == "/api/estado" {
                Response::from_string(status_json(&state, &vit)).with_header(hdr("Content-Type", "application/json; charset=utf-8"))
            } else if let Some(rest) = path.strip_prefix("/vitrina/") {
                let (id, sub) = rest.split_once('/').unwrap_or((rest, ""));
                match vit.iter().find(|v| v.id() == id) {
                    None => Response::from_string("no existe").with_status_code(404),
                    Some(v) if is_post => {
                        let f = if ct.starts_with("application/x-www-form-urlencoded") {
                            form(&String::from_utf8_lossy(&body))
                        } else {
                            form(&query)
                        };
                        let msg = v.admin_post(sub, &f, &body);
                        if ct.starts_with("application/x-www-form-urlencoded") {
                            page(&state, &vit, &msg)
                        } else {
                            // subida por fetch(): responde texto plano para el JS del módulo
                            Response::from_string(msg).with_header(hdr("Content-Type", "text/plain; charset=utf-8"))
                        }
                    }
                    Some(v) => match v.admin_get(sub) {
                        Some(r) => captive::to_tiny(r),
                        None => Response::from_string("no encontrado").with_status_code(404),
                    },
                }
            } else if is_post {
                let msg = handle_post(&state, &vit, &path, &form(&String::from_utf8_lossy(&body)));
                page(&state, &vit, &msg)
            } else {
                page(&state, &vit, "")
            };
            let _ = req.respond(resp);
        }
    });
}

/// El POST debe venir del propio panel: Origin (o Referer) con el mismo host que la petición.
/// Sin ninguno de los dos (curl, scripts locales) se acepta: los navegadores siempre mandan Origin en POST.
fn same_origin(req: &tiny_http::Request) -> bool {
    let get = |n: &'static str| req.headers().iter().find(|h| h.field.equiv(n)).map(|h| h.value.as_str().to_string());
    let host = get("Host").unwrap_or_default();
    let src = get("Origin").filter(|o| o != "null").or_else(|| get("Referer"));
    match src {
        None => get("Origin").is_none(), // Origin: null (sandbox, data:) se rechaza
        Some(o) => {
            let rest = o.split_once("://").map(|(_, r)| r).unwrap_or("");
            let o_host = rest.split('/').next().unwrap_or("");
            !host.is_empty() && o_host.eq_ignore_ascii_case(&host)
        }
    }
}

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

fn field<'a>(f: &'a [(String, String)], k: &str) -> &'a str {
    f.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str()).unwrap_or("")
}

fn handle_post(state: &Arc<Mutex<State>>, vit: &Vitrinas, path: &str, f: &[(String, String)]) -> String {
    match path {
        "/modo" => match Mode::parse(field(f, "modo"), vit) {
            Some(m) => {
                save_mode(m);
                let mut s = state.lock().unwrap();
                s.mode = m;
                s.request = Some(Request::ApplyMode);
                format!("Modo cambiado a «{}».", html(&mode_label(m, vit)))
            }
            None => "Modo no válido.".into(),
        },
        "/powerbank" => {
            let v = field(f, "valor");
            let id = vit.iter().find(|x| x.id() == v).map(|x| x.id());
            save_powerbank(id);
            let mut s = state.lock().unwrap();
            s.powerbank = id;
            s.request = Some(Request::ApplyMode);
            match id {
                Some(i) => format!("En powerbank arrancará: {}.", html(vit.iter().find(|x| x.id() == i).map(|x| x.nombre()).unwrap_or(i))),
                None => "En powerbank se queda en su modo normal.".into(),
            }
        }
        "/ssid" => {
            let id = field(f, "id");
            let ssid = field(f, "ssid").trim();
            if ssid.is_empty() || ssid.len() > 32 {
                return "El nombre de la red debe tener de 1 a 32 caracteres.".into();
            }
            match vit.iter().find(|x| x.id() == id) {
                Some(v) => {
                    save_ssid(v.id(), ssid);
                    let mut s = state.lock().unwrap();
                    if s.effective == Mode::Vitrina(v.id()) {
                        s.request = Some(Request::ApplyMode);
                    }
                    format!("La red de «{}» ahora se llama «{}».", html(v.nombre()), html(ssid))
                }
                None => "Vitrina desconocida.".into(),
            }
        }
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
                    "Entrando a modo configuración: aparecerá la red «Openstick».".into()
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

fn mode_label(m: Mode, vit: &Vitrinas) -> String {
    match m {
        Mode::Vitrina(id) => format!("Vitrina: {}", vit.iter().find(|v| v.id() == id).map(|v| v.nombre()).unwrap_or(id)),
        m => m.key(),
    }
}

fn status_json(state: &Arc<Mutex<State>>, vit: &Vitrinas) -> String {
    let (mode, eff, cfg, internet, uplink, laptop, sharing, host, pb) = {
        let s = state.lock().unwrap();
        (s.mode, s.effective, s.config_mode, s.internet, s.uplink.clone(), s.laptop.clone(), s.usb_sharing, s.host_present, s.powerbank)
    };
    let l = net::lte();
    let vit_res: Vec<String> = vit.iter().map(|v| format!("{}:{}", json(v.id()), json(&v.resumen()))).collect();
    format!(
        "{{\"modo\":{},\"efectivo\":{},\"configuracion\":{},\"internet\":{},\"salida\":{},\"pc_conectada\":{},\"powerbank\":{},\"laptop\":{},\"laptop_comparte\":{},\"wifi\":{},\"lte\":{{\"estado\":{},\"senal\":{},\"operador\":{},\"tecnologia\":{},\"sim\":{}}},\"wireguard\":{},\"ips\":{},\"vitrinas\":{{{}}}}}",
        json(&mode.key()),
        json(&mode_label(eff, vit)),
        cfg,
        internet,
        json(&salida_humana(&uplink, sharing)),
        host,
        json(pb.unwrap_or("")),
        json(laptop.as_deref().unwrap_or("")),
        sharing,
        json(&net::current_ssid()),
        json(&l.state),
        json(&l.signal),
        json(&l.operator),
        json(&l.tech),
        l.sim,
        json(&net::wireguard()),
        json(&net::addrs()),
        vit_res.join(",")
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

fn page(state: &Arc<Mutex<State>>, vit: &Vitrinas, msg: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let (mode, cfg, scan, pb) = {
        let s = state.lock().unwrap();
        (s.mode, s.config_mode, s.scan.clone(), s.powerbank)
    };
    let radio = |key: &str, label: &str, desc: &str| {
        format!(
            "<label class=opt><input type=radio name=modo value=\"{}\" {}><span><b>{}</b><small>{}</small></span></label>",
            html(key),
            if mode.key() == key { "checked" } else { "" },
            html(label),
            html(desc)
        )
    };
    let mut radios = String::new();
    radios += &radio("auto", "Automático (recomendado)", "USB → WiFi → LTE → configuración, según lo que haya");
    radios += &radio("usb", "Solo USB", "Usa únicamente el internet que comparte la laptop");
    radios += &radio("casa", "Solo WiFi", "Cliente de tus redes guardadas; LTE apagado");
    radios += &radio("lte", "Solo LTE", "Datos de la SIM; WiFi cliente apagado");
    let mut pb_opts = format!("<option value=\"\" {}>Nada (quedarse en su modo)</option>", if pb.is_none() { "selected" } else { "" });
    let mut vit_cards = String::new();
    for v in vit.iter() {
        radios += &radio(&format!("vitrina:{}", v.id()), &format!("Vitrina: {}", v.nombre()), v.descripcion());
        pb_opts += &format!(
            "<option value=\"{}\" {}>{}</option>",
            html(v.id()),
            if pb == Some(v.id()) { "selected" } else { "" },
            html(v.nombre())
        );
        let ssid_form = if v.usa_ap() {
            format!(
                "<form method=post action=/ssid class=row><input type=hidden name=id value=\"{}\"><label style=flex:1>Nombre de la red WiFi<input type=text name=ssid maxlength=32 value=\"{}\"></label><button>Guardar</button></form>",
                html(v.id()),
                html(&ssid_for(v.as_ref()))
            )
        } else {
            "<p class=mut>Este modo no crea WiFi: usa el radio para escuchar.</p>".into()
        };
        vit_cards += &format!(
            "<section class=card id=\"v-{id}\"><details><summary><h2>{nombre}</h2><span class=mut>{res}</span></summary>{ssid_form}<div class=mod>{admin}</div></details></section>",
            id = html(v.id()),
            nombre = html(v.nombre()),
            res = html(&v.resumen()),
            admin = v.admin_html(),
        );
    }
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
:root{{--bg:#0f1115;--card:#181b22;--tx:#e8eaf0;--mut:#8a90a0;--ac:#4f8cff;--ok:#3ecf8e;--bad:#ff5d5d;--bd:#ffffff1a}}
@media (prefers-color-scheme:light){{:root{{--bg:#f4f5f8;--card:#fff;--tx:#14161c;--mut:#5d6372;--bd:#0000001a}}}}
*{{box-sizing:border-box}}body{{margin:0;font:16px/1.45 system-ui,sans-serif;background:var(--bg);color:var(--tx)}}
main{{max-width:620px;margin:auto;padding:16px}}h1{{font-size:22px;margin:8px 0 16px}}h2{{font-size:16px;margin:0}}
.card{{background:var(--card);border-radius:14px;padding:16px;margin-bottom:14px}}.card>h2{{margin-bottom:10px}}
.msg{{background:var(--ac);color:#fff;border-radius:10px;padding:10px 12px;margin-bottom:14px}}
dl{{display:grid;grid-template-columns:auto 1fr;gap:4px 12px;margin:0}}dt{{color:var(--mut)}}dd{{margin:0;word-break:break-word}}
.ok{{color:var(--ok)}}.bad{{color:var(--bad)}}.mut{{color:var(--mut)}}pre{{margin:0;font-size:12px;white-space:pre-wrap;color:var(--mut)}}
.opt{{display:flex;gap:10px;padding:8px 0;align-items:flex-start}}.opt small{{display:block;color:var(--mut)}}
input[type=text],input[type=password],input[type=email],input[type=url],input[type=tel],input[type=number],select,textarea{{width:100%;padding:10px;border-radius:8px;border:1px solid var(--bd);background:var(--bg);color:var(--tx);margin:4px 0 10px;font-size:16px}}
button{{background:var(--ac);color:#fff;border:0;border-radius:8px;padding:10px 16px;font-size:15px}}
button.link{{background:none;color:var(--bad);padding:0}}ul{{list-style:none;padding:0;margin:0 0 12px}}
li{{display:flex;justify-content:space-between;padding:6px 0;border-bottom:1px solid var(--bd)}}
summary{{cursor:pointer;display:flex;justify-content:space-between;align-items:center;gap:10px;list-style:none}}summary::-webkit-details-marker{{display:none}}
details[open] summary{{margin-bottom:12px}}.row{{display:flex;gap:10px;align-items:flex-end}}.row button{{margin-bottom:10px}}
.mod{{border-top:1px solid var(--bd);padding-top:12px}}h3{{font-size:15px;margin:14px 0 6px}}
</style></head><body><main>
<h1>OpenStick · UZ801</h1>
{msg_html}
<section class=card><h2>Estado</h2><dl id=st><dt>Cargando…</dt><dd></dd></dl></section>
<section class=card><h2>Modo de trabajo</h2><form method=post action=/modo>{radios}<button>Guardar modo</button></form></section>
<section class=card><h2>Al conectarlo a una powerbank</h2>
<p class=mut style="margin-top:0">Si no hay una computadora en el USB por 10 s, el stick arranca esta vitrina sola. Al volver a conectarlo a la PC regresa a su modo.</p>
<form method=post action=/powerbank class=row><select name=valor style=flex:1>{pb_opts}</select><button>Guardar</button></form></section>
<h2 style="margin:20px 0 10px">Vitrinas</h2>
{vit_cards}
<section class=card><h2>Redes WiFi (para internet)</h2><ul>{saved}</ul>
<form method=post action=/wifi>
<label>Red detectada<select name=ssid><option value="">— elegir —</option>{options}</select></label>
<label>…o escribe el nombre<input type=text name=ssid_manual maxlength=32 autocomplete=off></label>
<label>Contraseña<input type=password name=psk maxlength=63></label>
<button>Guardar y conectar</button></form></section>
<section class=card><h2>Modo configuración</h2>
<p class=mut style="margin-top:0">Pulsación corta del botón (agujerito) = entrar/salir. 8 s = reiniciar. También entra solo tras 2 min sin internet.</p>
<form method=post action=/config><input type=hidden name=accion value={accion}><button>{accion_txt}</button></form></section>
</main><script>
async function f(){{try{{const r=await fetch('/api/estado');const s=await r.json();
const e=x=>String(x).replace(/[&<>"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}})[c]);
const row=(k,v)=>`<dt>${{k}}</dt><dd>${{v}}</dd>`;
document.getElementById('st').innerHTML=
row('Corriendo',e(s.efectivo)+(s.configuracion?' · <b>configuración</b>':''))+
row('Internet',s.internet?'<span class=ok>sí</span>':'<span class=bad>no</span>')+
row('Saliendo por',e(s.salida))+
row('USB',s.pc_conectada?(s.laptop?'PC '+e(s.laptop)+(s.laptop_comparte?' (comparte internet)':''):'PC conectada'):'powerbank / cargador')+
row('WiFi',e(s.wifi||'—'))+
row('LTE',s.lte.sim?e(s.lte.estado)+' · '+e(s.lte.senal)+'% '+e(s.lte.tecnologia)+' '+e(s.lte.operador):'sin SIM')+
row('WireGuard',e(s.wireguard))+row('IPs','<pre>'+e(s.ips)+'</pre>');}}catch(_){{}}}}
f();setInterval(f,5000);
</script></body></html>"#,
        msg_html = if msg.is_empty() { String::new() } else { format!("<div class=msg>{msg}</div>") },
        saved = if saved.is_empty() { "<li>Ninguna</li>".into() } else { saved },
        accion = if cfg { "salir" } else { "entrar" },
        accion_txt = if cfg { "Salir de configuración" } else { "Entrar a configuración" },
    );
    Response::from_string(html_page).with_header(hdr("Content-Type", "text/html; charset=utf-8"))
}
