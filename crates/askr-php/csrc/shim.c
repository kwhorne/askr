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
#include <zend_exceptions.h>
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

/* Parsed multipart POST fields + uploaded files (worker mode). */
#define ASKR_MAX_POST 256
static char  *w_pnames[ASKR_MAX_POST];
static char  *w_pvalues[ASKR_MAX_POST];
static int    w_npost;

#define ASKR_MAX_FILES 64
typedef struct {
    char  *field;
    char  *name;
    char  *type;
    char  *tmp;
    size_t size;
    int    error;
} w_file_t;
static w_file_t w_files[ASKR_MAX_FILES];
static int      w_nfiles;

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
    for (int i = 0; i < w_npost; i++) { free(w_pnames[i]); free(w_pvalues[i]); }
    w_npost = 0;
    for (int i = 0; i < w_nfiles; i++) {
        free(w_files[i].field); free(w_files[i].name);
        free(w_files[i].type);  free(w_files[i].tmp);
    }
    w_nfiles = 0;
    free(w_body); w_body = NULL; w_body_len = 0;
}

void askr_req_add_post(const char *name, const char *value) {
    if (w_npost < ASKR_MAX_POST) {
        w_pnames[w_npost] = dup_cstr(name);
        w_pvalues[w_npost] = dup_cstr(value);
        w_npost++;
    }
}

void askr_req_add_file(const char *field, const char *file_name, const char *content_type,
                       const char *tmp_path, size_t size, int error) {
    if (w_nfiles < ASKR_MAX_FILES) {
        w_files[w_nfiles].field = dup_cstr(field);
        w_files[w_nfiles].name = dup_cstr(file_name);
        w_files[w_nfiles].type = dup_cstr(content_type);
        w_files[w_nfiles].tmp = dup_cstr(tmp_path);
        w_files[w_nfiles].size = size;
        w_files[w_nfiles].error = error;
        w_nfiles++;
    }
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

/* Deferred closures (askr_defer): run after the response is handed to Rust, so
 * the client already has the reply while the worker does the extra work (email,
 * webhooks, logging) before it accepts the next request. */
#define ASKR_MAX_DEFER 256
static zval g_deferred[ASKR_MAX_DEFER];
static int  g_ndeferred = 0;

/* void askr_defer(callable $fn) */
ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_defer, 0, 0, 1)
    ZEND_ARG_INFO(0, callback)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_defer) {
    zval *cb;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_ZVAL(cb)
    ZEND_PARSE_PARAMETERS_END();
    if (g_ndeferred < ASKR_MAX_DEFER) {
        ZVAL_COPY(&g_deferred[g_ndeferred], cb);
        g_ndeferred++;
        RETURN_TRUE;
    }
    RETURN_FALSE;
}

/* Run and clear the deferred queue. Each callback is isolated: a thrown
 * exception is reported and cleared so it can't poison the next callback or the
 * next request. */
static void askr_run_deferred(void) {
    for (int i = 0; i < g_ndeferred; i++) {
        zval dret;
        if (call_user_function(NULL, NULL, &g_deferred[i], &dret, 0, NULL) == SUCCESS) {
            zval_ptr_dtor(&dret);
        }
        if (EG(exception)) {
            zend_clear_exception();
        }
        zval_ptr_dtor(&g_deferred[i]);
        ZVAL_UNDEF(&g_deferred[i]);
    }
    g_ndeferred = 0;
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

    /* Parsed multipart POST fields → $request['post']. */
    zval post;
    array_init(&post);
    for (int i = 0; i < w_npost; i++) {
        add_assoc_string(&post, w_pnames[i], w_pvalues[i] ? w_pvalues[i] : "");
    }
    add_assoc_zval(&request, "post", &post);

    /* Uploaded files (streamed to temp paths) → $request['files']. */
    zval files;
    array_init(&files);
    for (int i = 0; i < w_nfiles; i++) {
        zval f;
        array_init(&f);
        add_assoc_string(&f, "field", w_files[i].field ? w_files[i].field : "");
        add_assoc_string(&f, "name", w_files[i].name ? w_files[i].name : "");
        add_assoc_string(&f, "type", w_files[i].type ? w_files[i].type : "");
        add_assoc_string(&f, "tmp_name", w_files[i].tmp ? w_files[i].tmp : "");
        add_assoc_long(&f, "size", (zend_long)w_files[i].size);
        add_assoc_long(&f, "error", w_files[i].error);
        add_next_index_zval(&files, &f);
    }
    add_assoc_zval(&request, "files", &files);

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

    /* Response is now in Rust's hands (being flushed to the client). Run any
     * deferred work before we block for the next request. */
    if (g_ndeferred > 0) {
        askr_run_deferred();
    }
    RETURN_TRUE;
}

/* ------------------------------------------------------------------ */
/* shared cache bridge (askr_cache_*)                                 */
/* ------------------------------------------------------------------ */

