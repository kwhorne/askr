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
#include <zend_API.h>
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
static int g_worker_mode = 0;      /* 1 while inside the persistent worker loop */

/* Worker-request body (declared early: askr_read_post references it). */
static char  *w_body;
static size_t w_body_len;

/* Script mode (artisan queue/scheduler sidecars): output goes straight to
 * stdout instead of being captured, so a long-running command can't grow an
 * ever-larger capture buffer. */
static int g_script_mode = 0;

/* ------------------------------------------------------------------ */
/* SAPI callbacks                                                     */
/* ------------------------------------------------------------------ */

static size_t askr_ub_write(const char *str, size_t len) {
    if (g_script_mode) {
        size_t n = fwrite(str, 1, len, stdout);
        fflush(stdout); /* sidecar output should reach logs promptly */
        return n;
    }
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

static size_t w_body_off_read = 0;

static size_t askr_read_post(char *buffer, size_t count_bytes) {
    if (g_worker_mode) {
        size_t avail = w_body_len - w_body_off_read;
        size_t n = count_bytes < avail ? count_bytes : avail;
        if (n) {
            memcpy(buffer, w_body + w_body_off_read, n);
            w_body_off_read += n;
        }
        return n;
    }
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

/* Defined in the worker section below; registered at module startup so the
 * function is available to worker scripts. */
static const zend_function_entry askr_functions[];

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

    /* Register askr_handle_request() at module startup (MINIT). */
    php_embed_module.additional_functions = askr_functions;

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
/* persistent worker loop (A4: boot once, serve many)                 */
/* ------------------------------------------------------------------ */
/*
 * The worker script boots the application once, then loops calling the PHP
 * function askr_handle_request($handler). Each call blocks until Rust delivers
 * a request, invokes $handler($request) against the already-booted app, and
 * ships the captured output/headers/status back to Rust — with no per-request
 * framework bootstrap. This is the Octane model, in-process.
 */

/* Rust-provided bridge callbacks. */
typedef int (*askr_wait_fn)(void *ctx);   /* block; 1 = request ready, 0 = stop */
typedef void (*askr_reply_fn)(void *ctx, const char *body, size_t blen,
                              const char *hdrs, size_t hlen, int status);

static askr_wait_fn  g_wait = NULL;
static askr_reply_fn g_reply = NULL;
static void         *g_ctx = NULL;

/* Current worker request, populated by Rust via the setters below. */
#define ASKR_MAX_HEADERS 128
static char  *w_method;
static char  *w_uri;
static char  *w_query;
static char  *w_hnames[ASKR_MAX_HEADERS];
static char  *w_hvalues[ASKR_MAX_HEADERS];
static int    w_nheaders;

static char *dup_cstr(const char *s) {
    if (!s) return NULL;
    size_t n = strlen(s) + 1;
    char *p = (char *)malloc(n);
    if (p) memcpy(p, s, n);
    return p;
}

/* Setters called by Rust from inside the wait callback. */
void askr_req_reset(void) {
    free(w_method); w_method = NULL;
    free(w_uri);    w_uri = NULL;
    free(w_query);  w_query = NULL;
    for (int i = 0; i < w_nheaders; i++) { free(w_hnames[i]); free(w_hvalues[i]); }
    w_nheaders = 0;
    free(w_body); w_body = NULL; w_body_len = 0;
}

void askr_req_set_meta(const char *method, const char *uri, const char *query) {
    w_method = dup_cstr(method);
    w_uri = dup_cstr(uri);
    w_query = dup_cstr(query);
}

void askr_req_add_header(const char *name, const char *value) {
    if (w_nheaders < ASKR_MAX_HEADERS) {
        w_hnames[w_nheaders] = dup_cstr(name);
        w_hvalues[w_nheaders] = dup_cstr(value);
        w_nheaders++;
    }
}

void askr_req_set_body(const char *ptr, size_t len) {
    free(w_body);
    w_body = (char *)malloc(len + 1);
    if (w_body) { memcpy(w_body, ptr, len); w_body[len] = '\0'; }
    w_body_len = len;
}

/* Reset SAPI response state between iterations (no RSHUTDOWN). */
static void worker_reset_response(void) {
    buf_reset(&g_req.out);
    buf_reset(&g_req.hdr);
    SG(sapi_headers).http_response_code = 200;
    SG(headers_sent) = 0;
    zend_llist_clean(&SG(sapi_headers).headers);
    SG(sapi_headers).send_default_content_type = 1;
}

/* bool askr_handle_request(callable $handler) */
ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_handle_request, 0, 0, 1)
    ZEND_ARG_INFO(0, handler)
ZEND_END_ARG_INFO()

static PHP_FUNCTION(askr_handle_request) {
    zval *handler;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_ZVAL(handler)
    ZEND_PARSE_PARAMETERS_END();

    if (!g_wait || !g_wait(g_ctx)) {
        RETURN_FALSE;
    }
    worker_reset_response();

    /* Build $request = ['method','uri','query','headers'=>[..],'body']. */
    zval request;
    array_init(&request);
    add_assoc_string(&request, "method", w_method ? w_method : "GET");
    add_assoc_string(&request, "uri", w_uri ? w_uri : "/");
    add_assoc_string(&request, "query", w_query ? w_query : "");
    zval headers;
    array_init(&headers);
    for (int i = 0; i < w_nheaders; i++) {
        add_assoc_string(&headers, w_hnames[i], w_hvalues[i]);
    }
    add_assoc_zval(&request, "headers", &headers);
    add_assoc_stringl(&request, "body", w_body ? w_body : "", w_body_len);

    /* $handler($request) */
    zval retval, params[1];
    ZVAL_COPY_VALUE(&params[0], &request);
    if (call_user_function(NULL, NULL, handler, &retval, 1, params) == SUCCESS) {
        zval_ptr_dtor(&retval);
    }
    zval_ptr_dtor(&request);

    /* Flush headers into our capture even if the body was empty. */
    sapi_send_headers();

    int status = SG(sapi_headers).http_response_code;
    if (status == 0) status = 200;

    if (g_reply) {
        g_reply(g_ctx, g_req.out.ptr, g_req.out.len, g_req.hdr.ptr, g_req.hdr.len, status);
    }
    RETURN_TRUE;
}

/* ------------------------------------------------------------------ */
/* shared cache bridge (askr_cache_*)                                 */
/* ------------------------------------------------------------------ */

typedef int  (*askr_cache_get_fn)(const char *key, size_t klen, char **out, size_t *out_len);
typedef int  (*askr_cache_set_fn)(const char *key, size_t klen, const char *val, size_t vlen, long ttl);
typedef int  (*askr_cache_del_fn)(const char *key, size_t klen);
typedef long (*askr_cache_incr_fn)(const char *key, size_t klen, long delta, long ttl);
typedef void (*askr_cache_flush_fn)(void);

static askr_cache_get_fn   g_cache_get = NULL;
static askr_cache_set_fn   g_cache_set = NULL;
static askr_cache_del_fn   g_cache_del = NULL;
static askr_cache_incr_fn  g_cache_incr = NULL;
static askr_cache_flush_fn g_cache_flush = NULL;

void askr_php_set_cache_bridge(askr_cache_get_fn g, askr_cache_set_fn s, askr_cache_del_fn d,
                               askr_cache_incr_fn i, askr_cache_flush_fn f) {
    g_cache_get = g;
    g_cache_set = s;
    g_cache_del = d;
    g_cache_incr = i;
    g_cache_flush = f;
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_get, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_get) {
    char *key; size_t klen;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, klen)
    ZEND_PARSE_PARAMETERS_END();
    char *out = NULL; size_t olen = 0;
    if (g_cache_get && g_cache_get(key, klen, &out, &olen)) {
        RETVAL_STRINGL(out, olen); /* copies */
        free(out);
    } else {
        RETVAL_NULL();
    }
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_set, 0, 0, 2)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, value)
    ZEND_ARG_INFO(0, ttl)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_set) {
    char *key, *val; size_t klen, vlen; zend_long ttl = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(key, klen)
        Z_PARAM_STRING(val, vlen)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(ttl)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(g_cache_set ? g_cache_set(key, klen, val, vlen, (long)ttl) : 0);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_delete, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_delete) {
    char *key; size_t klen;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(key, klen)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(g_cache_del ? g_cache_del(key, klen) : 0);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_increment, 0, 0, 1)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, delta)
    ZEND_ARG_INFO(0, ttl)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_increment) {
    char *key; size_t klen; zend_long delta = 1; zend_long ttl = 0;
    ZEND_PARSE_PARAMETERS_START(1, 3)
        Z_PARAM_STRING(key, klen)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(delta)
        Z_PARAM_LONG(ttl)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG(g_cache_incr ? g_cache_incr(key, klen, (long)delta, (long)ttl) : 0);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_flush, 0, 0, 0)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_flush) {
    if (g_cache_flush) g_cache_flush();
}

