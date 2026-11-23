/* Plugin "Compilador de plugins" para stickd.
 *
 * Deja escribir (o pegar) el código C de un plugin en el panel y lo compila EN
 * EL PROPIO STICK con gcc, dejando el .so en /var/lib/stickd/plugins/ y
 * cargándolo al instante (host->recargar). Así se pueden hacer plugins sin PC.
 *
 * Nota: NO uses tcc en este aparato: su backend aarch64 (0.9.27) genera mal el
 * struct stickd_info y el plugin segfaultea al cargarse. Usa gcc.
 *
 * Requisitos en el stick:
 *   - gcc instalado            (sudo apt install gcc)
 *   - el header abi.h en       /var/lib/stickd/compilador/abi.h
 *     (se copia ahí al instalar este plugin)
 *
 * Compilar este plugin (desde la PC):
 *   aarch64-linux-gnu-gcc -shared -fPIC -O2 -Iplugins -o compilador.so plugin-ejemplo/compilador.c
 *
 * Seguridad: el .so que compiles corre como root dentro de stickd. Esto es una
 * herramienta del panel de administración; solo tú (con acceso al panel) la usas.
 */
#include "abi.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define DIR_DATOS   "/var/lib/stickd/compilador"
#define DIR_PLUGINS "/var/lib/stickd/plugins"
#define ABI_H       DIR_DATOS "/abi.h"

static const stickd_host *H = NULL;

bool stickd_init(uint32_t abi_version, const stickd_host *host) {
    if (abi_version != STICKD_ABI_VERSION || host == NULL)
        return false;
    H = host;
    if (H->log)
        H->log("compilador: listo");
    return true;
}

void stickd_shutdown(void) {}

const stickd_info *stickd_get_info(void) {
    static const stickd_info i = {
        .abi_version = STICKD_ABI_VERSION,
        .id = "compilador",
        .nombre = "Compilador de plugins",
        .descripcion = "Compila el código C de un plugin en el propio stick (tcc)",
        .usa_ap = 1,
        .max_body = 512 * 1024, /* código fuente de hasta 512 KB */
    };
    return &i;
}

/* ---- utilidades ---- */

/* Decodifica x-www-form-urlencoded in situ-ish: dst debe tener tamaño >= len(src)+1. */
static void urldecode(char *dst, const char *src) {
    char a, b;
    while (*src) {
        if (*src == '%' && (a = src[1]) && (b = src[2])) {
            a = (a >= 'a') ? a - 'a' + 10 : (a >= 'A') ? a - 'A' + 10 : a - '0';
            b = (b >= 'a') ? b - 'a' + 10 : (b >= 'A') ? b - 'A' + 10 : b - '0';
            *dst++ = (char)(a * 16 + b);
            src += 3;
        } else if (*src == '+') {
            *dst++ = ' ';
            src++;
        } else {
            *dst++ = *src++;
        }
    }
    *dst = '\0';
}

/* Extrae el valor (decodificado) del campo `clave` del cuerpo urlencoded.
   Devuelve un buffer malloc (liberar con free) o NULL si no está. */
static char *campo(const char *form, const char *clave) {
    size_t kl = strlen(clave);
    const char *p = form;
    while (p && *p) {
        if (strncmp(p, clave, kl) == 0 && p[kl] == '=') {
            const char *v = p + kl + 1;
            const char *fin = strchr(v, '&');
            size_t n = fin ? (size_t)(fin - v) : strlen(v);
            char *crudo = malloc(n + 1);
            if (!crudo) return NULL;
            memcpy(crudo, v, n);
            crudo[n] = '\0';
            char *out = malloc(n + 1);
            if (out) urldecode(out, crudo);
            free(crudo);
            return out;
        }
        p = strchr(p, '&');
        if (p) p++;
    }
    return NULL;
}

