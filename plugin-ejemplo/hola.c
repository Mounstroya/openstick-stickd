/* Plugin de ejemplo para stickd (ABI v2).
 *
 * Es lo más simple que puede ser un módulo: una página que saluda y cuenta
 * visitas, con una sección de panel y resumen. Sirve para probar el cargador y
 * como plantilla para escribir plugins propios. Muestra además cómo usar la
 * API del host: `stickd_init` recibe una tabla de funciones del stick (log,
 * led, uplink, wifi, http, sms…). Aquí se usan `log` y `led`.
 *
 * Compilar para el stick (aarch64) desde tu PC:
 *   aarch64-linux-gnu-gcc -shared -fPIC -O2 -o hola.so hola.c
 * O en el propio stick si tiene compilador:
 *   gcc -shared -fPIC -O2 -o hola.so hola.c
 * Y desde el panel: Plugins → Subir (o scp a /var/lib/stickd/plugins/ + Recargar).
 *
 * Datos persistentes: /var/lib/stickd/hola/visitas (un número).
 */
#include "../plugins/abi.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Tabla de funciones del stick; la guarda stickd_init y la usan las demás. */
static const stickd_host *H = NULL;

#define DATOS "/var/lib/stickd/hola/visitas"

static long visitas_leer(void) {
    long n = 0;
    FILE *f = fopen(DATOS, "r");
    if (f) {
        if (fscanf(f, "%ld", &n) != 1)
            n = 0;
        fclose(f);
    }
    return n;
}

static void visitas_escribir(long n) {
    FILE *f = fopen(DATOS, "w");
    if (f) {
        fprintf(f, "%ld", n);
        fclose(f);
    }
}

bool stickd_init(uint32_t abi_version, const stickd_host *host) {
    if (abi_version != STICKD_ABI_VERSION || host == NULL)
        return false;
    H = host;
    if (H->log)
        H->log("hola: inicializado con la API del host");
    return true;
}

void stickd_shutdown(void) {}

const stickd_info *stickd_get_info(void) {
    static const stickd_info i = {
        .abi_version = STICKD_ABI_VERSION,
        .id = "hola",
        .nombre = "Hola (ejemplo)",
        .descripcion = "Saluda a quien se conecta y cuenta las visitas",
        .usa_ap = 1,
        .max_body = 0,
    };
    return &i;
}

stickd_respuesta *stickd_guest(uintptr_t handle, const char *method, const char *path, const char *query, const char *host,
                               const char *user_agent, const char *content_type, const char *client_ip, const uint8_t *body,
                               size_t body_len) {
    (void)handle; (void)method; (void)query; (void)host; (void)user_agent; (void)content_type; (void)body; (void)body_len;

    stickd_respuesta *r = calloc(1, sizeof *r);
    if (strcmp(path, "/") != 0) {
        r->estado = 404;
        r->cabeceras = strdup("Content-Type: text/plain; charset=utf-8");
        r->cuerpo = strdup("no encontrado");
        r->cuerpo_len = strlen((const char *)r->cuerpo);
        return r;
    }

    long n = visitas_leer() + 1;
    visitas_escribir(n);

    char buf[320];
    snprintf(buf, sizeof buf,
             "<!doctype html><html lang=es><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">"
             "<body style=\"font-family:system-ui;margin:40px\"><h1>Hola</h1>"
             "<p>Esta página la sirve un <b>plugin</b> (.so) cargado en caliente por stickd.</p>"
             "<p>Visita n.º %ld desde <code>%s</code>.</p>"
             "<p>Para ver el contador, abre el panel y mira la sección «Hola (ejemplo)».</p></body>",
             n, client_ip);

    r->estado = 200;
    r->cabeceras = strdup("Content-Type: text/html; charset=utf-8");
    r->cuerpo = strdup(buf);
    r->cuerpo_len = strlen(buf);
    return r;
}

void stickd_free_respuesta(stickd_respuesta *r) {
    if (!r)
        return;
    free((void *)r->cabeceras);
    free((void *)r->cuerpo);
    free(r);
}

char *stickd_admin_html(uintptr_t handle) {
    (void)handle;
    return strdup("<p class=mut>Plugin de ejemplo (hola.c): solo saluda y cuenta visitas. "
                  "No hay nada que configurar. Bórrame desde la lista de arriba cuando quieras.</p>");
}

char *stickd_admin_post(uintptr_t handle, const char *accion, const char *form_urlencoded, const uint8_t *body, size_t body_len) {
    (void)handle; (void)form_urlencoded; (void)body; (void)body_len;
    if (strcmp(accion, "saludo") == 0) {
        /* ejemplo de uso de la API del host: parpadea el LED verde */
        if (H && H->led)
            H->led("verde", "heartbeat");
        return strdup("Hola: este plugin se cargó sin reiniciar el stick (y parpadeó el LED verde).");
    }
    return strdup("Acción no válida.");
}

char *stickd_resumen(uintptr_t handle) {
    (void)handle;
    char buf[64];
    snprintf(buf, sizeof buf, "%ld visitas", visitas_leer());
    return strdup(buf);
}

void stickd_free_string(char *s) {
    free(s);
}

void stickd_start(uintptr_t handle) {
    (void)handle;
}

void stickd_stop(uintptr_t handle) {
    (void)handle;
}
