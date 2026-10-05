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
    /// Keys in the file that no section has, one warning each, filled in by
    /// [`FileConfig::parse`]. Not a key itself.
    #[serde(skip)]
    pub unknown_keys: Vec<String>,
}

/// A virtual host: one or more `hosts` (exact or `*.suffix`) served from `root`
/// with its own `front` controller. Full dynamic dispatch requires per-request
/// mode; in worker mode statics are per-site but the booted app is fixed.
#[derive(Debug, Deserialize, Clone)]
pub struct SiteSpec {
    pub hosts: Vec<String>,
    pub root: PathBuf,
    #[serde(default = "default_front")]
    pub front: String,
    /// This application's name — see [`ServerSection::app_id`].
    #[serde(default)]
    pub app_id: Option<String>,
}

/// A declarative host redirect (e.g. `www.domene.no` → `https://domene.no`). The
/// request path + query are preserved; `status` defaults to 308 (permanent, keeps
/// the method). `from` matches the Host header exactly or as a `*.suffix` glob.
#[derive(Debug, Deserialize, Clone)]
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
pub struct SidecarSpec {
    /// The command to run (via `sh -c`), e.g. "node bootstrap/ssr/ssr.mjs".
    pub command: String,
}

#[derive(Debug, Deserialize)]
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
    /// A name for this application (`shop`), used instead of its docroot to tell its data
    /// apart from other applications'. The same name is the same application on every
    /// host — which lets the durable SQL backends keep applications apart, as shared
    /// memory does. Unset, the docroot names it, as before.
    #[serde(default)]
    pub app_id: Option<String>,
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
    /// Refuse to serve unless the sandbox applied. Opt-in: flipping the default would
    /// turn an upgrade into an outage on any kernel missing a feature.
    #[serde(default)]
    pub sandbox_required: bool,
    /// Landlock-writable paths (enables the filesystem restriction).
    #[serde(default)]
    pub sandbox_write: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
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
    /// Production state-bleed detection: check one request in this many per worker.
    /// Findings reach `/api/status` as `state_bleed`. Unset = off.
    #[serde(default)]
    pub paranoid_sample: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
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
pub struct AdminSection {
    /// Admin dashboard/API listen address (e.g. "127.0.0.1:9000"). Off if unset.
    pub listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
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
    /// Keep the ring in a named shared-memory object so pending jobs survive a
    /// restart. The value names the ring; two instances on one host need two names.
    /// Off by default: the object lives in /dev/shm, which a container caps at 64 MiB.
    #[serde(default)]
    pub persist: Option<String>,
    /// How long a job may sit ready and unclaimed before Askr calls the lane stalled —
    /// in the log, in `/api/status`'s `warnings`, and in `askr_queue_unattended`.
    /// Defaults to 30 seconds: ten is unremarkable queue latency, thirty means nothing
    /// is listening. Raise it for an app whose queues are deliberately batchy, lower it
    /// to be told sooner. 0 keeps the default.
    #[serde(default)]
    pub stall_secs: u64,
    /// Docroot of the application whose jobs these workers consume. Defaults to
    /// `[server] root`.
    ///
    /// Only needed with `[[site]]`: shared memory is namespaced by docroot, so a sidecar
    /// rooted at the top-level `root` cannot pop jobs a site's application pushed. Set it
    /// to the same path as that site's `root`.
    pub root: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SchedulerSection {
    /// Scheduler runner script (e.g. examples/askr-scheduler.php). Off if unset.
    pub script: Option<PathBuf>,
    /// Docroot of the application the scheduler runs for. Defaults to `[queue] root`,
    /// then `[server] root`. See [`QueueSection::root`].
    pub root: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
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
pub struct BroadcastSection {
    /// Enable the broadcast ring + SSE endpoint (askr_broadcast()).
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
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
    /// Verified reloads: the canary replays the last distinct anonymous GETs the old
    /// code answered, and a page that worked before and fails now aborts the rollout.
    /// Needs `canary`.
    #[serde(default)]
    pub verify: bool,
    /// URLs replayed per reload, most recent first.
    #[serde(default = "default_verify_requests")]
    pub verify_requests: u64,
    /// Seconds the gate waits for the replay before calling the canary unhealthy.
    #[serde(default = "default_verify_timeout")]
    pub verify_timeout: u64,
}

fn default_verify_requests() -> u64 {
    200
}
fn default_verify_timeout() -> u64 {
    120
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
            verify: false,
            verify_requests: default_verify_requests(),
            verify_timeout: default_verify_timeout(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RecordSection {
    /// Record failing (5xx) requests into this directory for `askr replay`.
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
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
            app_id: None,
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
            sandbox_required: false,
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

/// The fully-resolved startup configuration, from a file or the command line — both
/// come out of [`FileConfig::assemble`].
pub struct Resolved {
    pub config: Config,
    pub workers: usize,
    pub workers_min: usize,
    pub workers_max: usize,
    pub ini: Option<String>,
    pub app_base: Option<PathBuf>,
    pub paranoid: bool,
    /// `[worker] paranoid_sample`, checked: at least 1, and not with `paranoid`.
    pub paranoid_sample: Option<u64>,
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
    pub queue_persist: Option<String>,
    /// Seconds a ready job may wait before its lane is reported stalled. 0 = default.
    pub queue_stall_secs: u64,
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
    /// `[reload] verify`, and its budget and patience.
    pub verify: bool,
    pub verify_requests: u64,
    pub verify_timeout: u64,
}

impl FileConfig {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Parse a config document, collecting unknown keys as warnings in
    /// [`unknown_keys`](Self::unknown_keys) rather than refusing the file.
    ///
    /// Up to 1.7.3 an unknown key was an error. That caught typos, but it also meant a
    /// config using a newer release's key would not load on an older one, so rolling a
    /// binary back could take a site down over a line nobody needed. A warning keeps the
    /// typo-catching — it names the key, where it is, and the nearest key that section
    /// does accept — without making the file unloadable. A key of the wrong *type* is
    /// still an error: that is a value Askr would otherwise have to guess at.
    pub fn parse(text: &str) -> Result<Self> {
        let mut unknown = Vec::new();
        let parsed: std::result::Result<FileConfig, _> =
            serde_ignored::deserialize(toml::Deserializer::parse(text)?, |path| {
                unknown.push(unknown_key_warning(&path_segments(&path)));
            });
        match parsed {
            Ok(mut cfg) => {
                cfg.unknown_keys = unknown;
                Ok(cfg)
            }
            // The unknown key is often *why* the file failed — `lisen = …` is what leaves
            // `listen` missing — so the error carries them, not just the warnings a file
            // that loaded would have printed.
            Err(e) if !unknown.is_empty() => Err(anyhow::Error::new(e).context(unknown.join("\n"))),
            Err(e) => Err(e.into()),
        }
    }

    /// Validate and resolve into a runtime [`Config`], checking that paths and
    /// certificates actually exist.
    pub fn resolve(self, cpus: usize) -> Result<Resolved> {
        let listen: SocketAddr = self
            .server
            .listen
            .parse()
            .with_context(|| format!("invalid server.listen {:?}", self.server.listen))?;

        let docroot = app_root(&self.server.root, "server.root")?;

        let front = PathBuf::from(&self.server.front);
        anyhow::ensure!(
            docroot.join(&front).is_file(),
            "front controller not found: {}",
            docroot.join(&front).display()
        );

        // Which application the queue/scheduler sidecars belong to. Checked here; spelled
        // below, once the sites are known.
        let queue_root = match &self.queue.root {
            Some(p) => Some(app_root(p, "queue.root")?),
            None => None,
        };
        let scheduler_root = match &self.scheduler.root {
            Some(p) => Some(app_root(p, "scheduler.root")?),
            None => None,
        };

        // Resolve [[site]] virtual hosts (each with its own docroot + front
        // controller). Host-routed per request; full dynamic dispatch is
        // per-request mode — in worker mode the booted app is fixed (statics are
        // still served per site).
        let mut sites = Vec::new();
        for s in &self.site {
            let sroot = app_root(&s.root, "site root")?;
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
                app_id: s.app_id.clone(),
            });
        }

        // `app_id`s: well-formed, one per application, and never one for two. An id names
        // a docroot, so two domains serving one docroot share it, as they share data.
        let mut ids: Vec<(&PathBuf, &str, String)> = Vec::new();
        let named =
            std::iter::once((
                &docroot,
                self.server.app_id.as_deref(),
                "[server]".to_string(),
            ))
            .chain(self.site.iter().zip(&sites).enumerate().map(
                |(i, (spec, site))| {
                    (
                        &site.docroot,
                        spec.app_id.as_deref(),
                        format!("[[site]] #{}", i + 1),
                    )
                },
            ));
        for (root, id, place) in named {
            let Some(id) = id else { continue };
            anyhow::ensure!(
                crate::ns::valid_app_id(id),
                "{place} app_id {id:?} is not a valid name: lowercase letters, digits, `.`, \
                 `_` and `-`, starting with a letter or digit, at most 64 characters"
            );
            for (r, other, where_) in &ids {
                anyhow::ensure!(
                    !(*r == root && *other != id),
                    "{place} and {where_} serve the same docroot {} but name it {id:?} and \
                     {other:?} — one docroot is one application, with one name",
                    root.display()
                );
                anyhow::ensure!(
                    !(*r != root && *other == id),
                    "{place} and {where_} both have app_id {id:?} but serve different \
                     docroots — they would share every cache key, session and queue"
                );
            }
            ids.push((root, id, place));
        }

        // The namespace is a hash of the docroot as spelled, so a sidecar root that names
        // a served application's directory another way — through a symlink, or the
        // other way round — must take that application's spelling, or it becomes a
        // second application and its workers never see the jobs (the 1.7.0 fault, by
        // another route). One the instance does not serve keeps its own.
        let served: Vec<&PathBuf> = std::iter::once(&docroot)
            .chain(sites.iter().map(|s| &s.docroot))
            .collect();
        let as_served = |p: PathBuf| -> PathBuf {
            let real = std::fs::canonicalize(&p).ok();
            served
                .iter()
                .find(|d| **d == &p || (real.is_some() && std::fs::canonicalize(d).ok() == real))
                .map(|d| (*d).clone())
                .unwrap_or(p)
        };
        let queue_root = queue_root.map(as_served);
        let scheduler_root = scheduler_root.map(as_served);
        // Queue workers and the scheduler are separate processes and may legitimately
        // belong to different applications, so they get separate roots rather than one
        // shared value. Collapsing them into one — which the first cut of this did —
        // means `[scheduler] root` is silently ignored whenever `[queue] root` is also
        // set: a key that does not do what its name says, which is worse than no key.
        let sidecar_docroot = queue_root.clone().unwrap_or_else(|| docroot.clone());
        let scheduler_docroot = scheduler_root
            .or(queue_root)
            .unwrap_or_else(|| docroot.clone());

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
        check_paranoid_sample(
            self.worker.paranoid,
            self.worker.paranoid_sample,
            "worker.paranoid_sample",
        )?;

        anyhow::ensure!(
            !self.reload.verify || self.reload.canary,
            "reload.verify needs reload.canary = true — the replay runs in the canary, and \
             its verdict is part of the canary gate"
        );

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
        let acme = &self.acme;
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
        // Virtual hosts plus a sidecar is ambiguous, and the wrong guess is silent.
        //
        // Shared memory is namespaced per application, derived from the docroot, and
        // `askr_queue_pop` matches the *namespaced* key. A sidecar is one process with one
        // namespace for its whole life, so it can only ever serve one application — and
        // Askr cannot infer which one from a queue script that could belong to any of
        // them. Until 1.7.0 it silently used the top-level `root`, so on any instance
        // where a `[[site]]` application dispatched the jobs, every job was accepted,
        // stored, and never read: no exception, no failed job, nothing in the log from the
        // application's side. One deployment ran that way for six days and only noticed
        // because a person could not reset their password.
        //
        // So: say which application. `[queue] root` = `[server] root` is a perfectly good
        // answer when the top-level application is the one queueing; it just has to be an
        // answer rather than a default nobody knew was being chosen.
        // `[[sidecar]]` commands are in here too: they are supervised processes that
        // inherit the same namespace, so a sidecar running `php artisan …` against shared
        // memory is exposed to exactly the same silence.
        if (self.queue.script.is_some()
            || self.scheduler.script.is_some()
            || !self.sidecar.is_empty())
            && !self.site.is_empty()
            && self.queue.root.is_none()
            && self.scheduler.root.is_none()
        {
            anyhow::bail!(
                "[[site]] is configured together with a sidecar, so \
                 Askr cannot tell which application the sidecar serves. Shared memory is \
                 namespaced per application (by docroot) and a sidecar can only consume \
                 one of them — picking wrong means every job is stored and never read, \
                 silently. Set `[queue] root` (and `[scheduler] root` if it differs) to the \
                 docroot of the application that dispatches the jobs. If that is the \
                 top-level application, set it to the same path as `[server] root`."
            );
        }
        if let Some(s) = &self.queue.script {
            anyhow::ensure!(s.is_file(), "queue.script not found: {}", s.display());
        }
        if let Some(s) = &self.scheduler.script {
            anyhow::ensure!(s.is_file(), "scheduler.script not found: {}", s.display());
        }
        Ok(self.assemble(Checked {
            listen,
            docroot,
            front,
            sidecar_docroot,
            scheduler_docroot,
            sites,
            workers,
            max_body_size,
            admin_listen,
            acme_http,
        }))
    }

    /// The one place a runtime [`Config`] — and the rest of [`Resolved`] — is built.
    ///
    /// Both ways of configuring `serve` come through here: [`resolve`](Self::resolve) for
    /// a file, and the command line, which describes itself as a `FileConfig` and calls
    /// this directly. There used to be two struct literals, one per path, and a setting
    /// added to one had to be remembered in the other — or, as happened once, put on the
    /// wrong struct altogether. Now a new field is decided here, once, and the compiler
    /// asks for it.
    ///
    /// Validation is *not* here, because the two paths validate differently and must
    /// keep saying so in their own terms (`tls.cert not found` is the right message for
    /// a file and the wrong one for `--tls-cert`). Whatever validation had to compute on
    /// the way — a canonical path, a parsed address — arrives in `checked`, and the
    /// matching raw fields of `self` (`server.listen`, `server.root`, `server.workers`,
    /// `server.front`, `server.max_body_size`, `admin.listen`, `acme.http`) are not read
    /// again.
    pub(crate) fn assemble(self, checked: Checked) -> Resolved {
        let Checked {
            listen,
            docroot,
            front,
            sidecar_docroot,
            scheduler_docroot,
            sites,
            workers,
            max_body_size,
            admin_listen,
            acme_http,
        } = checked;
        let tls_self_signed = self.tls.self_signed;
        // ACME counts as TLS: the resolved config has to say the server will speak HTTPS,
        // even though the certificate doesn't exist yet. Otherwise anything reading it
        // before the ACME step runs — logging, admin status — reports plain HTTP.
        let tls_on = self.tls.cert.is_some() || tls_self_signed || self.acme.enabled;
        // Queue workers need a script to run; without one the count means nothing.
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
        let acme = self.acme;

        Resolved {
            config: Config {
                docroot,
                sidecar_docroot,
                scheduler_docroot,
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
                sandbox: self.server.sandbox
                    || self.server.sandbox_required
                    || !self.server.sandbox_write.is_empty(),
                sandbox_required: self.server.sandbox_required,
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
                app_id: self.server.app_id.clone(),
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
            paranoid_sample: self.worker.paranoid_sample,
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
            queue_persist: self.queue.persist,
            queue_stall_secs: self.queue.stall_secs,
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
            verify: self.reload.verify,
            verify_requests: self.reload.verify_requests.max(1),
            verify_timeout: self.reload.verify_timeout.max(1),
        }
    }
}

/// A serde path as plain segments: table keys, and array indices as numbers.
fn path_segments(path: &serde_ignored::Path) -> Vec<String> {
    fn walk(p: &serde_ignored::Path, out: &mut Vec<String>) {
        use serde_ignored::Path;
        match p {
            Path::Root => {}
            Path::Seq { parent, index } => {
                walk(parent, out);
                out.push(index.to_string());
            }
            Path::Map { parent, key } => {
                walk(parent, out);
                out.push(key.clone());
            }
            Path::Some { parent }
            | Path::NewtypeStruct { parent }
            | Path::NewtypeVariant { parent } => walk(parent, out),
        }
    }
    let mut out = Vec::new();
    walk(path, &mut out);
    out
}

/// The warning for one unknown key: where it is, and the key that was probably meant.
fn unknown_key_warning(segments: &[String]) -> String {
    let (key, table) = match segments.split_last() {
        Some((k, t)) => (k.as_str(), t),
        None => ("", &[][..]),
    };
    // `["site", "1"]` is the second `[[site]]`; `["cache", "rule", "0"]` the first
    // `[[cache.rule]]`; `["server"]` is `[server]`; `[]` the top level.
    let (names, entry): (Vec<&str>, Option<usize>) = match table.split_last() {
        Some((last, rest)) if last.parse::<usize>().is_ok() => (
            rest.iter().map(String::as_str).collect(),
            last.parse::<usize>().ok(),
        ),
        _ => (table.iter().map(String::as_str).collect(), None),
    };
    let place = match (names.is_empty(), entry) {
        (true, _) => "at the top level".to_string(),
        (false, Some(i)) => format!("in [[{}]] #{}", names.join("."), i + 1),
        (false, None) => format!("in [{}]", names.join(".")),
    };
    let accepted = accepted_keys(&names);
    let hint = match closest(key, accepted) {
        Some(m) => format!(" — did you mean `{m}`?"),
        None if !accepted.is_empty() => format!(". That section accepts: {}.", accepted.join(", ")),
        None => String::new(),
    };
    format!(
        "unknown config key `{key}` {place} is ignored (a typo, or a key from a newer Askr \
         than this one){hint}"
    )
}

/// The keys a section accepts, read from the section type itself so the list can never
/// drift from what is actually parsed.
fn accepted_keys(table: &[&str]) -> &'static [&'static str] {
    match table {
        [] => fields_of::<FileConfig>(),
        ["server"] => fields_of::<ServerSection>(),
        ["worker"] => fields_of::<WorkerSection>(),
        ["tls"] => fields_of::<TlsSection>(),
        ["acme"] => fields_of::<AcmeSection>(),
        ["admin"] => fields_of::<AdminSection>(),
        ["queue"] => fields_of::<QueueSection>(),
        ["scheduler"] => fields_of::<SchedulerSection>(),
        ["cache"] => fields_of::<CacheSection>(),
        ["cache", "rule"] => fields_of::<CacheRule>(),
        ["broadcast"] => fields_of::<BroadcastSection>(),
        ["reload"] => fields_of::<ReloadSection>(),
        ["record"] => fields_of::<RecordSection>(),
        ["pusher"] => fields_of::<PusherSection>(),
        ["sidecar"] => fields_of::<SidecarSpec>(),
        ["redirect"] => fields_of::<RedirectRule>(),
        ["site"] => fields_of::<SiteSpec>(),
        ["ratelimit"] => fields_of::<RateLimitRule>(),
        _ => &[],
    }
}

/// The accepted key nearest to `key`, if it is near enough to be a typo of it.
fn closest(key: &str, accepted: &[&'static str]) -> Option<&'static str> {
    let norm = |s: &str| s.to_ascii_lowercase().replace('-', "_");
    let key_n = norm(key);
    // The same words in another order — `max_workers` for `workers_max` — is a slip the
    // edit distance alone rates as far apart.
    let words = |s: &str| {
        let mut w: Vec<String> = s.split('_').map(str::to_string).collect();
        w.sort();
        w
    };
    if let Some(a) = accepted.iter().find(|a| words(&norm(a)) == words(&key_n)) {
        return Some(a);
    }
    accepted
        .iter()
        .map(|a| (strsim::levenshtein(&key_n, &norm(a)), *a))
        .filter(|(d, _)| *d <= (key.chars().count() / 3).max(1))
        .min_by_key(|(d, _)| *d)
        .map(|(_, a)| a)
}

/// The field names a derived `Deserialize` struct accepts.
///
/// serde hands them to `Deserializer::deserialize_struct` — they are how it knows which
/// keys are fields — so a deserializer that records them and stops is all it takes.
fn fields_of<T: serde::de::DeserializeOwned>() -> &'static [&'static str] {
    struct Probe<'a>(&'a mut &'static [&'static str]);
    impl<'de> serde::Deserializer<'de> for Probe<'_> {
        type Error = serde::de::value::Error;
        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom("probe"))
        }
        fn deserialize_struct<V: serde::de::Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> std::result::Result<V::Value, Self::Error> {
            *self.0 = fields;
            Err(serde::de::Error::custom("probe"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map
            enum identifier ignored_any
        }
    }
    let mut fields: &'static [&'static str] = &[];
    let _ = T::deserialize(Probe(&mut fields));
    fields
}

