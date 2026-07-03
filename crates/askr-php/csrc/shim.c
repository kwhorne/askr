/*
 * Askr PHP embed shim.
 *
 * A thin C layer over PHP's embed SAPI. It runs the Zend engine in-process (no
 * FastCGI, no FPM) and gives Rust a proper *server* request cycle:
 *
 *   startup (once)          MINIT — boot the engine
 *   handle  (per request)   RINIT -> execute script -> RSHUTDOWN, capturing
 *                           status + headers + body, feeding $_SERVER and
 *                           php://input
 *   shutdown (once)         MSHUTDOWN
 *
 * The embed SAPI's php_embed_init() is deliberately CLI-shaped: it suppresses
 * headers (no_headers = 1) and owns a single request. We bypass it and drive
 * sapi_startup / module startup / php_request_startup ourselves so we can
 * capture HTTP headers and run many requests against one warm interpreter.
 *
 * Single-threaded, non-ZTS: one interpreter per thread. Current-request state
 * lives in a single file-global (g_req) because the SAPI callbacks
 * (ub_write / send_header / read_post / register_variables) don't receive a
 * user context pointer.
 */

#include <php_embed.h>
#include <zend_stream.h>
#include <SAPI.h>

#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

/* ------------------------------------------------------------------ */
/* growable byte buffer                                               */
/* ------------------------------------------------------------------ */

typedef struct {
    char  *ptr;
    size_t len;
    size_t cap;
} buf_t;

static int buf_append(buf_t *b, const char *s, size_t n) {
    if (b->len + n + 1 > b->cap) {
        size_t ncap = b->cap ? b->cap : 4096;
        while (ncap < b->len + n + 1) {
            ncap *= 2;
        }
        char *np = (char *)realloc(b->ptr, ncap);
        if (!np) {
            return -1;
        }
        b->ptr = np;
        b->cap = ncap;
    }
    memcpy(b->ptr + b->len, s, n);
    b->len += n;
    b->ptr[b->len] = '\0';
    return 0;
}

static void buf_reset(buf_t *b) {
    b->len = 0;
    if (b->ptr) {
        b->ptr[0] = '\0';
    }
}

/* ------------------------------------------------------------------ */
/* current-request state                                             */
/* ------------------------------------------------------------------ */

typedef struct {
    /* inputs, borrowed from the Rust caller for the duration of a call */
    const char *const *names;
    const char *const *values;
    int                nvars;

    const char *body;
    size_t      body_len;
    size_t      body_off;

    /* outputs, owned here */
    buf_t out;
    buf_t hdr;
} askr_req;

static askr_req g_req;             /* single-threaded: the one in-flight request */
static const char *g_cookie = NULL; /* Cookie header for the current request */

/* ------------------------------------------------------------------ */
/* SAPI callbacks                                                     */
/* ------------------------------------------------------------------ */

static size_t askr_ub_write(const char *str, size_t len) {
    if (buf_append(&g_req.out, str, len) != 0) {
        return 0;
    }
    return len;
}

static void askr_send_header(sapi_header_struct *h, void *server_context) {
    (void)server_context;
    if (h && h->header && h->header_len) {
        buf_append(&g_req.hdr, h->header, h->header_len);
        buf_append(&g_req.hdr, "\r\n", 2);
    }
}

static size_t askr_read_post(char *buffer, size_t count_bytes) {
    size_t avail = g_req.body_len - g_req.body_off;
    size_t n = count_bytes < avail ? count_bytes : avail;
    if (n) {
        memcpy(buffer, g_req.body + g_req.body_off, n);
        g_req.body_off += n;
    }
    return n;
}

static char *askr_read_cookies(void) {
    return (char *)g_cookie;
}

static void askr_register_variables(zval *track_vars_array) {
    for (int i = 0; i < g_req.nvars; i++) {
        const char *name = g_req.names[i];
        const char *val = g_req.values[i];
        if (name && val) {
            php_register_variable_safe((char *)name, (char *)val, strlen(val), track_vars_array);
        }
    }
}

/* ------------------------------------------------------------------ */
/* lifecycle                                                         */
/* ------------------------------------------------------------------ */

/* Lowest-precedence INI defaults; an app .ini/.user.ini can still override. */
static char askr_ini[] =
    "html_errors=0\n"
    "implicit_flush=0\n"
    "output_buffering=0\n"
    "max_execution_time=0\n"
    "display_errors=1\n"
    "log_errors=0\n"
    "error_reporting=E_ALL\n"
    "register_argc_argv=0\n";

