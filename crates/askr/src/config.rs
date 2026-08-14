//! Typed, declarative configuration (`askr.toml`).
//!
//! A config file is the source of truth a GUI / admin tooling edits. It mirrors
//! the `serve` flags. `askr config check <file>` validates and prints the
//! resolved settings without starting the server.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::server::Config;

/// The on-disk config file (`askr.toml`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub worker: WorkerSection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub acme: AcmeSection,
    #[serde(default)]
    pub admin: AdminSection,
    #[serde(default)]
    pub queue: QueueSection,
    #[serde(default)]
    pub scheduler: SchedulerSection,
    #[serde(default)]
    pub cache: CacheSection,
    #[serde(default)]
    pub broadcast: BroadcastSection,
    #[serde(default)]
    pub reload: ReloadSection,
    #[serde(default)]
    pub record: RecordSection,
    #[serde(default)]
    pub pusher: PusherSection,
    /// Arbitrary supervised external commands: `[[sidecar]] command = "…"`.
    #[serde(default)]
    pub sidecar: Vec<SidecarSpec>,
    /// Host redirects: `[[redirect]] from = "www.x.no" to = "https://x.no"`.
    #[serde(default)]
    pub redirect: Vec<RedirectRule>,
    /// Virtual hosts: `[[site]] hosts = [...] root = "…"` — route by Host header.
    #[serde(default)]
    pub site: Vec<SiteSpec>,
    /// Rate limits: `[[ratelimit]] path = "/api/*" limit = 60`.
    #[serde(default)]
    pub ratelimit: Vec<RateLimitRule>,
}

/// A virtual host: one or more `hosts` (exact or `*.suffix`) served from `root`
/// with its own `front` controller. Full dynamic dispatch requires per-request
/// mode; in worker mode statics are per-site but the booted app is fixed.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SiteSpec {
    pub hosts: Vec<String>,
    pub root: PathBuf,
    #[serde(default = "default_front")]
    pub front: String,
}

/// A declarative host redirect (e.g. `www.domene.no` → `https://domene.no`). The
/// request path + query are preserved; `status` defaults to 308 (permanent, keeps
/// the method). `from` matches the Host header exactly or as a `*.suffix` glob.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RedirectRule {
    pub from: String,
    pub to: String,
    #[serde(default = "default_redirect_status")]
    pub status: u16,
}

fn default_redirect_status() -> u16 {
    308
}

/// A declarative response-cache rule: `[[cache.rule]]`.
///
/// Rules let an operator set cache policy per path **without touching the app** — the
/// one thing VCL is genuinely needed for once redirects, cache keys, PURGE/BAN,
/// stale-if-error and ESI are all first-class elsewhere in Askr.
///
/// Patterns are globs (`*`, `?`), not regexes: rules are evaluated on the request hot
/// path, and a regex engine has no business there. A regex-looking pattern is
/// rejected at config load, so you find out at startup (or from `askr config-check`)
/// rather than from a rule that silently never matches.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRule {
    /// Path glob, e.g. `/admin/*`. Matched against the request path (no query).
    pub path: String,
    /// `"pass"` — never cache this path, even if the app sent an `Askr-Cache` header.
    #[serde(default)]
    pub action: Option<String>,
    /// Fresh seconds. Set this to cache a path the app never opted in to; it also
    /// overrides the app's `Askr-Cache` TTL for matching paths.
    #[serde(default)]
    pub ttl: Option<u64>,
    /// Stale-while-revalidate window (seconds past `ttl`).
    #[serde(default)]
    pub swr: u64,
    /// `stale-if-error` window (seconds past `ttl`).
    #[serde(default)]
    pub stale_if_error: u64,
    /// Cache this path **even when the request carries cookies**.
    ///
    /// This is the dangerous one, exactly as in Varnish: if the path can render
    /// anything user-specific, one visitor's page is then served to everyone. Only
    /// use it on paths you know are identical for all visitors.
    #[serde(default)]
    pub force: bool,
}

impl CacheRule {
    /// Does this rule bypass the cache entirely?
    pub fn is_pass(&self) -> bool {
        self.action.as_deref() == Some("pass")
    }
}

/// A rate-limit rule: `[[ratelimit]]`.
///
/// Enforced in the Rust layer before PHP is woken, with token buckets in shared
/// memory — so the limit applies across the whole worker fleet, not per process.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitRule {
    /// Path glob, e.g. `/api/*`. Matched against the request path (no query).
    pub path: String,
    /// Requests allowed per `window`.
    pub limit: u64,
    /// Window length in seconds.
    #[serde(default = "default_rl_window")]
    pub window: u64,
    /// Identity to count by: `ip`, `header:<Name>`, or `cookie:<name>`.
    #[serde(default = "default_rl_by")]
    pub by: String,
    /// Extra tokens a bursty client may accumulate on top of `limit`.
    #[serde(default)]
    pub burst: u64,
}

fn default_rl_window() -> u64 {
    60
}