typedef int  (*askr_cache_get_fn)(const char *key, size_t klen, char **out, size_t *out_len);
typedef int  (*askr_cache_set_fn)(const char *key, size_t klen, const char *val, size_t vlen, long ttl);
typedef int  (*askr_cache_add_fn)(const char *key, size_t klen, const char *val, size_t vlen, long ttl);
typedef int  (*askr_cache_del_fn)(const char *key, size_t klen);
typedef long (*askr_cache_incr_fn)(const char *key, size_t klen, long delta, long ttl);
typedef void (*askr_cache_flush_fn)(void);
typedef void (*askr_cache_forget_tag_fn)(const char *tag, size_t tlen);

static askr_cache_get_fn        g_cache_get = NULL;
static askr_cache_set_fn        g_cache_set = NULL;
static askr_cache_add_fn        g_cache_add = NULL;
static askr_cache_del_fn        g_cache_del = NULL;
static askr_cache_incr_fn       g_cache_incr = NULL;
static askr_cache_flush_fn      g_cache_flush = NULL;
static askr_cache_forget_tag_fn g_cache_forget_tag = NULL;

void askr_php_set_cache_bridge(askr_cache_get_fn g, askr_cache_set_fn s, askr_cache_add_fn a,
                               askr_cache_del_fn d, askr_cache_incr_fn i, askr_cache_flush_fn f,
                               askr_cache_forget_tag_fn ft) {
    g_cache_get = g;
    g_cache_set = s;
    g_cache_add = a;
    g_cache_del = d;
    g_cache_incr = i;
    g_cache_flush = f;
    g_cache_forget_tag = ft;
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

/* bool askr_cache_add(string $key, string $value, int $ttl = 0) — atomic
 * set-if-absent; backs Cache::lock(). */
ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_add, 0, 0, 2)
    ZEND_ARG_INFO(0, key)
    ZEND_ARG_INFO(0, value)
    ZEND_ARG_INFO(0, ttl)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_add) {
    char *key, *val; size_t klen, vlen; zend_long ttl = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(key, klen)
        Z_PARAM_STRING(val, vlen)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(ttl)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(g_cache_add ? g_cache_add(key, klen, val, vlen, (long)ttl) : 0);
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

/* void askr_cache_forget_tag(string $tag) — invalidate every cached response
 * carrying $tag, across all workers, instantly. */
ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cache_forget_tag, 0, 0, 1)
    ZEND_ARG_INFO(0, tag)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cache_forget_tag) {
    char *tag; size_t tlen;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(tag, tlen)
    ZEND_PARSE_PARAMETERS_END();
    if (g_cache_forget_tag) g_cache_forget_tag(tag, tlen);
}

/* ------------------------------------------------------------------ */
/* queue bridge (askr_queue_*)                                        */
/* ------------------------------------------------------------------ */

typedef long (*askr_queue_push_fn)(const char *q, size_t qlen, const char *payload, size_t plen, long delay);
typedef int  (*askr_queue_pop_fn)(const char *q, size_t qlen, long visibility,
                                  long *out_id, int *out_attempts, char **out_payload, size_t *out_len);
typedef int  (*askr_queue_delete_fn)(long id);
typedef int  (*askr_queue_release_fn)(long id, long delay);
typedef long (*askr_queue_size_fn)(const char *q, size_t qlen);

static askr_queue_push_fn    g_queue_push = NULL;
static askr_queue_pop_fn     g_queue_pop = NULL;
static askr_queue_delete_fn  g_queue_delete = NULL;
static askr_queue_release_fn g_queue_release = NULL;
static askr_queue_size_fn    g_queue_size = NULL;

void askr_php_set_queue_bridge(askr_queue_push_fn push, askr_queue_pop_fn pop,
                               askr_queue_delete_fn del, askr_queue_release_fn rel,
                               askr_queue_size_fn size) {
    g_queue_push = push;
    g_queue_pop = pop;
    g_queue_delete = del;
    g_queue_release = rel;
    g_queue_size = size;
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_queue_push, 0, 0, 2)
    ZEND_ARG_INFO(0, queue)
    ZEND_ARG_INFO(0, payload)
    ZEND_ARG_INFO(0, delay)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_queue_push) {
    char *q, *payload; size_t qlen, plen; zend_long delay = 0;
    ZEND_PARSE_PARAMETERS_START(2, 3)
        Z_PARAM_STRING(q, qlen)
        Z_PARAM_STRING(payload, plen)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(delay)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG(g_queue_push ? g_queue_push(q, qlen, payload, plen, (long)delay) : 0);
}

