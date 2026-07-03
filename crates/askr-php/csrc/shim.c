/*
 * Askr PHP embed shim.
 *
 * A thin C layer over PHP's embed SAPI. Its whole job for the M0 spike is to:
 *   1. boot the Zend engine in-process (no FastCGI, no FPM),
 *   2. redirect the SAPI's output writer into a growable buffer so Rust can
 *      capture what PHP emits (this is the seam grove's serve_php() needs),
 *   3. evaluate a snippet / run a script and hand the output back.
 *
 * Everything here runs on a single thread. The interpreter is non-ZTS; one
 * interpreter per process/thread is the model (see PRD 6.1).
 */

#include <php_embed.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>

/* Captured output for the current request/eval. */
static char  *g_buf = NULL;
static size_t g_len = 0;
static size_t g_cap = 0;

static void buf_reset(void) {
    g_len = 0;
    if (g_buf) {
        g_buf[0] = '\0';
    }
}

/* SAPI ub_write override: append everything PHP writes into g_buf. */
static size_t askr_ub_write(const char *str, size_t len) {
    if (g_len + len + 1 > g_cap) {
        size_t ncap = g_cap ? g_cap * 2 : 4096;
        while (ncap < g_len + len + 1) {
            ncap *= 2;
        }
        char *n = (char *)realloc(g_buf, ncap);
        if (!n) {
            return 0;
        }
        g_buf = n;
        g_cap = ncap;
    }
    memcpy(g_buf + g_len, str, len);
    g_len += len;
    g_buf[g_len] = '\0';
    return len;
}

/* Boot the engine. Returns 0 on success. */
int askr_php_startup(void) {
    /* Override the writer *before* init so the embed SAPI adopts it. */
    php_embed_module.ub_write = askr_ub_write;
    return php_embed_init(0, NULL);
}

void askr_php_shutdown(void) {
    php_embed_shutdown();
    free(g_buf);
    g_buf = NULL;
    g_len = 0;
    g_cap = 0;
}

/*
 * Evaluate a PHP code string. Output captured during the call is copied into a
 * freshly malloc'd buffer returned via *out (NUL-terminated, length in *out_len).
 * Caller frees with askr_php_free().
 *
 * Return: 0 ok, -1 PHP eval FAILURE, -2 uncaught bailout/exception.
 */
int askr_php_eval(const char *code, char **out, size_t *out_len) {
    buf_reset();
    int rc = 0;

    zend_first_try {
        if (zend_eval_string((char *)code, NULL, "askr eval") == FAILURE) {
            rc = -1;
        }
    } zend_catch {
        rc = -2;
    } zend_end_try();

    *out_len = g_len;
    *out = (char *)malloc(g_len + 1);
    if (*out) {
        memcpy(*out, g_buf ? g_buf : "", g_len);
        (*out)[g_len] = '\0';
    }
    return rc;
}

void askr_php_free(char *p) {
    free(p);
}