int askr_php_startup(void) {
#if defined(SIGPIPE) && defined(SIG_IGN)
    signal(SIGPIPE, SIG_IGN);
#endif
    zend_signal_startup();

    /* Wire our callbacks into the SAPI module before startup. */
    php_embed_module.ub_write = askr_ub_write;
    php_embed_module.send_header = askr_send_header;
    php_embed_module.read_post = askr_read_post;
    php_embed_module.read_cookies = askr_read_cookies;
    php_embed_module.register_server_variables = askr_register_variables;
    php_embed_module.flush = NULL;

    sapi_startup(&php_embed_module);

    /* Base INI plus optional extra lines from $ASKR_PHP_INI (e.g. to load
     * opcache: "zend_extension=.../opcache.so\nopcache.enable=1"). */
    const char *extra = getenv("ASKR_PHP_INI");
    if (extra && *extra) {
        size_t n = strlen(askr_ini) + strlen(extra) + 2;
        char *combined = (char *)malloc(n);
        if (combined) {
            snprintf(combined, n, "%s%s\n", askr_ini, extra);
            php_embed_module.ini_entries = combined; /* leaked once, lives for process */
        } else {
            php_embed_module.ini_entries = askr_ini;
        }
    } else {
        php_embed_module.ini_entries = askr_ini;
    }

    if (php_embed_module.startup(&php_embed_module) == FAILURE) {
        return -1;
    }
    /* Don't chdir into each script's directory (like CGI -C). */
    SG(options) |= SAPI_OPTION_NO_CHDIR;
    return 0;
}

void askr_php_shutdown(void) {
    php_module_shutdown();
    sapi_shutdown();
    free(g_req.out.ptr);
    free(g_req.hdr.ptr);
    memset(&g_req, 0, sizeof(g_req));
}

/*
 * Handle one request end-to-end.
 *
 * Returns 0 on success (a response was produced, even a 500), negative on a
 * hard failure to start the request. Output buffers are malloc'd and handed to
 * the caller, who frees them with askr_php_free().
 */
int askr_php_handle(
    const char *script_filename,
    const char *method,
    const char *query_string,
    const char *content_type,
    size_t      content_length,
    const char *body,
    size_t      body_len,
    const char *const *var_names,
    const char *const *var_values,
    int         nvars,
    const char *cookie,
    char      **out_body,
    size_t     *out_body_len,
    char      **out_headers,
    size_t     *out_headers_len,
    int        *out_status)
{
    /* reset per-request state */
    buf_reset(&g_req.out);
    buf_reset(&g_req.hdr);
    g_req.names = var_names;
    g_req.values = var_values;
    g_req.nvars = nvars;
    g_req.body = body;
    g_req.body_len = body_len;
    g_req.body_off = 0;
    g_cookie = cookie;

    /* request_info must be populated before RINIT (POST handling reads it). */
    SG(server_context) = &g_req; /* non-NULL => live connection */
    SG(request_info).request_method = method;
    SG(request_info).query_string = (char *)query_string;
    SG(request_info).content_type = content_type;
    SG(request_info).content_length = (zend_long)content_length;
    SG(request_info).path_translated = (char *)script_filename;
    SG(request_info).request_uri = (char *)query_string; /* refined below via $_SERVER */
    SG(request_info).proto_num = 1001;
    SG(sapi_headers).http_response_code = 200;

    if (php_request_startup() == FAILURE) {
        return -1;
    }

    int rc = 0;
    zend_first_try {
        zend_file_handle file_handle;
        zend_stream_init_filename(&file_handle, script_filename);
        if (php_execute_script(&file_handle) == false) {
            rc = 1; /* script raised a fatal; response still returned */
        }
        zend_destroy_file_handle(&file_handle);
    } zend_catch {
        rc = 2; /* bailout */
    } zend_end_try();

    *out_status = SG(sapi_headers).http_response_code;
    if (*out_status == 0) {
        *out_status = 200;
    }

    php_request_shutdown((void *)0);

    /* hand copies to the caller */
    *out_body_len = g_req.out.len;
    *out_body = (char *)malloc(g_req.out.len + 1);
    if (*out_body) {
        memcpy(*out_body, g_req.out.ptr ? g_req.out.ptr : "", g_req.out.len);
        (*out_body)[g_req.out.len] = '\0';
    }

    *out_headers_len = g_req.hdr.len;
    *out_headers = (char *)malloc(g_req.hdr.len + 1);
    if (*out_headers) {
        memcpy(*out_headers, g_req.hdr.ptr ? g_req.hdr.ptr : "", g_req.hdr.len);
        (*out_headers)[g_req.hdr.len] = '\0';
    }

    return rc;
}

/* ------------------------------------------------------------------ */
/* simple eval (kept for the M0 hello-world path)                     */
/* ------------------------------------------------------------------ */

int askr_php_eval(const char *code, char **out, size_t *out_len) {
    if (php_request_startup() == FAILURE) {
        return -1;
    }
    buf_reset(&g_req.out);

    int rc = 0;
    zend_first_try {
        if (zend_eval_string((char *)code, NULL, "askr eval") == FAILURE) {
            rc = -1;
        }
    } zend_catch {
        rc = -2;
    } zend_end_try();

    php_request_shutdown((void *)0);

    *out_len = g_req.out.len;
    *out = (char *)malloc(g_req.out.len + 1);
    if (*out) {
        memcpy(*out, g_req.out.ptr ? g_req.out.ptr : "", g_req.out.len);
        (*out)[g_req.out.len] = '\0';
    }
    return rc;
}

void askr_php_free(char *p) {
    free(p);
}