/// A docroot as Askr keeps it: checked to exist, made absolute, and otherwise **as
/// configured** — a symlink in it stays a symlink.
///
/// It used to be canonicalised, which resolved `/srv/app/current/public` to the release
/// `current` pointed at when the server started. Every reload after that served the
/// same release: the documented deploy (swap the link, reload) reported success and
/// changed nothing, and in worker mode — where the application boots from
/// `ASKR_APP_BASE`, which was never canonicalised — the PHP was new and the static files
/// old. Kept as written, the link is followed on every request, like nginx's `root`.
pub(crate) fn app_root(p: &std::path::Path, key: &str) -> Result<PathBuf> {
    std::fs::metadata(p).with_context(|| format!("{key} {} not found", p.display()))?;
    Ok(crate::ns::app_path(p))
}

/// `paranoid_sample` is a rate (1 = every request), and choosing it alongside `paranoid`
/// — which already checks every request, verbosely — is two answers to one question.
pub(crate) fn check_paranoid_sample(paranoid: bool, sample: Option<u64>, key: &str) -> Result<()> {
    if let Some(n) = sample {
        anyhow::ensure!(
            n >= 1,
            "{key} must be at least 1 (one request in N is checked)"
        );
        anyhow::ensure!(
            !paranoid,
            "{key} and paranoid are both set — paranoid checks every request (dev); \
             {key} checks one in N (production). Choose one."
        );
    }
    Ok(())
}

