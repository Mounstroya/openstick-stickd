//! HTML/CSS/JS de la página del invitado.
//!
//! El JS está escrito con sintaxis ES5 (var, function) para que, en un navegador viejo o en el
//! mini-navegador del portal cautivo, al menos corra y muestre el aviso de "abre esto en tu
//! navegador" en vez de romperse. Todo dato ajeno (nombres de archivo) se pinta con
//! `textContent`. Sin animaciones salvo la barra de progreso.
//!
//! Protocolo por el DataChannel "f" (ordenado y confiable):
//!   emisor → {t:"lista",a:[{n,s,y}]}        nombres, tamaños y tipos
//!   receptor → {t:"acepto",i,o} | {t:"rechazo"}   i,o = desde qué archivo/byte (reanudar)
//!   emisor → {t:"ini",i,o}, trozos binarios (≤64 KB), {t:"fin",i} … {t:"todo"}
//!   receptor → {t:"listo"};  cualquiera → {t:"cancela"}
//! Si se corta, el receptor pide por señalización ("re") una conexión nueva, el emisor vuelve a
//! mandar la lista y el receptor responde con el archivo y byte donde se quedó.

use super::{IP_AP, Nav};
use crate::util::html;

const CSS: &str = r#"
:root{--bg:#f3f4f7;--card:#fff;--tx:#14161c;--mut:#5b6170;--ac:#1f5fd6;--on:#fff;--bd:#00000020;--ok:#18794e;--bad:#b42318;--warn:#fff4d6;--warnb:#d9a520}
@media (prefers-color-scheme:dark){:root{--bg:#0f1115;--card:#1a1d25;--tx:#e8eaf0;--mut:#9ba1ae;--ac:#6ea2ff;--on:#0b1530;--bd:#ffffff22;--ok:#4cc38a;--bad:#ff7b72;--warn:#2b2410;--warnb:#8a6d1c}}
*{box-sizing:border-box}html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--bg);color:var(--tx);font:16px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,system-ui,sans-serif}
main{max-width:520px;margin:0 auto;padding:12px 16px 40px}
header{max-width:520px;margin:0 auto;padding:14px 16px 0}h1{font-size:22px;margin:0}header p{margin:2px 0 0;color:var(--mut);font-size:14px}
.card{background:var(--card);border-radius:16px;padding:16px;margin:12px 0}
h2{font-size:18px;margin:0 0 10px}
.gran{display:block;width:100%;text-align:center;background:var(--ac);color:var(--on);border:0;border-radius:14px;padding:18px;font:inherit;font-size:19px;font-weight:650;cursor:pointer}
button{font:inherit;border:0;border-radius:12px;padding:12px 16px;font-weight:600;background:var(--ac);color:var(--on);cursor:pointer}
button.sec{background:transparent;color:var(--tx);border:1px solid var(--bd)}button:disabled{opacity:.5}
.fila{display:flex;gap:8px}.fila input{flex:1;min-width:0}
input[type=text]{width:100%;padding:12px;border-radius:12px;border:1px solid var(--bd);background:var(--bg);color:var(--tx);font:inherit;font-size:17px}
label.t{display:block;font-weight:600;margin-bottom:8px}
.mut{color:var(--mut);font-size:14px}
.codigo{font:700 26px/1.2 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;text-align:center;margin:6px 0 10px;word-break:break-word;user-select:all;-webkit-user-select:all}
.qr{display:block;width:min(220px,64vw);margin:0 auto 8px;background:#fff;border-radius:10px;padding:6px}
.estado{font-weight:600;margin:0 0 8px}.estado.ok{color:var(--ok)}.estado.bad{color:var(--bad)}
ul.lista{list-style:none;margin:8px 0;padding:0}
ul.lista li{display:flex;gap:10px;align-items:center;padding:9px 0;border-top:1px solid var(--bd)}
ul.lista .n{flex:1;min-width:0;overflow-wrap:anywhere}ul.lista .s{color:var(--mut);font-size:14px;white-space:nowrap}
ul.lista a,ul.lista button{padding:8px 12px;border-radius:10px;background:var(--ac);color:var(--on);text-decoration:none;font-weight:600;font-size:15px}
.total{font-size:14px;color:var(--mut);text-align:right}
.barra{height:14px;background:var(--bd);border-radius:7px;overflow:hidden;margin:6px 0}
.barra i{display:block;height:100%;width:0;background:var(--ac);transition:width .25s linear}
.ptxt{display:flex;justify-content:space-between;gap:8px;font-size:14px;color:var(--mut);font-variant-numeric:tabular-nums}
.botones{display:flex;gap:8px;flex-wrap:wrap;margin-top:12px}.botones button{flex:1}
.aviso{background:var(--warn);border:1px solid var(--warnb);border-radius:12px;padding:12px;margin:10px 0;font-size:15px}
.aviso p{margin:4px 0}.ip{display:block;font:700 24px ui-monospace,Menlo,monospace;margin:6px 0;user-select:all;-webkit-user-select:all}
#salir{position:fixed;inset:0;z-index:9;background:var(--bg);overflow:auto;padding:0 16px 24px}
#salir .flecha{display:block;width:120px;height:120px;margin:0 0 0 auto;color:var(--ac)}
#salir h2{font-size:22px;margin:0 0 8px}#salir ol{padding-left:22px;margin:8px 0}#salir li{margin:6px 0}
#salir .boton{display:block;text-align:center;background:var(--ac);color:var(--on);border-radius:14px;padding:16px;font-weight:650;text-decoration:none;margin:14px 0}
.link{background:none;border:0;color:var(--ac);text-decoration:underline;padding:6px 0;font-weight:400}
[hidden]{display:none!important}
"#;

const FLECHA: &str = r#"<svg class=flecha viewBox="0 0 120 120" aria-hidden="true"><path d="M18 108 C40 60 62 40 100 20" fill="none" stroke="currentColor" stroke-width="10" stroke-linecap="round"/><path d="M70 14 L106 16 L96 50" fill="none" stroke="currentColor" stroke-width="10" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

fn salir_html(nav: Nav) -> String {
    match nav {
        Nav::AppleCna => format!(
            r#"<div id=salir role=dialog aria-labelledby=salir-t>{FLECHA}
<h2 id=salir-t>Toca «Listo» o «Cancelar», aquí arriba</h2>
<p>Esta ventanita de inicio de sesión WiFi no puede pasar archivos.</p>
<ol><li>Toca el botón de la esquina de arriba (<b>Listo</b> o <b>Cancelar</b>). Si pregunta, elige <b>Usar sin Internet</b> para seguir en este WiFi.</li>
<li>Abre <b>Safari</b> y escribe:<span class=ip>{IP_AP}</span></li></ol>
<p class=mut>Si Safari no carga la página, revisa en Ajustes → Wi-Fi que sigues conectado a esta red.</p>
<button type=button class=link id=salir-seguir>Seguir aquí de todos modos</button></div>"#
        ),
        Nav::AndroidWv => format!(
            r#"<div id=salir role=dialog aria-labelledby=salir-t>{FLECHA}
<h2 id=salir-t>Ábrelo en Chrome</h2>
<p>Esta ventana de «Iniciar sesión en la red» no puede guardar los archivos que recibas.</p>
<a class=boton href="intent://{IP_AP}/#Intent;scheme=http;package=com.android.chrome;end">Abrir en el navegador</a>
<p class=mut>Si aparece «página no compatible», toca <b>Continuar en el navegador</b>.</p>
<p>Si no funciona:</p>
<ol><li>Toca <b>⋮</b> (arriba a la derecha) → <b>Usar esta red tal como está</b>. Si pregunta si quieres seguir sin Internet, di <b>Sí</b>.</li>
<li>Abre <b>Chrome</b> y escribe:<span class=ip>{IP_AP}</span></li></ol>
<p class=mut>Si Chrome no carga la página, apaga un momento los datos móviles: Android manda el tráfico por la SIM cuando el WiFi no tiene Internet.</p>
<button type=button class=link id=salir-seguir>Seguir aquí de todos modos</button></div>"#
        ),
        Nav::Normal => String::new(),
    }
}

pub(super) fn pagina(nav: Nav, codigo: &str) -> String {
    let nav_s = match nav {
        Nav::AppleCna => "cna",
        Nav::AndroidWv => "wv",
        Nav::Normal => "normal",
    };
    format!(
        r#"<!doctype html><html lang=es><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name=color-scheme content="light dark"><title>Compartir archivos</title><style>{CSS}</style></head>
<body data-nav="{nav_s}" data-codigo="{codigo}">
{salir}
<header><h1>Compartir archivos</h1><p>De un dispositivo a otro por este WiFi, directo y cifrado</p></header>
<main>
<noscript><div class=aviso>Necesitas activar JavaScript para pasar archivos.</div></noscript>
<div id=sinrtc class=aviso hidden><p><b>Este navegador no puede pasar archivos directo.</b></p>
<p>Abre esta misma dirección en Chrome, Safari, Firefox o Edge actualizados:</p><span class=ip>{IP_AP}</span></div>

<section id=p-inicio class=pant>
<div class=card><h2>Enviar</h2>
<label class=gran for=archivos>Elegir archivos para enviar</label>
<input type=file id=archivos multiple hidden>
<p class=mut>De cualquier tamaño. Puedes elegir varios.</p></div>
<div class=card><h2>Recibir</h2>
<form id=f-recibir><label class=t for=codigo>Escribe el código que te dicen</label>
<div class=fila><input type=text id=codigo autocomplete=off autocapitalize=none autocorrect=off spellcheck=false placeholder="7-luna-perro-nube" enterkeyhint=go><button>Recibir</button></div></form>
<p class=mut>O escanea con la cámara el QR que aparece en la pantalla de quien envía.</p>
<p id=i-error class="estado bad" hidden></p></div>
<p class=mut>Los archivos no pasan por el stick: viajan directo de un dispositivo al otro. Mantén las dos pantallas encendidas y esta página abierta hasta que termine.</p>
</section>

<section id=p-enviar class=pant hidden>
<div class=card id=e-cod><p class=mut style="margin:0;text-align:center">Código para recibir</p>
<p class=codigo id=e-codigo></p><img class=qr id=e-qr alt="Código QR para recibir">
<p class=mut>En el otro dispositivo: conéctate a este mismo WiFi y escanea el QR, o abre <b>{IP_AP}</b> y escribe el código.</p></div>
<div class=card><p class=estado id=e-estado>Preparando…</p>
<ul class=lista id=e-lista></ul><p class=total id=e-total></p>
<div id=e-prog hidden><div class=barra><i id=e-barra></i></div><div class=ptxt><span id=e-p1></span><span id=e-p2></span></div></div>
<div class=botones><button type=button class=sec id=e-cancelar>Cancelar</button><button type=button id=e-otra hidden>Enviar otros archivos</button></div></div>
</section>

<section id=p-recibir class=pant hidden>
<div class=card><p class=estado id=r-estado>Conectando…</p>
<div id=r-aviso class=aviso hidden></div>
<ul class=lista id=r-lista></ul><p class=total id=r-total></p>
<div id=r-prog hidden><div class=barra><i id=r-barra></i></div><div class=ptxt><span id=r-p1></span><span id=r-p2></span></div></div>
<div class=botones id=r-decidir hidden><button type=button class=sec id=r-rechazar>Rechazar</button><button type=button id=r-aceptar>Aceptar y recibir</button></div>
<div class=botones><button type=button class=sec id=r-cancelar>Cancelar</button><button type=button id=r-reintentar hidden>Reintentar</button><button type=button id=r-otra hidden>Listo</button></div></div>
</section>
</main>
<script>{JS}</script></body></html>"#,
        codigo = html(codigo),
        salir = salir_html(nav),
    )
}

const JS: &str = r#"
(function(){
var d=document,B=d.body;function $(i){return d.getElementById(i)}
var NAV=B.getAttribute('data-nav'),COD0=B.getAttribute('data-codigo')||'';
var RTC=window.RTCPeerConnection||window.webkitRTCPeerConnection;
var SOPORTA=!!(RTC&&RTC.prototype&&RTC.prototype.createDataChannel&&window.Blob&&window.FileReader&&window.ArrayBuffer&&window.Promise&&window.JSON&&window.URL&&URL.createObjectURL);
var TROZO=65536,BLOQUE=1048576,ALTO=4194304,BAJO=1048576,CONSOLIDAR=8388608;

if($('salir-seguir'))$('salir-seguir').onclick=function(){$('salir').hidden=true};
if(!SOPORTA){$('sinrtc').hidden=false;$('p-inicio').hidden=true;if(window.console)console.log('sin WebRTC');return}

function pant(id){var s=d.querySelectorAll('.pant');for(var i=0;i<s.length;i++)s[i].hidden=s[i].id!==id;window.scrollTo(0,0)}
function txt(id,t,cls){var e=$(id);e.textContent=t;if(cls!==undefined)e.className='estado'+(cls?' '+cls:'')}
function tam(b){var u=['B','KB','MB','GB','TB'],i=0;while(b>=1024&&i<4){b/=1024;i++}return (i?b.toFixed(b<10?1:0):b)+' '+u[i]}
function dur(s){s=Math.round(s);if(!isFinite(s)||s<0)return '';if(s<60)return s+' s';var m=Math.floor(s/60);if(m<60)return m+' min '+(s%60)+' s';return Math.floor(m/60)+' h '+(m%60)+' min'}
function ajax(met,url,cuerpo,cb){var x=new XMLHttpRequest();x.open(met,url,true);x.timeout=15000;
 x.onload=function(){var j=null;try{j=JSON.parse(x.responseText)}catch(e){}cb(j||{ok:0,e:'red',msg:'Respuesta inválida del stick ('+x.status+')'})};
 x.onerror=x.ontimeout=function(){cb({ok:0,e:'red',msg:'No hay conexión con el stick. ¿Sigues en su WiFi?'})};
 if(cuerpo!=null)x.setRequestHeader('Content-Type','text/plain;charset=UTF-8');x.send(cuerpo==null?null:cuerpo)}

// ---------- señalización (polling corto, una petición a la vez) ----------
var S={s:'',t:'',emisor:false,desde:0,g:0,vivo:false,timer:0,pidiendo:false,salida:[],mandando:false};
function q(){return 's='+S.s+'&t='+S.t}
function sondear(){clearTimeout(S.timer);if(!S.vivo||S.pidiendo)return;S.pidiendo=true;
 ajax('GET','/api/recibir?'+q()+'&desde='+S.desde,null,function(r){S.pidiendo=false;if(!S.vivo)return;
  if(r.ok){S.otro=r.o;for(var i=0;i<r.m.length;i++){var m=r.m[i];if(m[0]>S.desde)S.desde=m[0];try{alMensaje(m[1],m[2],m[3])}catch(e){if(window.console)console.log(e)}}}
  else if(r.e==='nosala'){S.vivo=false;salaTerminada(r.msg);return}
  programar()})}
function programar(){clearTimeout(S.timer);if(S.vivo)S.timer=setTimeout(sondear,(DC&&DC.readyState==='open')?2500:700)}
function senal(tipo,datos){S.salida.push([tipo,S.g,datos||'']);mandar()}
function mandar(){if(S.mandando||!S.salida.length)return;var x=S.salida.shift();S.mandando=true;
 ajax('POST','/api/enviar?'+q()+'&tipo='+x[0]+'&g='+x[1],x[2],function(r){S.mandando=false;
  if(!r.ok&&(r.e==='limite'||r.e==='lleno'||r.e==='red')){S.salida.unshift(x);setTimeout(mandar,1500);return}mandar()})}
function salirSala(){if(!S.s)return;var u='/api/salir?'+q();S.vivo=false;clearTimeout(S.timer);
 try{if(navigator.sendBeacon&&navigator.sendBeacon(u,''))return}catch(e){}ajax('POST',u,'',function(){})}

// ---------- WebRTC ----------
var PC=null,DC=null,CANDS=[],cortes=0,tCorte=0,tConectar=0;
function cerrarPC(){if(DC){DC.onclose=DC.onmessage=DC.onopen=null;try{DC.close()}catch(e){}}if(PC){PC.onicecandidate=PC.oniceconnectionstatechange=PC.ondatachannel=null;try{PC.close()}catch(e){}}PC=DC=null;CANDS=[]}
function nuevaPC(){cerrarPC();PC=new RTC({iceServers:[]});
 PC.onicecandidate=function(e){var c=e.candidate;if(c&&c.candidate)senal('ice',c.candidate+'\n'+(c.sdpMid||'')+'\n'+(c.sdpMLineIndex||0))};
 PC.oniceconnectionstatechange=function(){var s=PC&&PC.iceConnectionState;clearTimeout(tCorte);
  if(s==='failed')cortado();else if(s==='disconnected')tCorte=setTimeout(cortado,7000)};
 return PC}
function prepDC(dc){DC=dc;dc.binaryType='arraybuffer';try{dc.bufferedAmountLowThreshold=BAJO}catch(e){}
 dc.onopen=function(){clearTimeout(tConectar);cortes=0;programar();if(S.emisor)E.abierto();else R.abierto()};
 dc.onclose=function(){if(DC===dc)cortado()};
 dc.onmessage=function(e){if(typeof e.data==='string'){var m;try{m=JSON.parse(e.data)}catch(x){return}if(S.emisor)E.ctrl(m);else R.ctrl(m)}else if(!S.emisor)R.datos(e.data)};
 dc.onbufferedamountlow=function(){if(S.emisor)E.bucle()}}
function agregarCand(c){var p=c.split('\n');var o={candidate:p[0],sdpMid:p[1]||'0',sdpMLineIndex:+p[2]||0};
 PC.addIceCandidate(o).then(null,function(e){if(window.console)console.log('candidato',e)})}
function vaciarCands(){var c=CANDS;CANDS=[];for(var i=0;i<c.length;i++)agregarCand(c[i])}
function maxTrozo(){var m=PC&&PC.sctp&&PC.sctp.maxMessageSize;return (m&&m>=16384)?Math.min(TROZO,m):(m?16384:TROZO)}
function mandarCtrl(o){if(DC&&DC.readyState==='open')DC.send(JSON.stringify(o))}

function alMensaje(tipo,g,datos){
 if(S.emisor){
  if(tipo==='hola'||tipo==='re'){conectarEmisor();return}
  if(tipo==='bye'){cerrarPC();E.otroSalio();return}
  if(g!==S.g||!PC)return;
  if(tipo==='answer')PC.setRemoteDescription({type:'answer',sdp:datos}).then(vaciarCands,falloRTC);
  else if(tipo==='ice'){if(PC.remoteDescription&&PC.remoteDescription.type)agregarCand(datos);else CANDS.push(datos)}
 }else{
  if(tipo==='offer'&&g>=S.g){S.g=g;var pc=nuevaPC();pc.ondatachannel=function(e){prepDC(e.channel)};
   pc.setRemoteDescription({type:'offer',sdp:datos}).then(function(){vaciarCands();return pc.createAnswer()}).then(function(a){return pc.setLocalDescription(a)})
    .then(function(){senal('answer',pc.localDescription.sdp)},falloRTC);esperarConexion()}
  else if(tipo==='ice'&&g===S.g){if(PC&&PC.remoteDescription&&PC.remoteDescription.type)agregarCand(datos);else CANDS.push(datos)}
  else if(tipo==='bye'){R.fin('Quien enviaba cerró la página.','bad')}
 }}
function conectarEmisor(){S.g++;E.gen++;E.enviando=false;E.leyendo=false;E.bloque=null;var pc=nuevaPC();prepDC(pc.createDataChannel('f',{ordered:true}));
 pc.createOffer().then(function(o){return pc.setLocalDescription(o)}).then(function(){senal('offer',pc.localDescription.sdp)},falloRTC);
 E.estado('Conectando con quien recibe…','');programar()}
function falloRTC(e){if(window.console)console.log('webrtc',e)}
function esperarConexion(){clearTimeout(tConectar);tConectar=setTimeout(function(){if(!(DC&&DC.readyState==='open'))R.noConecta()},20000)}
function cortado(){clearTimeout(tCorte);if(S.emisor)E.cortado();else R.cortado()}

// ---------- emisor ----------
var E={a:[],i:0,o:0,gen:0,bloque:null,bpos:0,leyendo:false,ini:false,enviando:false,hecho:false,base:0,total:0,muestras:[]};
E.estado=function(t,c){txt('e-estado',t,c)};
E.iniciar=function(files){if(!files||!files.length)return;E.a=[];E.total=0;for(var i=0;i<files.length;i++){E.a.push(files[i]);E.total+=files[i].size}
 var ul=$('e-lista');ul.textContent='';for(i=0;i<E.a.length;i++){var li=d.createElement('li'),n=d.createElement('span'),s=d.createElement('span');
  n.className='n';n.textContent=E.a[i].name;s.className='s';s.textContent=tam(E.a[i].size);li.appendChild(n);li.appendChild(s);ul.appendChild(li)}
 $('e-total').textContent=E.a.length>1?E.a.length+' archivos · '+tam(E.total):'';
 pant('p-enviar');E.estado('Creando el código…','');
 ajax('POST','/api/crear','',function(r){if(!r.ok){E.estado(r.msg,'bad');return}
  S.s=r.s;S.t=r.t;S.emisor=true;S.vivo=true;$('e-codigo').textContent=r.c;$('e-qr').src='/qr.svg?c='+encodeURIComponent(r.c);
  E.estado('Esperando a quien recibe…','');sondear()})};
E.abierto=function(){E.estado('Conectado. Esperando que acepte…','');$('e-cod').hidden=true;
 var l=[];for(var i=0;i<E.a.length;i++)l.push({n:E.a[i].name,s:E.a[i].size,y:E.a[i].type||''});mandarCtrl({t:'lista',a:l})};
E.ctrl=function(m){
 if(m.t==='acepto'){var i=+m.i||0,o=+m.o||0;if(i<0||i>E.a.length)i=0;if(i<E.a.length&&(o<0||o>E.a[i].size))o=0;
  E.gen++;E.i=i;E.o=o;E.bloque=null;E.leyendo=false;E.ini=false;E.enviando=true;E.base=o;for(var k=0;k<i;k++)E.base+=E.a[k].size;
  E.enviados=E.base;E.muestras=[];$('e-prog').hidden=false;E.estado(E.base?'Reanudando…':'Enviando…','');E.bucle();E.tick()}
 else if(m.t==='rechazo'){E.enviando=false;E.estado('Quien recibe no aceptó los archivos.','bad');E.terminar()}
 else if(m.t==='listo'){E.enviando=false;E.hecho=true;E.pintar(E.total);E.estado('Listo: '+(E.a.length>1?E.a.length+' archivos enviados':'archivo enviado')+'.','ok');E.terminar()}
 else if(m.t==='cancela'){E.enviando=false;E.estado('Quien recibe canceló.','bad');E.terminar()}};
E.bucle=function(){var dc=DC,tr=maxTrozo();
 while(E.enviando&&dc&&dc.readyState==='open'){
  if(dc.bufferedAmount>ALTO)return;
  if(E.bloque&&E.bpos<E.bloque.byteLength){var n=Math.min(tr,E.bloque.byteLength-E.bpos);dc.send(E.bloque.slice(E.bpos,E.bpos+n));E.bpos+=n;E.o+=n;E.enviados+=n;continue}
  E.bloque=null;
  if(E.i>=E.a.length){E.enviando=false;mandarCtrl({t:'todo'});E.estado('Terminando…','');return}
  var f=E.a[E.i];
  if(!E.ini){mandarCtrl({t:'ini',i:E.i,o:E.o});E.ini=true}
  if(E.o>=f.size){mandarCtrl({t:'fin',i:E.i});E.i++;E.o=0;E.ini=false;continue}
  if(E.leyendo)return;
  E.leer(f);return}};
E.leer=function(f){E.leyendo=true;var gen=E.gen,fr=new FileReader();
 fr.onload=function(){if(gen!==E.gen)return;E.leyendo=false;E.bloque=fr.result;E.bpos=0;E.bucle()};
 fr.onerror=function(){if(gen!==E.gen)return;E.enviando=false;E.estado('No se pudo leer «'+f.name+'». ¿Lo borraste o moviste?','bad');mandarCtrl({t:'cancela'});E.terminar()};
 fr.readAsArrayBuffer(f.slice(E.o,Math.min(f.size,E.o+BLOQUE)))};
E.pintar=function(b){var p=E.total?b/E.total:1;$('e-barra').style.width=(p*100).toFixed(1)+'%';$('e-p1').textContent=tam(b)+' de '+tam(E.total)};
E.tick=function(){if(!E.enviando)return;var b=E.enviados-(DC?DC.bufferedAmount:0);if(b<E.base)b=E.base;E.pintar(b);
 var v=vel(E.muestras,b);$('e-p2').textContent=v?tam(v)+'/s · faltan '+dur((E.total-b)/v):'';
 if(E.i<E.a.length)E.estado('Enviando '+(E.a.length>1?(E.i+1)+' de '+E.a.length+': ':'')+E.a[E.i].name,'');setTimeout(E.tick,500)};
E.cortado=function(){cerrarPC();if(E.hecho||!S.vivo)return;E.gen++;E.leyendo=false;
 E.estado(E.enviando?'Se cortó la conexión. Esperando a que vuelva quien recibe…':'Se perdió la conexión. Esperando a quien recibe…','bad');E.enviando=false};
E.otroSalio=function(){E.enviando=false;if(E.hecho)return;$('e-cod').hidden=false;E.estado('Quien recibía se fue. Esperando a otra persona con el mismo código…','')};
E.terminar=function(){cerrarPC();salirSala();$('e-cancelar').hidden=true;$('e-otra').hidden=false};
$('e-cancelar').onclick=function(){mandarCtrl({t:'cancela'});E.enviando=false;E.estado('Cancelado.','bad');E.terminar()};
$('e-otra').onclick=function(){location.href='/'};

function vel(arr,b){var t=Date.now();arr.push([t,b]);while(arr.length>2&&t-arr[0][0]>4000)arr.shift();var a=arr[0];return t-a[0]>800?(b-a[1])*1000/(t-a[0]):0}

// ---------- receptor ----------
var R={lista:null,firma:'',aceptado:false,i:-1,rec:0,partes:[],tp:0,blobs:[],hechos:[],urls:[],total:0,base:0,listo:false,muestras:[],intentos:0};
R.estado=function(t,c){txt('r-estado',t,c)};
R.unirse=function(c){pant('p-recibir');R.estado('Buscando el código…','');
 ajax('POST','/api/unirse?c='+encodeURIComponent(c),'',function(r){
  if(!r.ok){pant('p-inicio');$('codigo').value=c;$('i-error').textContent=r.msg;$('i-error').hidden=false;return}
  S.s=r.s;S.t=r.t;S.emisor=false;S.vivo=true;R.estado('Conectando directo con quien envía…','');esperarConexion();sondear()})};
R.abierto=function(){$('r-reintentar').hidden=true;$('r-aviso').hidden=true;R.estado(R.aceptado?'Reconectado.':'Conectado.','')};
R.ctrl=function(m){
 if(m.t==='lista'&&m.a&&m.a.length){var f='';for(var i=0;i<m.a.length;i++)f+=m.a[i].n+'\u0000'+m.a[i].s+'\u0001';
  if(R.aceptado&&f===R.firma){var k=R.i;if(k<0){k=0;while(R.hechos[k])k++;}mandarCtrl({t:'acepto',i:k,o:R.i<0?0:R.rec});R.estado('Reanudando…','');return}
  R.lista=m.a;R.firma=f;R.aceptado=false;R.i=-1;R.hechos=[];R.total=0;
  var ul=$('r-lista');ul.textContent='';for(i=0;i<m.a.length;i++){var li=d.createElement('li'),n=d.createElement('span'),s=d.createElement('span');
   n.className='n';n.textContent=String(m.a[i].n);s.className='s';s.textContent=tam(+m.a[i].s||0);li.appendChild(n);li.appendChild(s);ul.appendChild(li);R.total+=+m.a[i].s||0}
  $('r-total').textContent=m.a.length>1?m.a.length+' archivos · '+tam(R.total):'';
  R.estado('Te quieren pasar '+(m.a.length>1?'estos archivos':'este archivo')+':','');$('r-decidir').hidden=false;R.avisoRam()}
 else if(m.t==='ini'){var o=+m.o||0;if(m.i!==R.i||o!==R.rec){R.consolidar();if(m.i!==R.i||o===0){R.blobs=[];R.rec=0}R.i=m.i}}
 else if(m.t==='fin'){R.terminarArchivo(m.i)}
 else if(m.t==='todo'){R.listo=true;$('r-prog').hidden=true;R.estado('Listo: recibiste '+(R.lista.length>1?R.lista.length+' archivos':'el archivo')+'. Guárdalos:','ok');
  ajax('POST','/api/reporte?'+q()+'&ev=fin&b='+R.total,'',function(){mandarCtrl({t:'listo'});setTimeout(function(){salirSala();cerrarPC()},1500)});
  $('r-cancelar').hidden=true;$('r-otra').hidden=false;if(R.lista.length>1)R.botonTodos()}
 else if(m.t==='cancela'){R.fin('Quien enviaba canceló.','bad')}};
R.datos=function(b){if(R.i<0)return;R.partes.push(b);R.tp+=b.byteLength;R.rec+=b.byteLength;if(R.tp>=CONSOLIDAR)R.consolidar()};
// juntar los trozos en un Blob cada 8 MB: Chrome manda los Blobs grandes a disco, así no se llena la RAM
R.consolidar=function(){if(R.partes.length){R.blobs.push(new Blob(R.partes));R.partes=[];R.tp=0}};
R.recibido=function(){var b=R.rec;for(var k=0;k<R.hechos.length;k++)if(R.hechos[k])b+=R.hechos[k].size;return b};
R.terminarArchivo=function(i){R.consolidar();var a=R.lista[i];if(!a)return;var bl=new Blob(R.blobs,{type:a.y||'application/octet-stream'});R.blobs=[];R.rec=0;R.i=-1;
 if(bl.size!==+a.s){R.estado('«'+a.n+'» llegó incompleto. Vuelve a intentarlo.','bad');return}
 R.hechos[i]=bl;var url=URL.createObjectURL(bl);R.urls[i]=url;var li=$('r-lista').children[i];if(!li)return;
 var g=d.createElement('a');g.href=url;g.setAttribute('download',String(a.n).replace(/[\/\\]/g,'_')||'archivo');g.textContent='Guardar';g.className='guardar';li.appendChild(g)};
R.botonTodos=function(){var b=d.createElement('button');b.type='button';b.textContent='Guardar todos';b.onclick=function(){var l=d.querySelectorAll('a.guardar'),k=0;
 (function sig(){if(k<l.length){l[k++].click();setTimeout(sig,700)}})()};$('r-otra').parentNode.insertBefore(b,$('r-otra'))};
R.tick=function(){if(R.listo||!R.aceptado)return;var b=R.recibido();$('r-barra').style.width=(R.total?b/R.total*100:100).toFixed(1)+'%';
 $('r-p1').textContent=tam(b)+' de '+tam(R.total);var v=vel(R.muestras,b);$('r-p2').textContent=v?tam(v)+'/s · faltan '+dur((R.total-b)/v):'';
 if(R.i>=0&&R.lista[R.i])R.estado('Recibiendo '+(R.lista.length>1?(R.i+1)+' de '+R.lista.length+': ':'')+R.lista[R.i].n,'');setTimeout(R.tick,500)};
R.avisoRam=function(){var ios=/iPhone|iPad|iPod/.test(navigator.userAgent)||(navigator.platform==='MacIntel'&&navigator.maxTouchPoints>1);
 var lim=ios?1073741824:(/Firefox/.test(navigator.userAgent)?2147483648:0);var big=0;for(var i=0;i<R.lista.length;i++)if(+R.lista[i].s>big)big=+R.lista[i].s;
 if(lim&&big>lim){var a=$('r-aviso');a.textContent='Aviso: este navegador junta el archivo en la memoria del teléfono hasta terminar. Con archivos de más de '+tam(lim)+' puede quedarse sin memoria y fallar. Si puedes, recibe en una PC o en Chrome.';a.hidden=false}};
R.cortado=function(){cerrarPC();if(R.listo||!S.vivo)return;
 if(!R.aceptado&&!R.lista){R.noConecta();return}
 R.estado('Se cortó la conexión. Reintentando…','bad');R.consolidar();cortes++;
 if(cortes<=5)setTimeout(function(){if(S.vivo&&!(DC&&DC.readyState==='open')){senal('re');esperarConexion()}},1500*cortes);else{R.estado('No se pudo reconectar.','bad');$('r-reintentar').hidden=false}};
R.noConecta=function(){if(DC&&DC.readyState==='open')return;R.estado('No se pudo conectar directo con el otro dispositivo.','bad');var a=$('r-aviso');a.textContent='';
 var p=['Revisa que los dos sigan conectados a este WiFi.','Si un teléfono tiene datos móviles encendidos, apágalos un momento: algunos mandan el tráfico por la SIM cuando el WiFi no tiene Internet.','Usa Chrome, Safari, Firefox o Edge (no la ventanita de inicio de sesión del WiFi).'];
 for(var i=0;i<p.length;i++){var e=d.createElement('p');e.textContent=p[i];a.appendChild(e)}a.hidden=false;$('r-reintentar').hidden=false};
R.fin=function(t,c){R.estado(t,c);cerrarPC();salirSala();$('r-decidir').hidden=true;$('r-prog').hidden=true;$('r-cancelar').hidden=true;$('r-otra').hidden=false};
$('r-aceptar').onclick=function(){R.aceptado=true;$('r-decidir').hidden=true;$('r-prog').hidden=false;R.muestras=[];mandarCtrl({t:'acepto',i:0,o:0});
 ajax('POST','/api/reporte?'+q()+'&ev=inicio','',function(){});R.estado('Recibiendo…','');R.tick()};
$('r-rechazar').onclick=function(){mandarCtrl({t:'rechazo'});R.fin('Rechazaste los archivos.','')};
$('r-cancelar').onclick=function(){mandarCtrl({t:'cancela'});R.fin('Cancelado.','bad')};
$('r-reintentar').onclick=function(){$('r-reintentar').hidden=true;$('r-aviso').hidden=true;cortes=0;R.estado('Reintentando…','');senal('re');esperarConexion()};
$('r-otra').onclick=function(){location.href='/'};

function salaTerminada(msg){cerrarPC();if(S.emisor){if(!E.hecho){E.estado(msg||'La sesión terminó.','bad');$('e-cancelar').hidden=true;$('e-otra').hidden=false}}
 else if(!R.listo){R.fin(msg||'La sesión terminó.','bad')}}

// ---------- inicio ----------
$('archivos').onchange=function(){E.iniciar(this.files)};
$('f-recibir').onsubmit=function(e){e.preventDefault();var c=$('codigo').value.replace(/^\s+|\s+$/g,'');$('i-error').hidden=true;if(c)R.unirse(c);return false};
window.addEventListener('pagehide',function(){if(S.vivo&&(S.emisor||!R.listo)){mandarCtrl({t:'cancela'});salirSala()}});
if(COD0&&!(NAV!=='normal'&&$('salir')&&!$('salir').hidden))R.unirse(COD0);
})();
"#;