fn default_rl_by() -> String {
    "ip".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarSpec {
    /// The command to run (via `sh -c`), e.g. "node bootstrap/ssr/ssr.mjs".
    pub command: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Address to listen on, e.g. "0.0.0.0:8000".
    pub listen: String,
    /// Document root (the app's public/ directory).
    pub root: PathBuf,
    /// Front controller, relative to the root.
    #[serde(default = "default_front")]
    pub front: String,
    /// Worker processes: a number, or "auto" (= CPU cores).
    #[serde(default = "default_workers")]
    pub workers: String,
    /// CoW autoscaling floor (minimum web workers). Defaults to `workers`.
    #[serde(default)]
    pub workers_min: Option<usize>,
    /// CoW autoscaling ceiling. When greater than `workers_min`, the CoW
    /// template scales the pool on live queue depth. Defaults to `workers`.
    #[serde(default)]
    pub workers_max: Option<usize>,
    /// Recycle each worker after this many requests (0 = never).
    #[serde(default)]
    pub max_requests: usize,
    /// Recycle a worker gracefully once its RSS exceeds this many MB (0 = off).
    #[serde(default)]
    pub max_rss: usize,
    /// Traffic shadowing: mirror sampled safe requests to this upstream URL.
    #[serde(default)]
    pub shadow_to: Option<String>,
    /// Percent (1..=100) of eligible requests to mirror.
    #[serde(default = "default_shadow_sample")]
    pub shadow_sample: u8,
    /// Max request body size, e.g. "16M".
    #[serde(default = "default_body")]
    pub max_body_size: String,
    /// Mark requests as HTTPS in $_SERVER (e.g. behind a TLS terminator).
    #[serde(default)]
    pub https: bool,
    /// Redirect plain-HTTP requests to HTTPS (308). Uses the connection's TLS
    /// state, `https`, or an `X-Forwarded-Proto` header to decide.
    #[serde(default)]
    pub force_https: bool,
    /// Answer plain HTTP here and 308 it to HTTPS (e.g. "0.0.0.0:80"). Needed because
    /// a TLS listener never sees a plain-HTTP request, so `force_https` has nothing to
    /// act on when Askr terminates TLS itself. Automatic on the ACME challenge address
    /// when `--acme` is used.
    #[serde(default)]
    pub http_redirect: Option<std::net::SocketAddr>,
    /// One JSONL line per PHP-served request, for `askr cache-report` to analyse.
    /// A diagnostic: turn it on for an hour, then turn it off.
    #[serde(default)]
    pub traffic_log: Option<PathBuf>,
    /// Proxies whose `X-Forwarded-For` may be believed, as IPs or CIDRs
    /// (`10.0.0.0/8`). Without this, a forwarded header is ignored — otherwise
    /// anyone could rotate a fake client IP and walk straight past a rate limit.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Structured (JSON) access log destination: a file path, or "-" for stdout.
    pub access_log: Option<PathBuf>,
    /// Serve HTTP/3 (QUIC) on the TLS port (requires TLS; build with `http3`).
    #[serde(default)]
    pub http3: bool,
    /// Seconds a client may take to complete the TLS handshake (slowloris guard).
    #[serde(default = "default_handshake_timeout")]
    pub tls_handshake_timeout: u64,
    /// Seconds a client may take to send the full request headers (slowloris guard).
    #[serde(default = "default_header_read_timeout")]
    pub header_read_timeout: u64,
    /// Harden workers on Linux (seccomp no-exec).
    #[serde(default)]
    pub sandbox: bool,
    /// Landlock-writable paths (enables the filesystem restriction).
    #[serde(default)]
    pub sandbox_write: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSection {
    /// Worker script — boot the app once and serve many (Octane model).
    pub script: Option<PathBuf>,
    /// Application base path, exported as $ASKR_APP_BASE for the worker script.
    pub app_base: Option<PathBuf>,
    /// Extra php.ini lines (e.g. to load opcache).
    pub ini: Option<String>,
    /// Dev only: detect state bleed between requests (expensive; worker mode).
    #[serde(default)]
    pub paranoid: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    #[serde(default)]
    pub self_signed: bool,
}

/// `[acme]` — auto-TLS from a config file.
///
/// ACME used to be reachable only through CLI flags, and since `--config` is the whole
/// configuration rather than a set of defaults, auto-TLS and a config file were mutually
/// exclusive. That made real combinations unreachable: `trusted_proxies` is file-only, so
/// "auto-TLS behind a proxy" could not be expressed at all. Every flag has a twin here.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeSection {
    /// Obtain and renew a certificate over HTTP-01.
    #[serde(default)]
    pub enabled: bool,
    /// Domain(s) to certify. At least one is required when `enabled`.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Contact email for the ACME account.
    pub email: Option<String>,
    /// Where to cache the account key and certificate.
    pub dir: Option<PathBuf>,
    /// Use Let's Encrypt staging — untrusted certs, far higher rate limits. Worth doing
    /// first: the production limits are per-domain and per-week.
    #[serde(default)]
    pub staging: bool,
    /// Custom directory URL (a Pebble test server, say). Distinct from `dir`.
    pub directory_url: Option<String>,
    /// Address to answer HTTP-01 challenges on, and to redirect from when
    /// `server.force_https` is set. Defaults to 0.0.0.0:80.
    pub http: Option<String>,
    /// Extra CA root to trust for the directory (testing only).
    pub ca_root: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminSection {
    /// Admin dashboard/API listen address (e.g. "127.0.0.1:9000"). Off if unset.
    pub listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueSection {
    /// Number of queue-worker processes (runs the queue script). 0 = off.
    /// With `workers_max`, this is the floor of an autoscaling range.
    #[serde(default)]
    pub workers: usize,
    /// Autoscaling ceiling for queue workers (backlog-driven). Defaults to
    /// `workers` (fixed count).
    #[serde(default)]
    pub workers_max: Option<usize>,
    /// Queue runner script (e.g. examples/askr-queue.php).
    pub script: Option<PathBuf>,
    /// Shared-memory job queue slots (0 = off; 32 KB each). Enables askr_queue_*
    /// and the Redis-free AskrQueue driver.
    #[serde(default)]
    pub slots: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerSection {
    /// Scheduler runner script (e.g. examples/askr-scheduler.php). Off if unset.
    pub script: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSection {
    /// Shared kv cache slots (0 = disabled). Each slot is ~4.3 KB.
    #[serde(default)]
    pub slots: usize,
    /// Large-value region slots (64 KB each; 0 = off). Enables cache values over
    /// 4 KB — Laravel sessions, cached fragments/collections.
    #[serde(default)]
    pub large_slots: usize,
    /// Response cache slots (0 = disabled). Full-response edge cache with tag
    /// invalidation (`Askr-Cache` header + `askr_cache_forget_tag`). ~140 KB each.
    #[serde(default)]
    pub response_slots: usize,
    /// Query parameters ignored when building the response-cache key. Trailing
    /// `*` globs (`utm_*`). Tracking params otherwise fragment the cache into a
    /// separate entry per visitor.
    #[serde(default)]
    pub strip_query_params: Vec<String>,
    /// Cookies that do *not* make a request non-cacheable (analytics cookies
    /// like `_ga`). A request whose cookies are all ignorable is still treated
    /// as anonymous. Trailing `*` globs supported.
    #[serde(default)]
    pub ignore_cookies: Vec<String>,
    /// Split the response-cache key on mobile vs desktop `User-Agent`, for apps
    /// that render different HTML per device.
    #[serde(default)]
    pub vary_user_agent: bool,
    /// Saint mode: after PHP returns 5xx (or a worker dies), treat the backend as
    /// unhealthy for this many seconds — requests holding a `stale-if-error` entry
    /// are then served from cache without running PHP. 0 = off (default).
    #[serde(default)]
    pub saint_seconds: u64,
    /// Declarative per-path cache policy: `[[cache.rule]]`. First match wins.
    #[serde(default)]
    pub rule: Vec<CacheRule>,
    /// Persist the response cache to this file on graceful shutdown and load it
    /// again at boot, so a restart doesn't start cold. Unset = off.
    #[serde(default)]
    pub persist: Option<PathBuf>,
    /// Optional release identifier. When set, a dump only loads if it matches —
    /// set it to your release SHA so a deploy can't resurrect pre-deploy HTML.
    #[serde(default)]
    pub persist_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BroadcastSection {
    /// Enable the broadcast ring + SSE endpoint (askr_broadcast()).
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadSection {
    /// Canary reload: roll one worker and health-check it before the rest.
    #[serde(default)]
    pub canary: bool,
    /// Seconds the canary must survive before the rest of the fleet is rolled.
    #[serde(default = "default_canary_window")]
    pub canary_window: u64,
    /// Requests the canary must serve before its numbers mean anything. Below
    /// this the rollout is "inconclusive" and continues — with a warning.
    #[serde(default = "default_canary_min_requests")]
    pub canary_min_requests: u64,
    /// Percentage points of error rate the canary may exceed the fleet by.
    #[serde(default = "default_canary_max_error_rate")]
    pub canary_max_error_rate: f64,
    /// Mean-latency factor the canary may exceed the fleet by (3.0 = 3×).
    #[serde(default = "default_canary_max_latency_factor")]
    pub canary_max_latency_factor: f64,
}

fn default_canary_window() -> u64 {
    5
}
fn default_canary_min_requests() -> u64 {
    20
}
fn default_canary_max_error_rate() -> f64 {
    2.0
}
fn default_canary_max_latency_factor() -> f64 {
    3.0
}

// Hand-written so an absent `[reload]` section gets the same values as the serde
// defaults above. `#[derive(Default)]` would zero them, which would mean "abort on
// any canary error at all" — a booby trap for anyone who never writes a [reload]
// section.
impl Default for ReloadSection {
    fn default() -> Self {
        ReloadSection {
            canary: false,
            canary_window: default_canary_window(),
            canary_min_requests: default_canary_min_requests(),
            canary_max_error_rate: default_canary_max_error_rate(),
            canary_max_latency_factor: default_canary_max_latency_factor(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordSection {
    /// Record failing (5xx) requests into this directory for `askr replay`.
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PusherSection {
    /// Pusher-compatible WebSocket + HTTP trigger (drop-in Reverb). Rides the
    /// broadcast ring, which is auto-enabled.
    #[serde(default)]
    pub enabled: bool,
    /// App secret for verifying private/presence subscription auth. Omit to
    /// accept them without a signature (dev).
    pub secret: Option<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection {
            http_redirect: None,
            listen: "127.0.0.1:8000".into(),
            root: PathBuf::from("public"),
            front: default_front(),
            traffic_log: None,
            trusted_proxies: Vec::new(),
            workers: default_workers(),
            workers_min: None,
            workers_max: None,
            max_requests: 0,
            max_rss: 0,
            shadow_to: None,
            shadow_sample: default_shadow_sample(),
            max_body_size: default_body(),
            https: false,
            force_https: false,
            access_log: None,
            http3: false,
            tls_handshake_timeout: default_handshake_timeout(),
            header_read_timeout: default_header_read_timeout(),
            sandbox: false,
            sandbox_write: Vec::new(),
        }
    }
}

fn default_front() -> String {
    "index.php".into()
}
fn default_workers() -> String {
    "auto".into()
}
fn default_body() -> String {
    "16M".into()
}

fn default_shadow_sample() -> u8 {
    100
}

fn default_handshake_timeout() -> u64 {
    10
}

fn default_header_read_timeout() -> u64 {
    15
}

/// The fully-resolved runtime configuration produced from a file.
pub struct Resolved {
    pub config: Config,
    pub workers: usize,
    pub workers_min: usize,
    pub workers_max: usize,
    pub ini: Option<String>,
    pub app_base: Option<PathBuf>,
    pub paranoid: bool,
    pub admin_listen: Option<SocketAddr>,
    /// Auto-TLS from `[acme]`. See [`AcmeSection`] for why these belong in the file.
    pub acme: bool,
    pub acme_domains: Vec<String>,
    pub acme_email: Option<String>,
    /// `None` means "use the CLI default", resolved by the caller so the default value
    /// lives in exactly one place.
    pub acme_dir: Option<PathBuf>,
    pub acme_staging: bool,
    pub acme_directory: Option<String>,
    /// `None` means 0.0.0.0:80, likewise resolved by the caller.
    pub acme_http: Option<SocketAddr>,
    pub acme_ca_root: Option<PathBuf>,
    pub queue_workers: usize,
    pub queue_workers_max: usize,
    pub queue_script: Option<PathBuf>,
    pub queue_slots: usize,
    pub scheduler_script: Option<PathBuf>,
    pub sidecars: Vec<String>,
    pub cache_slots: usize,
    pub cache_large_slots: usize,
    pub response_cache_slots: usize,
    pub cache_persist: Option<PathBuf>,
    pub cache_persist_key: Option<String>,
    pub broadcast: bool,
    pub canary_reload: bool,
    pub canary_window: u64,
    pub canary_min_requests: u64,
    pub canary_max_error_rate: f64,
    pub canary_max_latency_factor: f64,
}

impl FileConfig {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: FileConfig =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    /// Validate and resolve into a runtime [`Config`], checking that paths and
    /// certificates actually exist.
    pub fn resolve(self, cpus: usize) -> Result<Resolved> {
        let listen: SocketAddr = self
            .server
            .listen
            .parse()
            .with_context(|| format!("invalid server.listen {:?}", self.server.listen))?;

        let docroot = std::fs::canonicalize(&self.server.root)
            .with_context(|| format!("server.root {} not found", self.server.root.display()))?;

        let front = PathBuf::from(&self.server.front);
        anyhow::ensure!(
            docroot.join(&front).is_file(),
            "front controller not found: {}",
            docroot.join(&front).display()
        );

        // Resolve [[site]] virtual hosts (each with its own docroot + front
        // controller). Host-routed per request; full dynamic dispatch is
        // per-request mode — in worker mode the booted app is fixed (statics are
        // still served per site).
        let mut sites = Vec::new();
        for s in &self.site {
            let sroot = std::fs::canonicalize(&s.root)
                .with_context(|| format!("site root {} not found", s.root.display()))?;
            let sfront = PathBuf::from(&s.front);
            anyhow::ensure!(
                sroot.join(&sfront).is_file(),
                "site front controller not found: {}",
                sroot.join(&sfront).display()
            );
            anyhow::ensure!(!s.hosts.is_empty(), "each [[site]] needs at least one host");
            sites.push(crate::server::Site {
                hosts: s.hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
                docroot: sroot,
                front_controller: sfront,
            });
        }

        // Cache rules: validate at load so a typo fails at startup (and under
        // `askr config-check`) instead of becoming a rule that never matches.
        for r in &self.cache.rule {
            anyhow::ensure!(
                !r.path.trim().is_empty(),
                "[[cache.rule]] needs a path glob, e.g. path = \"/admin/*\""
            );
            // Check regex-shaped input first: its message is the useful one.
            anyhow::ensure!(
                !(r.path.starts_with('^') || r.path.contains(".*") || r.path.ends_with('$')),
                "[[cache.rule]] path is a glob, not a regex — use \"/admin/*\" instead of \"^/admin/.*\": {}",
                r.path
            );
            anyhow::ensure!(
                r.path.starts_with('/'),
                "[[cache.rule]] path must start with '/': {}",
                r.path
            );
            if let Some(a) = &r.action {
                anyhow::ensure!(
                    a == "pass",
                    "[[cache.rule]] unknown action \"{a}\" — the only action is \"pass\""
                );
            }
            anyhow::ensure!(
                r.is_pass() || r.ttl.is_some(),
                "[[cache.rule]] for {} needs either action = \"pass\" or a ttl",
                r.path
            );
            anyhow::ensure!(
                !(r.is_pass() && r.ttl.is_some()),
                "[[cache.rule]] for {} sets both action = \"pass\" and a ttl",
                r.path
            );
        }

        // Rate-limit rules: same fail-at-load discipline as [[cache.rule]].
        for r in &self.ratelimit {
            anyhow::ensure!(
                !(r.path.starts_with('^') || r.path.contains(".*") || r.path.ends_with('$')),
                "[[ratelimit]] path is a glob, not a regex — use \"/api/*\" instead of \"^/api/.*\": {}",
                r.path
            );
            anyhow::ensure!(
                r.path.starts_with('/'),
                "[[ratelimit]] path must start with '/': {}",
                r.path
            );
            anyhow::ensure!(
                r.limit > 0,
                "[[ratelimit]] for {} needs limit > 0 (remove the rule to disable it)",
                r.path
            );
            anyhow::ensure!(
                r.window > 0,
                "[[ratelimit]] for {} needs window > 0",
                r.path
            );
            let by_ok = r.by == "ip"
                || r.by
                    .strip_prefix("header:")
                    .is_some_and(|n| !n.trim().is_empty())
                || r.by
                    .strip_prefix("cookie:")
                    .is_some_and(|n| !n.trim().is_empty());
            anyhow::ensure!(
                by_ok,
                "[[ratelimit]] unknown `by` value \"{}\" — use \"ip\", \"header:X-Api-Key\" or \"cookie:session\"",
                r.by
            );
        }
        for p in &self.server.trusted_proxies {
            anyhow::ensure!(
                crate::server::parse_cidr(p).is_some(),
                "server.trusted_proxies entry is not an IP or CIDR: {p}"
            );
        }
        if !self.ratelimit.is_empty() && self.server.trusted_proxies.is_empty() {
            tracing::warn!(
                "rate limiting is on but server.trusted_proxies is empty — X-Forwarded-For is \
                 ignored and limits count the peer address. Set trusted_proxies if Askr runs \
                 behind a load balancer, or every client will share one bucket."
            );
        }

        let workers = match self.server.workers.as_str() {
            "auto" => cpus.max(1),
            n => n
                .parse::<usize>()
                .with_context(|| format!("invalid server.workers {:?}", self.server.workers))?
                .max(1),
        };

        let max_body_size = crate::parse_size(&self.server.max_body_size)?;

        if let Some(script) = &self.worker.script {
            anyhow::ensure!(
                script.is_file(),
                "worker.script not found: {}",
                script.display()
            );
        }
        if let Some(base) = &self.worker.app_base {
            anyhow::ensure!(
                base.is_dir(),
                "worker.app_base not found: {}",
                base.display()
            );
        }

        // TLS validation.
        let tls_self_signed = self.tls.self_signed;
        match (&self.tls.cert, &self.tls.key) {
            (Some(c), Some(k)) => {
                anyhow::ensure!(c.is_file(), "tls.cert not found: {}", c.display());
                anyhow::ensure!(k.is_file(), "tls.key not found: {}", k.display());
                anyhow::ensure!(
                    !tls_self_signed,
                    "set either tls.self_signed or tls.cert/key, not both"
                );
            }
            (None, None) => {}
            _ => anyhow::bail!("tls.cert and tls.key must both be set"),
        }
        // A certificate from a file, as opposed to one ACME will fetch. The two are
        // mutually exclusive, so this has to be a separate value from `tls_on` below.
        let static_tls = self.tls.cert.is_some() || tls_self_signed;

        // ACME validation. The failures here are all things that would otherwise surface
        // as a rate-limited rejection from Let's Encrypt minutes later.
        let acme = self.acme;
        if acme.enabled {
            anyhow::ensure!(
                !acme.domains.is_empty(),
                "acme.enabled needs at least one entry in acme.domains"
            );
            anyhow::ensure!(
                !static_tls,
                "acme.enabled obtains its own certificate, so tls.cert/key and \
                 tls.self_signed must be unset"
            );
            for d in &acme.domains {
                anyhow::ensure!(
                    !d.contains('/') && !d.contains(':') && !d.starts_with('*'),
                    "acme.domains entry {d:?} must be a bare hostname — no scheme, port \
                     or wildcard (HTTP-01 cannot validate a wildcard)"
                );
            }
            if let Some(r) = &acme.ca_root {
                anyhow::ensure!(r.is_file(), "acme.ca_root not found: {}", r.display());
            }
        } else {
            anyhow::ensure!(
                acme.domains.is_empty() && acme.email.is_none(),
                "acme.domains/acme.email are set but acme.enabled is false — set \
                 acme.enabled = true, or remove them (a half-configured section that \
                 silently does nothing is how a site ends up serving plain HTTP)"
            );
        }
        // ACME counts as TLS: the resolved config has to say the server will speak HTTPS,
        // even though the certificate doesn't exist yet. Otherwise anything reading it
        // before the ACME step runs — logging, admin status — reports plain HTTP.
        let tls_on = static_tls || acme.enabled;

        let acme_http = match &acme.http {
            Some(a) => Some(
                a.parse::<SocketAddr>()
                    .with_context(|| format!("invalid acme.http {a:?}"))?,
            ),
            None => None,
        };

        let admin_listen = match &self.admin.listen {
            Some(a) => Some(
                a.parse::<SocketAddr>()
                    .with_context(|| format!("invalid admin.listen {a:?}"))?,
            ),
            None => None,
        };

        // Queue / scheduler sidecars.
        if self.queue.workers > 0 {
            anyhow::ensure!(
                self.queue.script.is_some(),
                "queue.workers is set but queue.script is missing"
            );
            // The symmetric check, which was missing and cost a production outage: with
            // workers but no slots the ring is never mapped, so askr_queue_push() returns
            // 0, Laravel does not check the return, and every queued job — password
            // resets, invitations, all outgoing mail — is discarded without an exception,
            // a log line, or anything in the queue to age. Workers polling a ring that
            // does not exist is as useless as a ring nobody polls, and quieter.
            anyhow::ensure!(
                self.queue.slots > 0,
                "queue.workers is set but queue.slots is 0 — the shared-memory ring would \
                 never be mapped, so every queued job would be silently discarded. Set \
                 queue.slots (8192 is a reasonable start; ~32 KB per slot)."
            );
        }
        // Also the other way round, as a warning rather than an error: slots with no worker
        // is a legitimate configuration (jobs pushed here, consumed by a worker elsewhere),
        // but it is far more often a mistake — and it was, on the deployment that taught us
        // this. The backlog watchdog will name the queue once jobs start ageing.
        if self.queue.slots > 0 && self.queue.workers == 0 && self.queue.script.is_none() {
            tracing::warn!(
                "queue.slots is set but no queue worker is configured (queue.workers + \
                 queue.script). Jobs will be accepted and never processed unless something \
                 outside this instance consumes them."
            );
        }
        if let Some(s) = &self.queue.script {
            anyhow::ensure!(s.is_file(), "queue.script not found: {}", s.display());
        }
        if let Some(s) = &self.scheduler.script {
            anyhow::ensure!(s.is_file(), "scheduler.script not found: {}", s.display());
        }
        let queue_workers = if self.queue.script.is_some() {
            self.queue.workers
        } else {
            0
        };
        let queue_workers_max = self
            .queue
            .workers_max
            .unwrap_or(queue_workers)
            .max(queue_workers);

        Ok(Resolved {
            config: Config {
                docroot,
                front_controller: front,
                listen,
                https: self.server.https || tls_on,
                worker_script: self.worker.script,
                max_requests: self.server.max_requests,
                max_rss_mb: self.server.max_rss,
                tls_cert: self.tls.cert,
                tls_key: self.tls.key,
                tls_self_signed,
                max_body_size,
                record_dir: self.record.dir,
                pusher: self.pusher.enabled,
                pusher_secret: self.pusher.secret,
                access_log: self.server.access_log,
                traffic_log: self.server.traffic_log,
                sandbox: self.server.sandbox || !self.server.sandbox_write.is_empty(),
                sandbox_write: self.server.sandbox_write,
                shadow_to: self.server.shadow_to,
                shadow_sample: self.server.shadow_sample,
                http3: self.server.http3,
                tls_handshake_timeout: self.server.tls_handshake_timeout,
                header_read_timeout: self.server.header_read_timeout,
                force_https: self.server.force_https,
                http_redirect: self.server.http_redirect,
                redirects: self.redirect.clone(),
                sites,
                cache_strip_query: self.cache.strip_query_params.clone(),
                cache_ignore_cookies: self.cache.ignore_cookies.clone(),
                cache_vary_user_agent: self.cache.vary_user_agent,
                cache_saint_seconds: self.cache.saint_seconds,
                cache_rules: self.cache.rule.clone(),
                ratelimits: self.ratelimit.clone(),
                trusted_proxies: self
                    .server
                    .trusted_proxies
                    .iter()
                    .filter_map(|p| crate::server::parse_cidr(p))
                    .collect(),
            },
            workers,
            workers_min: self.server.workers_min.unwrap_or(workers).max(1),
            workers_max: self
                .server
                .workers_max
                .unwrap_or(workers)
                .max(self.server.workers_min.unwrap_or(workers).max(1)),
            ini: self.worker.ini,
            app_base: self.worker.app_base,
            paranoid: self.worker.paranoid,
            admin_listen,
            acme: acme.enabled,
            acme_domains: acme.domains,
            acme_email: acme.email,
            acme_dir: acme.dir,
            acme_staging: acme.staging,
            acme_directory: acme.directory_url,
            acme_http,
            acme_ca_root: acme.ca_root,
            queue_workers,
            queue_workers_max,
            queue_script: self.queue.script,
            queue_slots: self.queue.slots,
            scheduler_script: self.scheduler.script,
            sidecars: self.sidecar.into_iter().map(|s| s.command).collect(),
            cache_slots: self.cache.slots,
            cache_large_slots: self.cache.large_slots,
            response_cache_slots: self.cache.response_slots,
            cache_persist: self.cache.persist.clone(),
            cache_persist_key: self.cache.persist_key.clone(),
            broadcast: self.broadcast.enabled,
            canary_reload: self.reload.canary,
            canary_window: self.reload.canary_window.max(1),
            canary_min_requests: self.reload.canary_min_requests,
            canary_max_error_rate: self.reload.canary_max_error_rate.max(0.0),
            canary_max_latency_factor: self.reload.canary_max_latency_factor.max(1.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal document root so `resolve()` can canonicalise and find a front
    /// controller — validation is what's under test, not the filesystem.
    fn app_dir(name: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("askr-cfg-{name}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.php"), "<?php\n").unwrap();
        dir
    }

    /// Resolve a config body with `{ROOT}` pointing at a throwaway app.
    ///
    /// `listen` is injected when absent so each test shows only the keys it's
    /// actually about.
    fn resolve(name: &str, body: &str) -> Result<Resolved> {
        let dir = app_dir(name);
        let mut text = body.replace("{ROOT}", dir.to_str().unwrap());
        if !text.contains("listen") {
            text = text.replace("[server]", "[server]\nlisten = \"127.0.0.1:8000\"");
        }
        let cfg: FileConfig = toml::from_str(&text)?;
        let out = cfg.resolve(4);
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn err(name: &str, body: &str) -> String {
        match resolve(name, body) {
            Ok(_) => panic!("expected {name} to be rejected, but it resolved"),
            Err(e) => format!("{e:#}"),
        }
    }

    const MINIMAL: &str = r#"
[server]
root = "{ROOT}"
"#;

    /// `[acme]` exists so auto-TLS and a config file aren't mutually exclusive. Before
    /// 1.4.10, ACME was CLI-only while `trusted_proxies` was file-only, which made
    /// "auto-TLS behind a proxy" impossible to express at all.
    #[test]
    fn acme_section_resolves_and_defaults_are_left_to_the_caller() {
        let r = resolve(
            "acme-ok",
            r#"
[server]
root = "{ROOT}"
force_https = true
trusted_proxies = ["172.17.0.1"]

[acme]
enabled = true
domains = ["example.com", "www.example.com"]
email = "admin@example.com"
staging = true
"#,
        )
        .expect("an acme section should resolve");
        assert!(r.acme);
        assert_eq!(r.acme_domains, ["example.com", "www.example.com"]);
        assert!(r.acme_staging);
        assert!(
            r.acme_dir.is_none() && r.acme_http.is_none(),
            "absent keys stay None so the CLI's defaults are applied in one place"
        );
        assert!(
            r.config.https,
            "acme implies https, or workers would serve the cert over plain HTTP"
        );
    }

    #[test]
    fn acme_without_domains_is_refused() {
        let e = err(
            "acme-nodomains",
            r#"
[server]
root = "{ROOT}"

[acme]
enabled = true
"#,
        );
        assert!(e.contains("acme.domains"), "{e}");
    }

    /// The dangerous shape: keys present, `enabled` absent. TOML defaults it to false, so
    /// the site would quietly serve plain HTTP while the file looks like it asked for TLS.
    #[test]
    fn a_half_configured_acme_section_is_refused() {
        let e = err(
            "acme-half",
            r#"
[server]
root = "{ROOT}"

[acme]
domains = ["example.com"]
"#,
        );
        assert!(e.contains("acme.enabled"), "{e}");
    }

    #[test]
    fn acme_alongside_a_static_certificate_is_refused() {
        let e = err(
            "acme-and-tls",
            r#"
[server]
root = "{ROOT}"

[tls]
self_signed = true

[acme]
enabled = true
domains = ["example.com"]
"#,
        );
        assert!(e.contains("tls.cert") || e.contains("self_signed"), "{e}");
    }

    /// HTTP-01 validates a single hostname; a wildcard needs DNS-01, which Askr doesn't
    /// do. Better to say so now than to be rejected by Let's Encrypt after a
    /// rate-limited round trip.
    #[test]
    fn a_wildcard_or_url_domain_is_refused() {
        for bad in ["*.example.com", "https://example.com", "example.com:443"] {
            let body = format!(
                "\n[server]\nroot = \"{{ROOT}}\"\n\n[acme]\nenabled = true\ndomains = [\"{bad}\"]\n"
            );
            let e = err("acme-baddomain", &body);
            assert!(e.contains("bare hostname"), "{bad}: {e}");
        }
    }

    /// The configuration that dropped every outgoing mail on a live site: queue workers
    /// running, no slots, so the ring was never mapped and each push returned 0 into a
    /// framework that does not check the return value. Nothing failed. Nothing was logged.
    /// The mail simply never went.
    #[test]
    fn queue_workers_without_slots_are_refused() {
        let e = err(
            "queue-noslots",
            r#"
[server]
root = "{ROOT}"

[queue]
workers = 4
script = "{ROOT}/index.php"
"#,
        );
        assert!(e.contains("queue.slots"), "{e}");
        assert!(
            e.contains("silently discarded"),
            "the error must say what happens, not just which key is missing: {e}"
        );
    }

    /// The mirror image is allowed — jobs may be consumed by a worker outside this
    /// instance — but it warns, because far more often it is the same mistake from the
    /// other side.
    #[test]
    fn queue_slots_without_a_worker_still_resolves() {
        let r = resolve(
            "queue-noworker",
            r#"
[server]
root = "{ROOT}"

[queue]
slots = 64
"#,
        )
        .expect("slots without a worker is legal");
        assert_eq!(r.queue_workers, 0);
        assert_eq!(r.queue_slots, 64);
    }

    #[test]
    fn minimal_config_resolves() {
        let r = resolve("minimal", MINIMAL).expect("minimal config should resolve");
        assert_eq!(r.workers, 4);
        assert!(r.cache_persist.is_none());
    }

    /// An absent `[reload]` section must still get the documented defaults.
    ///
    /// This is a regression test for a booby trap: with a derived `Default`, the
    /// canary thresholds would have been zeroed, which means "abort the rollout on
    /// any canary error at all" for everyone who never writes a `[reload]` section.
    #[test]
    fn absent_reload_section_keeps_documented_defaults() {
        let r = resolve("reload-default", MINIMAL).unwrap();
        assert!(!r.canary_reload, "canary is opt-in");
        assert_eq!(r.canary_window, 5);
        assert_eq!(r.canary_min_requests, 20);
        assert_eq!(r.canary_max_error_rate, 2.0, "must not default to zero");
        assert_eq!(r.canary_max_latency_factor, 3.0);
    }

    #[test]
    fn cache_rules_are_validated_at_load() {
        // Globs, not regexes — and the message should say so.
        let e = err(
            "rule-regex",
            r#"
[server]
root = "{ROOT}"
[[cache.rule]]
path = "^/admin/.*"
action = "pass"
"#,
        );
        assert!(e.contains("glob, not a regex"), "got: {e}");

        assert!(err(
            "rule-action",
            r#"
[server]
root = "{ROOT}"
[[cache.rule]]
path = "/admin/*"
action = "lookup"
"#,
        )
        .contains("unknown action"));

        assert!(err(
            "rule-empty",
            r#"
[server]
root = "{ROOT}"
[[cache.rule]]
path = "/admin/*"
"#,
        )
        .contains("action = \"pass\" or a ttl"));

        assert!(err(
            "rule-both",
            r#"
[server]
root = "{ROOT}"
[[cache.rule]]
path = "/admin/*"
action = "pass"
ttl = 60
"#,
        )
        .contains("both"));

        assert!(err(
            "rule-slash",
            r#"
[server]
root = "{ROOT}"
[[cache.rule]]
path = "admin/*"
ttl = 60
"#,
        )
        .contains("must start with '/'"));

        // A valid set survives, in order.
        let r = resolve(
            "rule-ok",
            r#"
[server]
root = "{ROOT}"
[[cache.rule]]
path = "/admin/*"
action = "pass"
[[cache.rule]]
path = "/*"
ttl = 300
swr = 30
stale_if_error = 3600
"#,
        )
        .unwrap();
        assert_eq!(r.config.cache_rules.len(), 2);
        assert!(r.config.cache_rules[0].is_pass());
        assert_eq!(r.config.cache_rules[1].ttl, Some(300));
        assert_eq!(r.config.cache_rules[1].stale_if_error, 3600);
    }

    #[test]
    fn ratelimit_rules_are_validated_at_load() {
        assert!(err(
            "rl-regex",
            r#"
[server]
root = "{ROOT}"
[[ratelimit]]
path = "^/api/.*"
limit = 5
"#,
        )
        .contains("glob, not a regex"));

        assert!(err(
            "rl-zero",
            r#"
[server]
root = "{ROOT}"
[[ratelimit]]
path = "/api/*"
limit = 0
"#,
        )
        .contains("limit > 0"));

        assert!(err(
            "rl-by",
            r#"
[server]
root = "{ROOT}"
[[ratelimit]]
path = "/api/*"
limit = 5
by = "session"
"#,
        )
        .contains("unknown `by`"));

        // `ip` (the default), header: and cookie: forms are all accepted.
        let r = resolve(
            "rl-ok",
            r#"
[server]
root = "{ROOT}"
[[ratelimit]]
path = "/login"
limit = 5
window = 300
[[ratelimit]]
path = "/api/*"
limit = 60
by = "header:X-Api-Key"
burst = 20
[[ratelimit]]
path = "/x/*"
limit = 1
by = "cookie:sid"
"#,
        )
        .unwrap();
        assert_eq!(r.config.ratelimits.len(), 3);
        assert_eq!(r.config.ratelimits[0].by, "ip", "by defaults to ip");
        assert_eq!(r.config.ratelimits[0].window, 300);
        assert_eq!(r.config.ratelimits[1].burst, 20);
    }

    #[test]
    fn trusted_proxies_must_be_addresses() {
        assert!(err(
            "tp-bad",
            r#"
[server]
root = "{ROOT}"
trusted_proxies = ["not-an-ip"]
"#,
        )
        .contains("not an IP or CIDR"));

        assert!(err(
            "tp-prefix",
            r#"
[server]
root = "{ROOT}"
trusted_proxies = ["10.0.0.0/99"]
"#,
        )
        .contains("not an IP or CIDR"));

        let r = resolve(
            "tp-ok",
            r#"
[server]
root = "{ROOT}"
trusted_proxies = ["10.0.0.0/8", "::1", "192.168.1.5"]
"#,
        )
        .unwrap();
        assert_eq!(r.config.trusted_proxies.len(), 3);
    }

    /// A typo must fail loudly rather than being silently ignored.
    #[test]
    fn unknown_keys_are_rejected() {
        let dir = app_dir("typo");
        let text = format!(
            "[server]\nlisten = \"127.0.0.1:8000\"\nroot = \"{}\"\nmax_requsts = 100\n",
            dir.to_str().unwrap()
        );
        let parsed: Result<FileConfig, _> = toml::from_str(&text);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(parsed.is_err(), "deny_unknown_fields should catch typos");
    }

    #[test]
    fn cache_persistence_and_keys_round_trip() {
        let r = resolve(
            "persist",
            r#"
[server]
root = "{ROOT}"
[cache]
response_slots = 64
persist = "/var/lib/askr/rcache.bin"
persist_key = "abc123"
strip_query_params = ["utm_*", "gclid"]
ignore_cookies = ["_ga"]
vary_user_agent = true
saint_seconds = 5
"#,
        )
        .unwrap();
        assert_eq!(
            r.cache_persist.as_deref(),
            Some(std::path::Path::new("/var/lib/askr/rcache.bin"))
        );
        assert_eq!(r.cache_persist_key.as_deref(), Some("abc123"));
        assert_eq!(r.config.cache_strip_query, vec!["utm_*", "gclid"]);
        assert!(r.config.cache_vary_user_agent);
        assert_eq!(r.config.cache_saint_seconds, 5);
    }
}