/// The durable SQL backends this process has selected, by the variable that selects each.
///
/// Empty without the `sql-backend` feature: the variables are then not read at all.
pub fn l2_backends() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut on = Vec::new();
    #[cfg(feature = "sql-backend")]
    {
        if crate::cache_sql::enabled() {
            on.push("ASKR_CACHE_DB");
        }
        if crate::squeue_sql::enabled() {
            on.push("ASKR_QUEUE_DB");
        }
        if crate::broadcast_sql::enabled() {
            on.push("ASKR_BROADCAST_DB");
        }
    }
    on
}

/// A warning when more than one application would share the durable SQL backends.
///
/// Shared memory is namespaced per application; the SQL backends are not. Their tables
/// are keyed by the name PHP chose — a cache key, a queue name, a channel — and nothing
/// else, so two applications on one instance that both say `default` are talking about
/// the same row. Namespacing them needs an application id that stays the same across
/// hosts and deploys, which a hash of a local docroot path is not, so for now the honest
/// thing is to say so at startup rather than let it be discovered.
///
/// `l2` is what [`l2_backends`] returned; it is a parameter so this can be tested without
/// touching the environment.
pub fn l2_sharing_warning(config: &Config, l2: &[&str]) -> Option<String> {
    // Each application, by docroot, and the name it was given if any. A sidecar root is
    // the application it names; an id names a docroot, so a site sharing one shares it.
    let id_of = |root: &std::path::Path| -> Option<&str> {
        if root == config.docroot {
            if let Some(id) = &config.app_id {
                return Some(id);
            }
        }
        config
            .sites
            .iter()
            .find(|s| s.docroot == root && s.app_id.is_some())
            .and_then(|s| s.app_id.as_deref())
    };
    let mut apps: Vec<&std::path::Path> = vec![
        &config.docroot,
        &config.sidecar_docroot,
        &config.scheduler_docroot,
    ];
    apps.extend(config.sites.iter().map(|s| s.docroot.as_path()));
    apps.sort();
    apps.dedup();
    if apps.len() < 2 || l2.is_empty() {
        return None;
    }
    let unnamed = apps.iter().filter(|a| id_of(a).is_none()).count();
    let mut effects = Vec::new();
    for var in l2 {
        effects.push(match *var {
            // An unnamed application's rows carry no namespace, so its flush empties the
            // table for everyone, named or not — one is enough.
            "ASKR_CACHE_DB" if unnamed >= 1 => {
                "cache keys are shared, so one application reads another's cached values, \
                 and a cache flush in any of them empties the cache for all"
            }
            "ASKR_QUEUE_DB" if unnamed >= 2 => {
                "queue names are shared, so a queue worker takes jobs pushed by another \
                 application and runs them in the wrong one"
            }
            // Broadcasting is not namespaced at all, here or in shared memory: one instance
            // has one Pusher secret, so it serves one application's realtime traffic.
            "ASKR_BROADCAST_DB" => {
                "channel names are shared, so a broadcast reaches the other applications' \
                 subscribers on the same channel"
            }
            _ => continue,
        });
    }
    if effects.is_empty() {
        return None;
    }
    Some(format!(
        "{} {} set and this instance serves {} applications ([[site]] / [queue] root / \
         [scheduler] root), but the SQL backends do not keep them apart: {}. Give each \
         application an app_id ([server] app_id, [[site]] app_id) to separate its cache and \
         queue rows, run each as its own instance with its own database files, or leave {} \
         unset to use shared memory, which is separated per application.",
        l2.join(" and "),
        if l2.len() == 1 { "is" } else { "are" },
        apps.len(),
        effects.join("; "),
        if l2.len() == 1 { "it" } else { "them" },
    ))
}

