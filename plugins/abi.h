/* ABI C de stickd (v2) — plugins .so para el OpenStick UZ801.
 *
 * Un plugin es un shared object que exporta TODOS los símbolos obligatorios de
 * abajo. El loader (src/plugin.rs) valida la versión antes de usar nada.
 *
 * Un plugin se ve en el panel como una "vitrina" más: cuando está al aire, el
 * stick levanta un WiFi abierto con portal cautivo y te manda cada petición de
 * los invitados a stickd_guest(). Tu sección del panel la pintas con
 * stickd_admin_html() y recibes sus formularios en stickd_admin_post().
 *
 * === Lo que el stick te deja hacer (API del host) ===
 * stickd_init() recibe un puntero a `stickd_host`: una tabla de funciones del
 * stick que puedes llamar (ver más abajo). Guárdala. Así no tienes que ejecutar
 * procesos por tu cuenta: pídele al stick escanear WiFi, ver el uplink, bajar
 * una URL, mandar SMS, encender LEDs, etc. Las funciones que devuelven char*
 * se liberan con host->free_string().
 *
 * === Reglas ===
 * - Los strings y respuestas que TÚ devuelves se liberan con stickd_free_string
 *   / stickd_free_respuesta (el loader siempre llama a la liberación correcta).
 * - stickd_guest() se llama desde UN SOLO hilo (el servidor de invitados);
 *   stickd_admin_html / admin_post / resumen / start / stop pueden llegar desde
 *   hilos distintos: protégete con un mutex si guardas estado.
 * - El cuerpo (body) es un buffer CRUDO de body_len bytes: puede contener NUL
 *   (subidas de archivo). No lo trates como string.
 * - Tus datos persistentes van en /var/lib/stickd/<id>/ (el stick crea la
 *   carpeta al cargarte). No escribas fuera de ahí.
 * - Los formularios del panel llegan ya codificados como x-www-form-urlencoded
 *   en form_urlencoded (decodifícalos con tu librería; en C, mira el ejemplo).
 *
 * === Importante sobre seguridad ===
 * Un plugin es código nativo que corre como root dentro del proceso stickd. No
 * hay caja de arena: quien instale tu .so confía en ti por completo. No pidas
 * cargar plugins de desconocidos sin leer el código.
 *
 * === Compilar (aarch64) ===
 *   desde tu PC:    aarch64-linux-gnu-gcc -shared -fPIC -O2 -o mi.so mi.c
 *   en el stick:    gcc -shared -fPIC -O2 -o mi.so mi.c   (si tiene compilador)
 * === Instalar ===
 *   - panel: Plugins → Subir (elige el .so), o
 *   - scp mi.so user@192.168.5.1:/var/lib/stickd/plugins/  y panel: «Recargar carpeta»
 * === Quitar ===
 *   - panel: Plugins → Quitar (borra el archivo y descarga el módulo)
 */
#ifndef STICKD_ABI_H
#define STICKD_ABI_H

#include <stddef.h>
#include <stdint.h>
#include <stdbool.h>

#define STICKD_ABI_VERSION 2

/* ------------------------------------------------------------------ */
/* API del host: funciones del stick que tu plugin puede llamar.      */
/* El stick te pasa un puntero a esta tabla en stickd_init(). Vive    */
/* para siempre; guárdala. Todos los char* devueltos se liberan con   */
/* free_string(). Las de red/SMS pueden tardar; no llames desde       */
/* stickd_guest() si quieres respuesta inmediata.                     */
/* ------------------------------------------------------------------ */
typedef struct stickd_host {
    uint32_t abi_version; /* == STICKD_ABI_VERSION */

    /* Escribe una línea en el journal del stick (journalctl -u stickd). */
    void (*log)(const char *msg);
    /* Libera un char* devuelto por cualquier función de esta tabla. */
    void (*free_string)(char *s);

    /* Estado del uplink, JSON:
       {"internet":bool,"salida":"usb|wifi|lte|otro|-","dispositivo":"...",
        "ssid":"...","lte":{"sim":bool,"estado":"...","senal":"...",
        "tecnologia":"...","operador":"..."},"wireguard":"..."} */
    char *(*uplink_json)(void);

    /* Redes WiFi cercanas, JSON: [{"ssid":"...","senal":N,"seguridad":"..."}]. */
    char *(*wifi_scan_json)(void);

    /* GET http/https por el uplink activo (usa curl, máx 8 MB / 30 s). Devuelve
       el cuerpo (texto; se corta en el primer NUL) o NULL si falla. Escribe el
       código HTTP en *status si no es NULL. Libera el resultado con free_string. */
    char *(*http_get)(const char *url, int *status);

    /* Manda un SMS por el módem (si hay SIM). number: dígitos, + y espacios.
       Devuelve true si se envió. */
    bool (*sms_send)(const char *number, const char *text);
    /* SMS recibidos/guardados, JSON:
       [{"numero":"...","texto":"...","hora":"...","estado":"..."}]. */
    char *(*sms_list_json)(void);

    /* Enciende/apaga un LED. cual: "rojo"|"verde"|"azul". patron: "on"|"off"|
       "timer"|"heartbeat". Nota: el núcleo también maneja los LEDs y puede
       volver a pintarlos; úsalo sobre todo con usa_ap=0. */
    void (*led)(const char *cual, const char *patron);

    /* Invitados conectados al AP de la vitrina, JSON:
       [{"ip":"192.168.4.x","mac":"...","internet":bool}]. */
    char *(*guests_json)(void);
} stickd_host;