static int nombre_valido(const char *n) {
    if (!n || !*n) return 0;
    size_t len = strlen(n);
    if (len > 40) return 0;
    for (size_t i = 0; i < len; i++) {
        char c = n[i];
        if (!((c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_'))
            return 0;
    }
    return 1;
}

/* escapa texto para meterlo en HTML */
static char *html_escape(const char *s) {
    size_t n = 0;
    for (const char *p = s; *p; p++)
        n += (*p == '<' || *p == '>') ? 4 : (*p == '&') ? 5 : 1;
    char *o = malloc(n + 1), *w = o;
    if (!o) return NULL;
    for (const char *p = s; *p; p++) {
        if (*p == '<') { memcpy(w, "&lt;", 4); w += 4; }
        else if (*p == '>') { memcpy(w, "&gt;", 4); w += 4; }
        else if (*p == '&') { memcpy(w, "&amp;", 5); w += 5; }
        else *w++ = *p;
    }
    *w = '\0';
    return o;
}

/* ---- páginas ---- */

stickd_respuesta *stickd_guest(uintptr_t handle, const char *method, const char *path, const char *query,
                               const char *host, const char *user_agent, const char *content_type,
                               const char *client_ip, const uint8_t *body, size_t body_len) {
    (void)handle; (void)method; (void)path; (void)query; (void)host; (void)user_agent;
    (void)content_type; (void)client_ip; (void)body; (void)body_len;
    stickd_respuesta *r = calloc(1, sizeof *r);
    if (!r) return NULL;
    r->estado = 200;
    r->cabeceras = strdup("Content-Type: text/html; charset=utf-8");
    r->cuerpo = (const uint8_t *)strdup(
        "<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;margin:40px\">"
        "<h1>Compilador de plugins</h1><p>Esta es una herramienta del panel de administración. "
        "Abre el panel del stick para usarla.</p>");
    r->cuerpo_len = r->cuerpo ? strlen((const char *)r->cuerpo) : 0;
    return r;
}

void stickd_free_respuesta(stickd_respuesta *r) {
    if (!r) return;
    free((void *)r->cabeceras);
    free((void *)r->cuerpo);
    free(r);
}

char *stickd_admin_html(uintptr_t handle) {
    (void)handle;
    return strdup(
        "<p class=mut style=margin-top:0>Escribe o pega el código C de un plugin y compílalo "
        "aquí mismo. Debe incluir <code>#include \"abi.h\"</code>. Al compilar, el <code>.so</code> "
        "se carga al instante.</p>"
        "<form method=post action=/vitrina/compilador/compilar>"
        "<label>Nombre (letras, dígitos, - y _)<input name=nombre maxlength=40 required "
        "pattern=\"[a-z0-9_-]+\" placeholder=mi-plugin style=\"display:block;width:100%;margin:4px 0 10px\"></label>"
        "<label>Código C<textarea name=codigo rows=16 required spellcheck=false "
        "style=\"display:block;width:100%;font-family:monospace;white-space:pre\" "
        "placeholder='#include \"abi.h\"\n#include &lt;string.h&gt;\n...'></textarea></label>"
        "<button style=margin-top:8px>Compilar y cargar</button></form>"
        "<p class=mut>Si ya existe un plugin con ese id, quítalo primero en la lista de plugins antes de recompilarlo.</p>");
}

char *stickd_admin_post(uintptr_t handle, const char *accion, const char *form_urlencoded,
                        const uint8_t *body, size_t body_len) {
    (void)handle; (void)body; (void)body_len;
    if (strcmp(accion, "compilar") != 0)
        return strdup("Acción no válida.");

    char *nombre = campo(form_urlencoded, "nombre");
    char *codigo = campo(form_urlencoded, "codigo");
    if (!nombre || !codigo || !nombre_valido(nombre)) {
        free(nombre); free(codigo);
        return strdup("Falta el nombre (solo minúsculas, dígitos, - y _) o el código.");
    }

    /* escribir el .c */
    char ruta_c[256];
    snprintf(ruta_c, sizeof ruta_c, "%s/%s.c", DIR_DATOS, nombre);
    FILE *f = fopen(ruta_c, "w");
    if (!f) { free(nombre); free(codigo); return strdup("No pude escribir el archivo fuente."); }
    fputs(codigo, f);
    fclose(f);
    free(codigo);

    /* comprobar que exista abi.h */
    FILE *ah = fopen(ABI_H, "r");
    if (!ah) {
        free(nombre);
        return strdup("Falta " ABI_H ". Cópialo al stick (viene en plugins/abi.h del repo).");
    }
    fclose(ah);

    /* compilar con tcc; nombre ya validado => sin inyecci\xc3\xb3n */
    char cmd[768];
    snprintf(cmd, sizeof cmd,
             "cc -shared -fPIC -O2 -o '%s/%s.so' -I '%s' '%s' 2>&1",
             DIR_PLUGINS, nombre, DIR_DATOS, ruta_c);
    FILE *p = popen(cmd, "r");
    char salida[4096] = {0};
    size_t off = 0;
    if (p) {
        size_t n;
        while (off < sizeof salida - 1 && (n = fread(salida + off, 1, sizeof salida - 1 - off, p)) > 0)
            off += n;
        salida[off] = '\0';
    }
    int rc = p ? pclose(p) : -1;

    char *msg = malloc(5000);
    if (!msg) { free(nombre); return strdup("Sin memoria."); }

    if (rc == 0) {
        int cargados = (H && H->recargar) ? H->recargar() : -1;
        if (cargados > 0)
            snprintf(msg, 5000, "Compilado «%s.so» y cargado. Ya aparece en la lista de vitrinas.", nombre);
        else if (cargados == 0)
            snprintf(msg, 5000, "Compilado «%s.so», pero no se cargó (¿id duplicado? quítalo y usa «Recargar carpeta»).", nombre);
        else
            snprintf(msg, 5000, "Compilado «%s.so». Usa «Recargar carpeta» para cargarlo.", nombre);
    } else {
        char *esc = html_escape(salida[0] ? salida : "(sin mensaje; ¿gcc instalado? sudo apt install gcc)");
        snprintf(msg, 5000, "Error al compilar «%s»:<br><pre style=\"white-space:pre-wrap\">%s</pre>",
                 nombre, esc ? esc : "");
        free(esc);
    }
    free(nombre);
    return msg;
}

char *stickd_resumen(uintptr_t handle) {
    (void)handle;
    return strdup("escribe C y compílalo en el stick");
}

void stickd_free_string(char *s) { free(s); }
void stickd_start(uintptr_t handle) { (void)handle; }
void stickd_stop(uintptr_t handle) { (void)handle; }