/// What validation computed on the way to [`FileConfig::assemble`]: the values that had
/// to be parsed or looked up on disk to be checked at all, handed over rather than
/// worked out a second time. See `assemble` for which raw fields these stand in for.
pub(crate) struct Checked {
    pub listen: SocketAddr,
    /// As configured, made absolute ([`app_root`]); the namespace is a hash of it.
    pub docroot: PathBuf,
    /// Relative to `docroot`, and known to exist there.
    pub front: PathBuf,
    /// The application the queue workers and `[[sidecar]]` commands serve.
    pub sidecar_docroot: PathBuf,
    /// The application the scheduler runs for.
    pub scheduler_docroot: PathBuf,
    pub sites: Vec<crate::server::Site>,
    pub workers: usize,
    pub max_body_size: usize,
    pub admin_listen: Option<SocketAddr>,
    pub acme_http: Option<SocketAddr>,
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

    /// One application is the normal case and says nothing; a second one — a `[[site]]`,
    /// or a sidecar rooted elsewhere — with any SQL backend selected says what is shared.
    #[test]
    fn two_applications_on_the_sql_backends_are_warned_about() {
        let one = resolve("one", "[server]\nroot = \"{ROOT}\"\n").unwrap();
        assert_eq!(
            l2_sharing_warning(&one.config, &["ASKR_CACHE_DB", "ASKR_QUEUE_DB"]),
            None
        );

        let site = app_dir("l2-site");
        let body = format!(
            "[server]\nroot = \"{{ROOT}}\"\n[[site]]\nhosts = [\"b.test\"]\nroot = \"{}\"\n",
            site.display()
        );
        let two = resolve("two", &body).unwrap();
        assert_eq!(
            l2_sharing_warning(&two.config, &[]),
            None,
            "shared memory is separated"
        );
        let w = l2_sharing_warning(&two.config, &["ASKR_QUEUE_DB"]).expect("a warning");
        assert!(w.starts_with("ASKR_QUEUE_DB is set"), "{w}");
        assert!(w.contains("2 applications"), "{w}");
        assert!(w.contains("runs them in the wrong one"), "{w}");
        assert!(w.contains("leave it unset"), "{w}");
        assert!(!w.contains("cache keys"), "only what is selected: {w}");
        let all = ["ASKR_CACHE_DB", "ASKR_QUEUE_DB", "ASKR_BROADCAST_DB"];
        let w = l2_sharing_warning(&two.config, &all).unwrap();
        assert!(
            w.starts_with("ASKR_CACHE_DB and ASKR_QUEUE_DB and ASKR_BROADCAST_DB are"),
            "{w}"
        );
        assert!(
            w.contains("cache flush") && w.contains("subscribers"),
            "{w}"
        );

        // Named applications keep their cache and queue rows apart; broadcasting is not
        // namespaced, so it is still shared.
        let body = format!(
            "[server]\nroot = \"{{ROOT}}\"\napp_id = \"shop\"\n[[site]]\nhosts = [\"b.test\"]\nroot = \"{}\"\napp_id = \"blog\"\n",
            site.display()
        );
        let named = resolve("named", &body).unwrap();
        assert_eq!(
            l2_sharing_warning(&named.config, &["ASKR_CACHE_DB", "ASKR_QUEUE_DB"]),
            None
        );
        let w = l2_sharing_warning(&named.config, &["ASKR_QUEUE_DB", "ASKR_BROADCAST_DB"]).unwrap();
        assert!(
            w.contains("subscribers") && !w.contains("queue names"),
            "{w}"
        );
        // One unnamed application is enough to share the cache: its flush empties all of it.
        let body = format!(
            "[server]\nroot = \"{{ROOT}}\"\napp_id = \"shop\"\n[[site]]\nhosts = [\"b.test\"]\nroot = \"{}\"\n",
            site.display()
        );
        let half = resolve("half", &body).unwrap();
        assert!(l2_sharing_warning(&half.config, &["ASKR_CACHE_DB"]).is_some());
        assert_eq!(l2_sharing_warning(&half.config, &["ASKR_QUEUE_DB"]), None);

        // No [[site]], but the queue workers belong to a different application.
        let body = format!(
            "[server]\nroot = \"{{ROOT}}\"\n[queue]\nroot = \"{}\"\n",
            site.display()
        );
        let sidecar = resolve("sidecar", &body).unwrap();
        assert!(l2_sharing_warning(&sidecar.config, &["ASKR_CACHE_DB"]).is_some());
        let _ = std::fs::remove_dir_all(&site);
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
        let cfg = FileConfig::parse(&text)?;
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
    /// A sidecar beside `[[site]]` has to say which application it serves.
    ///
    /// Shared memory is namespaced by docroot and `askr_queue_pop` matches the namespaced
    /// key, so a sidecar rooted at one application cannot see another's jobs — ever. Askr
    /// used to default to the top-level `root` and a `[[site]]` application's queue then
    /// filled up and was never read, in complete silence. One deployment ran that way for
    /// six days.
    #[test]
    fn a_sidecar_beside_virtual_hosts_must_name_its_application() {
        let site = app_dir("sidecar-site");
        let body = format!(
            r#"
[server]
root = "{{ROOT}}"

[[site]]
hosts = ["other.test"]
root = "{site}"

[queue]
slots = 64
workers = 1
script = "{site}/index.php"
"#,
            site = site.display()
        );
        let e = err("sidecar-ambiguous", &body);
        assert!(
            e.contains("cannot tell which application the sidecar serves"),
            "the ambiguous case must refuse: {e}"
        );
        assert!(
            e.contains("[queue] root"),
            "and name the key that resolves it: {e}"
        );

        // Named, it resolves — and the sidecars land on the application that was named,
        // not on the top-level root.
        let named = body.replace(
            "[queue]\n",
            &format!("[queue]\nroot = \"{}\"\n", site.display()),
        );
        let r = resolve("sidecar-named", &named).expect("naming the application is accepted");
        assert_eq!(
            r.config.sidecar_docroot,
            crate::ns::app_path(&site),
            "queue workers take the named application"
        );
        assert_eq!(
            r.config.scheduler_docroot,
            crate::ns::app_path(&site),
            "and the scheduler inherits it when it has no root of its own"
        );
    }

    /// A `[queue] root` that reaches a site's directory by another spelling — through a
    /// symlink, or the site's own root being one — is that site's application, not a
    /// second one. The namespace is a hash of the spelling, so taking the other spelling
    /// would strand every job the site pushes.
    #[test]
    fn a_sidecar_root_spelled_another_way_joins_the_site_it_names() {
        let site = app_dir("spelled-site");
        let link = site.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&site, &link).unwrap();
        let body = format!(
            r#"
[server]
root = "{{ROOT}}"

[[site]]
hosts = ["b.test"]
root = "{site}"

[queue]
root = "{link}"
slots = 64
workers = 1
script = "{site}/index.php"
"#,
            site = site.display(),
            link = link.display()
        );
        let r = resolve("spelled", &body).unwrap();
        assert_eq!(r.config.sites[0].docroot, crate::ns::app_path(&site));
        assert_eq!(
            r.config.sidecar_docroot, r.config.sites[0].docroot,
            "the queue workers are the site's application"
        );
        assert_eq!(
            crate::ns::for_docroot(&r.config.sidecar_docroot),
            crate::ns::for_docroot(&r.config.sites[0].docroot)
        );
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&site);
    }

    /// An `app_id` is a name for one application: well-formed, never two names for one
    /// docroot, never one name for two.
    #[test]
    fn an_app_id_names_exactly_one_application() {
        let site = app_dir("appid-site");
        let cfg = |server_id: &str, site_root: &std::path::Path, site_id: &str| {
            format!(
                "[server]\nroot = \"{{ROOT}}\"\n{server_id}\n[[site]]\nhosts = [\"b.test\"]\nroot = \"{}\"\n{site_id}\n",
                site_root.display()
            )
        };
        let r = resolve(
            "appid-ok",
            &cfg("app_id = \"shop\"", &site, "app_id = \"blog\""),
        )
        .unwrap();
        assert_eq!(r.config.app_id.as_deref(), Some("shop"));
        assert_eq!(r.config.sites[0].app_id.as_deref(), Some("blog"));

        let e = resolve(
            "appid-bad",
            "[server]\nroot = \"{ROOT}\"\napp_id = \"My Shop\"\n",
        )
        .err()
        .expect("refused")
        .to_string();
        assert!(
            e.contains("[server] app_id \"My Shop\" is not a valid name"),
            "{e}"
        );

        let e = resolve(
            "appid-twice",
            &cfg("app_id = \"shop\"", &site, "app_id = \"shop\""),
        )
        .err()
        .expect("refused")
        .to_string();
        assert!(
            e.contains("both have app_id \"shop\" but serve different docroots"),
            "{e}"
        );

        // A site serving the top-level docroot is the same application, so it cannot be
        // given a different name.
        let body = "[server]\nroot = \"{ROOT}\"\napp_id = \"shop\"\n[[site]]\nhosts = [\"b.test\"]\nroot = \"{ROOT}\"\napp_id = \"blog\"\n";
        let e = resolve("appid-split", body)
            .err()
            .expect("refused")
            .to_string();
        assert!(e.contains("serve the same docroot"), "{e}");
        let _ = std::fs::remove_dir_all(&site);
    }

    /// `[scheduler] root` has to be honoured when `[queue] root` is also set.
    ///
    /// The first cut collapsed both into one value with `queue.root.or(scheduler.root)`,
    /// so `[scheduler] root` was silently ignored whenever the queue also had one — a key
    /// that does not do what its name says, which is worse than not having the key.
    #[test]
    fn the_scheduler_may_serve_a_different_application_than_the_queue() {
        let qapp = app_dir("sidecar-q");
        let sapp = app_dir("sidecar-s");
        let r = resolve(
            "sidecar-split",
            &format!(
                r#"
[server]
root = "{{ROOT}}"

[queue]
root = "{q}"
slots = 64
workers = 1
script = "{q}/index.php"

[scheduler]
root = "{s}"
script = "{s}/index.php"
"#,
                q = qapp.display(),
                s = sapp.display()
            ),
        )
        .expect("two roots is a valid configuration");
        assert_eq!(r.config.sidecar_docroot, crate::ns::app_path(&qapp));
        assert_eq!(
            r.config.scheduler_docroot,
            crate::ns::app_path(&sapp),
            "[scheduler] root must not be overridden by [queue] root"
        );
    }

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
    /// Every config Askr ships, and every `toml` block in the docs, uses only keys Askr
    /// knows. An unknown key used to stop the server, so a stale one could not survive in
    /// an example; now it is a warning, and this is what stops it going stale quietly.
    #[test]
    fn shipped_configs_and_doc_snippets_use_only_known_keys() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut sources = vec![
            root.join("examples/askr.toml"),
            root.join("examples/docker/askr.toml"),
        ];
        for e in std::fs::read_dir(root.join("docs")).unwrap() {
            sources.push(e.unwrap().path());
        }
        sources.push(root.join("README.md"));
        let mut found = Vec::new();
        let mut checked = 0;
        for path in sources
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "toml" || e == "md"))
        {
            let text = std::fs::read_to_string(path).unwrap();
            let blocks: Vec<String> = if path.extension().is_some_and(|e| e == "toml") {
                vec![text]
            } else {
                text.split("```toml\n")
                    .skip(1)
                    .filter_map(|b| b.split("```").next())
                    // A block inside a `> ` quote carries the quote marker on every line.
                    .map(|b| {
                        b.lines()
                            .map(|l| {
                                l.strip_prefix('>')
                                    .map_or(l, |l| l.strip_prefix(' ').unwrap_or(l))
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .collect()
            };
            for b in blocks {
                checked += 1;
                // A snippet is usually a fragment (no `listen`, no `root`), so it may not
                // parse as a whole file; the unknown keys are collected either way.
                if let Err(e) = toml::Deserializer::parse(&b) {
                    found.push(format!("{}: not valid TOML: {e}", path.display()));
                    continue;
                }
                let warnings = match FileConfig::parse(&b) {
                    Ok(c) => c.unknown_keys,
                    Err(e) => e
                        .chain()
                        .next()
                        .map(|c| c.to_string())
                        .filter(|c| c.starts_with("unknown config key"))
                        .map(|c| c.lines().map(str::to_string).collect())
                        .unwrap_or_default(),
                };
                for w in warnings {
                    found.push(format!("{}: {w}", path.display()));
                }
            }
        }
        assert!(
            checked > 10,
            "only {checked} configs found — is the path right?"
        );
        assert!(found.is_empty(), "{}", found.join("\n"));
    }

    #[test]
    fn an_unknown_key_is_a_warning_that_names_the_key_it_meant() {
        let text = "[server]\nlisten = \"127.0.0.1:8000\"\nroot = \"public\"\nmax_requsts = 100\n\
                    [queue]\nworkers-max = 3\nmax_workers = 3\nshiny_new_thing = true\n\
                    [[site]]\nhosts = [\"a.test\"]\nroot = \"a\"\n\
                    [[site]]\nhosts = [\"b.test\"]\nroot = \"b\"\nfrnt = \"x.php\"\n\
                    [[cache.rule]]\npath = \"/x/*\"\nttl = 5\ntags = []\n\
                    [sidecars]\ncommand = \"node x\"\n";
        let cfg = FileConfig::parse(text).expect("unknown keys no longer refuse the file");
        let w = &cfg.unknown_keys;
        let find = |needle: &str| {
            w.iter()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no warning for {needle}: {w:#?}"))
        };
        assert!(find("`max_requsts` in [server]").contains("did you mean `max_requests`?"));
        assert!(find("`workers-max` in [queue]").contains("did you mean `workers_max`?"));
        assert!(find("`max_workers` in [queue]").contains("did you mean `workers_max`?"));
        assert!(find("`frnt` in [[site]] #2").contains("did you mean `front`?"));
        assert!(find("`sidecars` at the top level").contains("did you mean `sidecar`?"));
        // Nothing near enough to guess at: list what the section does accept instead.
        let novel = find("`shiny_new_thing` in [queue]");
        assert!(novel.contains("newer Askr"), "{novel}");
        assert!(
            novel.contains("one). That section accepts: workers, workers_max, script"),
            "{novel}"
        );
        assert!(!novel.contains("did you mean"), "{novel}");
        assert!(find("`tags` in [[cache.rule]] #1").contains("accepts:"));
        assert_eq!(w.len(), 7, "{w:#?}");
        // The known keys still took effect.
        assert_eq!(cfg.server.listen, "127.0.0.1:8000");
        assert_eq!(cfg.site[1].hosts, ["b.test"]);

        // A clean file has nothing to say, and a key of the wrong type is still an error.
        let clean = FileConfig::parse("[server]\nlisten = \"x\"\nroot = \"r\"\n").unwrap();
        assert!(clean.unknown_keys.is_empty(), "{:?}", clean.unknown_keys);
        assert!(FileConfig::parse("[server]\nlisten = 5\nroot = \"r\"\n").is_err());

        // When the typo is why the file fails, the error says so.
        let e = FileConfig::parse("[server]\nlisen = \"127.0.0.1:1\"\nroot = \"r\"\n").unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("missing field `listen`"), "{e}");
        assert!(
            e.contains("`lisen` in [server]") && e.contains("did you mean `listen`?"),
            "{e}"
        );
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