/* ------------------------------------------------------------------ */
/* Lo que TU plugin exporta (todos obligatorios).                     */
/* ------------------------------------------------------------------ */

typedef struct {
    uint32_t abi_version; /* STICKD_ABI_VERSION */
    const char *id;       /* corto y estable: [a-z0-9-], ≤ 64, p.ej. "hola" */
    const char *nombre;   /* nombre a mostrar en el panel */
    const char *descripcion; /* una línea para el selector de modo */
    uint8_t usa_ap;       /* 1 = el stick levanta el AP + portal cautivo (normal); 0 = tú manejas el radio */
    uint32_t max_body;    /* máx. bytes de cuerpo de guest (0 = 16 KB) */
} stickd_info;

typedef struct {
    uint16_t estado;      /* código HTTP (0 = 200) */
    const char *cabeceras; /* "Clave: valor\nClave2: valor2"; Content-Type va aquí */
    const uint8_t *cuerpo;
    size_t cuerpo_len;
} stickd_respuesta;

/* Obligatorio. Se llama una vez al cargar el .so, antes de nada más.
 * `host` es la tabla de la API del host (vive para siempre; guárdala).
 * Devuelve false si no puedes trabajar (el stick no usará el plugin). */
bool stickd_init(uint32_t abi_version, const stickd_host *host);
/* Obligatorio. Se llama una vez al quitar el plugin (justo antes de dlclose). */
void stickd_shutdown(void);
/* Obligatorio. Devuelve el info estático (los strings viven para siempre). */
const stickd_info *stickd_get_info(void);

/* Petición de alguien conectado al WiFi de la vitrina (toda URL del portal
 * cautivo, salvo las sondas de detección, que el núcleo ya contesta).
 * handle: reservado, pasa 0. Devuelve una respuesta NUEVA (calloc). */
stickd_respuesta *stickd_guest(uintptr_t handle,
                               const char *method,      /* "GET", "POST"… */
                               const char *path,        /* "/" o "/x/y" (sin query) */
                               const char *query,       /* "a=1&b=2" (sin '?') */
                               const char *host,        /* el Host original (puede ser cualquier dominio) */
                               const char *user_agent,
                               const char *content_type,
                               const char *client_ip,   /* "192.168.4.x" */
                               const uint8_t *body,     /* NULL si body_len == 0 */
                               size_t body_len);
void stickd_free_respuesta(stickd_respuesta *r);

/* HTML (fragmento, sin <html>) de tu sección en el panel de administración.
 * Tus formularios deben enviar a "POST /vitrina/<tu-id>/<accion>". */
char *stickd_admin_html(uintptr_t handle);
/* POST /vitrina/<tu-id>/<accion>. `form_urlencoded` es el cuerpo decodificado
 * si era x-www-form-urlencoded, o la query string si no lo era; `body` es el
 * cuerpo crudo (subidas por fetch). Devuelve el mensaje a mostrar al usuario. */
char *stickd_admin_post(uintptr_t handle,
                        const char *accion,
                        const char *form_urlencoded,
                        const uint8_t *body,
                        size_t body_len);
/* Una línea de estado para el panel (p.ej. "12 mensajes"). "" si no hay nada. */
char *stickd_resumen(uintptr_t handle);
/* Libera lo que devolvieron stickd_admin_html/admin_post/resumen. */
void stickd_free_string(char *s);

/* Se llama al poner la vitrina al aire / al caerla. */
void stickd_start(uintptr_t handle);
void stickd_stop(uintptr_t handle);

#endif /* STICKD_ABI_H */