static const zend_function_entry askr_functions[] = {
    ZEND_FE(askr_handle_request, arginfo_askr_handle_request)
    ZEND_FE(askr_cache_get, arginfo_askr_cache_get)
    ZEND_FE(askr_cache_set, arginfo_askr_cache_set)
    ZEND_FE(askr_cache_delete, arginfo_askr_cache_delete)
    ZEND_FE(askr_cache_increment, arginfo_askr_cache_increment)
    ZEND_FE(askr_cache_flush, arginfo_askr_cache_flush)
    ZEND_FE_END
};

/* Run the worker script in one long-lived request context. Blocks until the
 * worker loop ends (g_wait returns 0). */
int askr_php_run_worker(const char *script, askr_wait_fn wait, askr_reply_fn reply, void *ctx) {
    g_wait = wait;
    g_reply = reply;
    g_ctx = ctx;

    SG(server_context) = &g_req; /* non-NULL => live connection */
    SG(request_info).request_method = "GET";
    SG(request_info).path_translated = (char *)script;
    SG(request_info).request_uri = (char *)"";
    SG(request_info).proto_num = 1001;
    SG(sapi_headers).http_response_code = 200;

    if (php_request_startup() == FAILURE) {
        return -1;
    }
    g_worker_mode = 1;

    int rc = 0;
    zend_first_try {
        zend_file_handle fh;
        zend_stream_init_filename(&fh, script);
        if (php_execute_script(&fh) == false) {
            rc = 1;
        }
        zend_destroy_file_handle(&fh);
    } zend_catch {
        rc = 2;
    } zend_end_try();

    g_worker_mode = 0;
    php_request_shutdown((void *)0);
    askr_req_reset();
    return rc;
}

/* ------------------------------------------------------------------ */
/* run a script to completion (artisan queue/scheduler sidecars)      */
/* ------------------------------------------------------------------ */
/*
 * Execute a PHP file like a CLI invocation and block until it returns (queue
 * workers loop forever; the scheduler ticks). Output goes straight to stdout
 * (script mode). Returns the script's exit status.
 */
int askr_php_run_script(const char *script) {
    SG(server_context) = &g_req;
    SG(request_info).request_method = "GET";
    SG(request_info).path_translated = (char *)script;
    SG(request_info).request_uri = (char *)"";

    if (php_request_startup() == FAILURE) {
        return -1;
    }
    g_script_mode = 1;

    int rc = 0;
    zend_first_try {
        zend_file_handle fh;
        zend_stream_init_filename(&fh, script);
        if (php_execute_script(&fh) == false) {
            rc = 1;
        }
        zend_destroy_file_handle(&fh);
    } zend_catch {
        rc = 2;
    } zend_end_try();

    int exit_status = EG(exit_status);
    g_script_mode = 0;
    php_request_shutdown((void *)0);

    return exit_status ? exit_status : rc;
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