/* array|null askr_queue_pop(string $queue, int $visibility = 60) */
ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_queue_pop, 0, 0, 1)
    ZEND_ARG_INFO(0, queue)
    ZEND_ARG_INFO(0, visibility)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_queue_pop) {
    char *q; size_t qlen; zend_long visibility = 60;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_STRING(q, qlen)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(visibility)
    ZEND_PARSE_PARAMETERS_END();
    long id = 0; int attempts = 0; char *payload = NULL; size_t plen = 0;
    if (g_queue_pop && g_queue_pop(q, qlen, (long)visibility, &id, &attempts, &payload, &plen)) {
        array_init(return_value);
        add_assoc_long(return_value, "id", id);
        add_assoc_long(return_value, "attempts", attempts);
        add_assoc_stringl(return_value, "payload", payload ? payload : "", plen);
        free(payload);
    } else {
        RETURN_NULL();
    }
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_queue_delete, 0, 0, 1)
    ZEND_ARG_INFO(0, id)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_queue_delete) {
    zend_long id;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_LONG(id)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(g_queue_delete ? g_queue_delete((long)id) : 0);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_queue_release, 0, 0, 1)
    ZEND_ARG_INFO(0, id)
    ZEND_ARG_INFO(0, delay)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_queue_release) {
    zend_long id, delay = 0;
    ZEND_PARSE_PARAMETERS_START(1, 2)
        Z_PARAM_LONG(id)
        Z_PARAM_OPTIONAL
        Z_PARAM_LONG(delay)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(g_queue_release ? g_queue_release((long)id, (long)delay) : 0);
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_queue_size, 0, 0, 1)
    ZEND_ARG_INFO(0, queue)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_queue_size) {
    char *q; size_t qlen;
    ZEND_PARSE_PARAMETERS_START(1, 1)
        Z_PARAM_STRING(q, qlen)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_LONG(g_queue_size ? g_queue_size(q, qlen) : 0);
}

/* ------------------------------------------------------------------ */
/* broadcast bridge (askr_broadcast)                                  */
/* ------------------------------------------------------------------ */

typedef int (*askr_broadcast_fn)(const char *chan, size_t clen, const char *payload, size_t plen);
static askr_broadcast_fn g_broadcast = NULL;

void askr_php_set_broadcast_bridge(askr_broadcast_fn f) {
    g_broadcast = f;
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_broadcast, 0, 0, 2)
    ZEND_ARG_INFO(0, channel)
    ZEND_ARG_INFO(0, payload)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_broadcast) {
    char *chan, *payload; size_t clen, plen;
    ZEND_PARSE_PARAMETERS_START(2, 2)
        Z_PARAM_STRING(chan, clen)
        Z_PARAM_STRING(payload, plen)
    ZEND_PARSE_PARAMETERS_END();
    RETURN_BOOL(g_broadcast ? g_broadcast(chan, clen, payload, plen) : 0);
}

/* ------------------------------------------------------------------ */
/* CoW template hook (askr_cow_ready)                                 */
/* ------------------------------------------------------------------ */
/* The worker script calls askr_cow_ready() after booting the app, before its
 * serving loop. In the template this forks the workers (and never returns); in
 * each forked worker it sets up that worker's serving bridge and returns, so the
 * worker's while(askr_handle_request) loop runs against the CoW-inherited app. */

typedef int (*askr_cow_ready_fn)(void *ctx);
static askr_cow_ready_fn g_cow_ready = NULL;
static void *g_cow_ctx = NULL;

void askr_php_set_cow(askr_cow_ready_fn f, void *ctx) {
    g_cow_ready = f;
    g_cow_ctx = ctx;
}

/* Point the worker bridge (used by askr_handle_request) at a new context —
 * each forked CoW worker installs its own. */
void askr_php_swap_worker_ctx(void *ctx) {
    g_ctx = ctx;
}

ZEND_BEGIN_ARG_INFO_EX(arginfo_askr_cow_ready, 0, 0, 0)
ZEND_END_ARG_INFO()
static PHP_FUNCTION(askr_cow_ready) {
    if (g_cow_ready) {
        g_cow_ready(g_cow_ctx);
    }
    RETURN_NULL();
}

static const zend_function_entry askr_functions[] = {
    ZEND_FE(askr_handle_request, arginfo_askr_handle_request)
    ZEND_FE(askr_defer, arginfo_askr_defer)
    ZEND_FE(askr_cache_get, arginfo_askr_cache_get)
    ZEND_FE(askr_cache_set, arginfo_askr_cache_set)
    ZEND_FE(askr_cache_add, arginfo_askr_cache_add)
    ZEND_FE(askr_cache_delete, arginfo_askr_cache_delete)
    ZEND_FE(askr_cache_increment, arginfo_askr_cache_increment)
    ZEND_FE(askr_cache_flush, arginfo_askr_cache_flush)
    ZEND_FE(askr_cache_forget_tag, arginfo_askr_cache_forget_tag)
    ZEND_FE(askr_queue_push, arginfo_askr_queue_push)
    ZEND_FE(askr_queue_pop, arginfo_askr_queue_pop)
    ZEND_FE(askr_queue_delete, arginfo_askr_queue_delete)
    ZEND_FE(askr_queue_release, arginfo_askr_queue_release)
    ZEND_FE(askr_queue_size, arginfo_askr_queue_size)
    ZEND_FE(askr_broadcast, arginfo_askr_broadcast)
    ZEND_FE(askr_cow_ready, arginfo_askr_cow_ready)
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

    /* exit()/die() unwind via a bailout (rc == 2) with the real code in
     * EG(exit_status): exit(0) => 0, exit(1) => 1, a fatal => 255. So the exit
     * status is authoritative; rc only matters when the script never ran. */
    int exit_status = EG(exit_status);
    g_script_mode = 0;
    php_request_shutdown((void *)0);

    return rc < 0 ? rc : exit_status;
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
