//! `askr doctor` — pre-flight checks before deploying.
//!
//! Verifies the embedded PHP build (version, non-ZTS), the extensions a modern
//! Laravel app needs, and platform support (io_uring on Linux). Returns false
//! if any critical check fails so `askr doctor` can gate a deploy.
//!
//! With `--app <path>` it also checks the *application* against the environment it is
//! about to run in. That half exists because every failure worth an afternoon so far has
//! been a coherent-looking configuration that could not work: an app queueing its mail to
//! `onQueue('mail')` while the only worker polled `default`; `SESSION_DRIVER=askr` with no
//! shared-memory slots, which fails silently and surfaces as 419 on every form; a mailer
//! configured under the vendor's variable name instead of Laravel's, so it looked ready
//! and wrote to the log. None of those are PHP-build problems, and the PHP-build checks
//! were the only ones that existed.

use std::thread;

use askr_php::Interpreter;

/// Extensions Laravel requires (its documented list, plus json/phar which
/// Composer and the framework rely on). The Linux release/Docker image ships all
/// of these; the minimal macOS dev build (test suite only) omits curl.
const REQUIRED: &[&str] = &[
    "ctype",
    "curl",
    "dom",
    "fileinfo",
    "filter",
    "hash",
    "json",
    "libxml",
    "mbstring",
    "openssl",
    "pcre",
    "pdo",
    "phar",
    "session",
    "tokenizer",
    "xml",
];

/// PDO database drivers — an app needs at least one. We report which are present.
const DB_DRIVERS: &[&str] = &["pdo_sqlite", "pdo_mysql", "pdo_pgsql"];

/// Extensions many real apps need (Filament needs intl; gd for images; zip/exif
/// are common; iconv is required by the QR-code library behind Fortify's two-factor
/// setup). Present in the Linux release/Docker image. Not fatal — an app with no QR
/// codes does without.
const RECOMMENDED: &[&str] = &["intl", "gd", "zip", "exif", "bcmath", "iconv"];

struct PhpInfo {
    version: String,
    zts: bool,
    extensions: Vec<String>,
}

pub fn run(ini: Option<String>, app: Option<std::path::PathBuf>) -> bool {
    println!("askr doctor\n");

    let mut ok = true;

    match probe_php(ini) {
        Ok(info) => {
            check(&mut ok, true, &format!("embedded PHP {}", info.version));

            // PHP version: Laravel 13 needs >= 8.3; we recommend the latest (8.5).
            match php_minor(&info.version) {
                Some((maj, min)) if (maj, min) >= (8, 3) => {
                    if (maj, min) >= (8, 5) {
                        mark(true, "PHP >= 8.5 (latest, JIT improvements)");
                    } else {
                        mark(true, &format!("PHP {maj}.{min} (>= 8.3; 8.5 recommended)"));
                    }
                }
                Some((maj, min)) => check(
                    &mut ok,
                    false,
                    &format!("PHP {maj}.{min} is too old — Laravel 13 needs >= 8.3"),
                ),
                None => {}
            }

            // non-ZTS is required.
            check(
                &mut ok,
                !info.zts,
                if info.zts {
                    "thread safety: ZTS  (REQUIRED: non-ZTS / NTS build)"
                } else {
                    "thread safety: non-ZTS (NTS)"
                },
            );

            for ext in REQUIRED {
                let present = info.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext));
                check(&mut ok, present, &format!("ext-{ext}"));
            }
            // At least one PDO database driver.
            let drivers: Vec<&str> = DB_DRIVERS
                .iter()
                .copied()
                .filter(|d| info.extensions.iter().any(|e| e.eq_ignore_ascii_case(d)))
                .collect();
            check(
                &mut ok,
                !drivers.is_empty(),
                &if drivers.is_empty() {
                    "no PDO database driver (need pdo_sqlite / pdo_mysql / pdo_pgsql)".to_string()
                } else {
                    format!("database drivers: {}", drivers.join(", "))
                },
            );

            // OpcCache is compiled into PHP 8.5; recommend enabling it for prod.
            let opcache = info
                .extensions
                .iter()
                .any(|e| e.eq_ignore_ascii_case("Zend OPcache"));
            mark(
                opcache,
                "Zend OPcache available (enable with opcache.enable=1 + JIT)",
            );

            for ext in RECOMMENDED {
                let present = info.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext));
                mark(present, &format!("ext-{ext} (recommended)"));
            }

            let loaded = info.extensions.len();
            println!("  · {loaded} extensions loaded");
        }
        Err(e) => {
            check(
                &mut ok,
                false,
                &format!("embedded PHP failed to start: {e}"),
            );
        }
    }

    // Platform / io_uring (prod is Linux).
    println!();
    platform_check();

    if let Some(dir) = app {
        println!();
        app_checks(&mut ok, &dir);
    }

    println!();
    if ok {
        println!("✓ all critical checks passed");
    } else {
        println!("✗ critical checks failed — see above");
    }
    ok
}

/// Where a value came from, because that is half the answer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A real environment variable — what the application will actually see.
    Process,
    /// The app's `.env` file, which loses to a real variable of the same name.
    DotEnv,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Process => "environment",
            Source::DotEnv => ".env",
        }
    }
}

/// Resolve a variable the way Laravel will, and say which source won.
///
/// Laravel's Dotenv **does not overwrite a variable that already exists**, so in any
/// container deployment the real environment wins and `.env` is the lower-priority source.
/// Reading only `.env` — which this did until 1.4.14 — means reporting on the source that
/// loses, confidently. On the deployment it was written against the two happened to agree,
/// which is luck: that compose file sets `APP_URL` and `DB_HOST` precisely because `.env`
/// says something else.
///
/// Run inside the container (`docker compose exec askr askr doctor --app …`) this sees the
/// same environment the workers do. Run from the host against a containerised server it
/// does not, and cannot — so every reported value names its source and the reader can tell.
fn resolve(
    env_file: &std::collections::HashMap<String, String>,
    key: &str,
) -> Option<(String, Source)> {
    // Existence, not emptiness, is what Laravel's Dotenv checks: it skips any variable
    // already present in the environment, so an *empty* real variable wins over a populated
    // `.env` line and the application sees the empty string. Falling back to `.env` here
    // would report a value the app will never use.
    //
    // That case is not hypothetical. A compose file with `TOKEN: ${TOKEN}` and an empty
    // entry in its own `.env` exports an empty variable — which is how an admin plane ended
    // up unauthenticated while the file it was configured from looked populated.
    if let Some(v) = std::env::var_os(key) {
        let v = v.to_string_lossy().into_owned();
        return if v.is_empty() {
            None // set, but empty: the app sees nothing, and neither should we
        } else {
            Some((v, Source::Process))
        };
    }
    env_file
        .get(key)
        .filter(|v| !v.is_empty())
        .map(|v| (v.clone(), Source::DotEnv))
}

/// Read `KEY=value` pairs from an app's `.env`, with the quoting Laravel allows.
///
/// Deliberately not a full dotenv implementation: this reads it to *report*, and a value
/// it misparses produces a wrong warning rather than wrong behaviour.
fn read_env(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
            .unwrap_or(v);
        out.insert(k.trim().to_string(), v.to_string());
    }
    out
}

/// Queue names the app actually dispatches to, from `onQueue('x')` and `$queue = 'x'`.
///
/// A grep, not a parser — it can miss a name built at runtime, so the report says "found"
/// rather than "all". Even so, this is the check that would have saved the longest
/// afternoon: an app sending every notification to `onQueue('mail')` while the worker
/// polled `default`, with no error anywhere and jobs quietly ageing in the ring.
fn queue_names_in(dir: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![dir.join("app")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("php") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            for (pat, skip) in [("onQueue(", 8usize), ("$queue = ", 9usize)] {
                let mut rest = text.as_str();
                while let Some(i) = rest.find(pat) {
                    rest = &rest[i + skip..];
                    let bytes = rest.as_bytes();
                    let Some(q) = bytes.first().copied() else {
                        break;
                    };
                    if q != b'\'' && q != b'"' {
                        continue; // a variable or expression, not a literal
                    }
                    if let Some(end) = rest[1..].find(q as char) {
                        let name = &rest[1..=end];
                        if !name.is_empty()
                            && name.len() < 64
                            && name
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                            && !out.iter().any(|o| o == name)
                        {
                            out.push(name.to_string());
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Does this application's configuration match the environment it will run in?
fn app_checks(ok: &mut bool, dir: &std::path::Path) {
    println!("app: {}", dir.display());

    if !dir.join("composer.json").is_file() {
        check(ok, false, "no composer.json here — is this the app root?");
        return;
    }
    let env_file = read_env(&dir.join(".env"));
    // Values are resolved the way the application will resolve them: a real environment
    // variable beats `.env`. `get` keeps the value, `src` says where it came from.
    let get = |k: &str| -> Option<String> { resolve(&env_file, k).map(|(v, _)| v) };
    let src = |k: &str| -> &'static str {
        resolve(&env_file, k)
            .map(|(_, s)| s.label())
            .unwrap_or("unset")
    };
    if env_file.is_empty() && std::env::var("APP_ENV").is_err() {
        note("no .env here and no APP_ENV in the environment — reading what little there is");
    }

    // --- shared-memory drivers need slots, and fail silently without them -------------
    note(&format!(
        "reading configuration from: {} (a real environment variable beats .env, which is \
         how Laravel resolves them too)",
        [
            "APP_ENV",
            "SESSION_DRIVER",
            "QUEUE_CONNECTION",
            "MAIL_MAILER"
        ]
        .iter()
        .map(|k| format!("{k}={}", src(k)))
        .collect::<Vec<_>>()
        .join(" ")
    ));
    let session = get("SESSION_DRIVER").unwrap_or_default();
    let cache = get("CACHE_STORE")
        .or_else(|| get("CACHE_DRIVER"))
        .unwrap_or_default();
    if session == "askr" {
        note("SESSION_DRIVER=askr — needs --cache-large-slots (sessions exceed 4 KB)");
        note("  without slots, sessions vanish silently and every form POST answers 419");
    }
    if cache == "askr" {
        note("CACHE_STORE=askr — needs --cache-slots");
    }

    // --- the queue, which is where silence has cost the most --------------------------
    let conn = get("QUEUE_CONNECTION").unwrap_or_else(|| "sync".to_string());
    if conn == "askr" {
        note("QUEUE_CONNECTION=askr — needs --queue-slots *and* a worker");
        note("  --queue-slots alone accepts jobs that nothing consumes: no error, no mail");
        let names = queue_names_in(dir);
        if !names.is_empty() {
            let polled: Vec<String> = std::env::var("ASKR_QUEUE")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let polled = if polled.is_empty() {
                vec!["default".to_string()]
            } else {
                polled
            };
            println!("  queue names found in app/: {}", names.join(", "));
            println!("  this worker would poll:    {}", polled.join(", "));
            let missed: Vec<&String> = names.iter().filter(|n| !polled.contains(n)).collect();
            if !missed.is_empty() {
                check(
                    ok,
                    false,
                    &format!(
                        "jobs dispatched to {} would never be processed — set \
                         ASKR_QUEUE={},default",
                        missed
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        names.join(",")
                    ),
                );
            } else {
                note("  every queue the app uses is polled — nothing would sit unclaimed");
            }
        }
    }

    // --- mail, where "looks configured" is the failure mode ---------------------------
    match get("MAIL_MAILER").as_deref() {
        Some("log") | Some("array") => {
            note("MAIL_MAILER writes mail to the log — nothing leaves the building")
        }
        Some("resend") => {
            let key = get("RESEND_KEY").or_else(|| get("RESEND_API_KEY"));
            check(
                ok,
                key.is_some(),
                if key.is_some() {
                    "MAIL_MAILER=resend with an API key present"
                } else {
                    "MAIL_MAILER=resend but neither RESEND_KEY nor RESEND_API_KEY is set \
                     — Laravel expects RESEND_KEY while Resend's own docs say \
                     RESEND_API_KEY, so this looks configured and sends nothing"
                },
            );
        }
        Some("smtp") => {
            let host = get("MAIL_HOST").is_some();
            check(ok, host, "MAIL_MAILER=smtp with MAIL_HOST set");
        }
        Some(other) => note(&format!("MAIL_MAILER={other}")),
        None => note("MAIL_MAILER unset — Laravel defaults apply"),
    }

    // --- scheduled ->command() tasks need a php binary this image does not have -------
    // Anchored on Laravel's scheduler, not on the method name.
    //
    // `command()` is an extremely common name. The first version of this looked for
    // `->command(` or `::command(` anywhere, and on the app it was written against the only
    // match was `Artisan::command()` — which *defines* a console command and schedules
    // nothing. It reported that scheduled tasks would fail on an app with no scheduled
    // tasks in those files at all. The conclusion happened to be true for other reasons,
    // which is worse than being wrong: right answer, false evidence, and it looks verified.
    //
    // `bootstrap/app.php` is included because Laravel 11+ puts scheduling in
    // `withSchedule(function (Schedule $schedule) { $schedule->command(...) })`.
    let scheduled = [
        "routes/console.php",
        "app/Console/Kernel.php",
        "bootstrap/app.php",
    ]
    .iter()
    .filter_map(|f| std::fs::read_to_string(dir.join(f)).ok())
    .any(|t| t.contains("Schedule::command(") || t.contains("schedule->command("));
    if scheduled {
        let php = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .any(|p| std::path::Path::new(p).join("php").is_file());
        check(
            ok,
            php,
            if php {
                "scheduled ->command() tasks have a php binary to shell out to"
            } else {
                "scheduled ->command() tasks shell out to `php`, which is not on PATH \
                 here — PHP is compiled into Askr, so every such task fails with exit \
                 code 127. Use ->call() instead, or install a php CLI."
            },
        );
    }

    // --- URLs that must match what the browser asks for ------------------------------
    if let Some(url) = get("APP_URL") {
        if url.contains("localhost") || url.contains(".test") {
            note(&format!(
                "APP_URL is {url} — every generated link and asset points there"
            ));
        }
    }
}

/// Parse the leading `major.minor` from a PHP version string like `8.5.8`.
fn php_minor(version: &str) -> Option<(u32, u32)> {
    let mut it = version.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next()?.parse().ok()?;
    Some((maj, min))
}

fn probe_php(ini: Option<String>) -> Result<PhpInfo, String> {
    // The interpreter is non-Send, so probe it on its own thread.
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        if let Some(ini) = ini {
            std::env::set_var("ASKR_PHP_INI", ini);
        }
        let result = (|| {
            let mut php = Interpreter::new().map_err(|e| e.to_string())?;
            let out = php
                .eval(
                    r#"echo PHP_VERSION . "\n" . (PHP_ZTS ? "1" : "0") . "\n" . implode(",", get_loaded_extensions());"#,
                )
                .map_err(|e| e.to_string())?;
            Ok::<String, String>(out.output)
        })();
        let _ = tx.send(result);
    });

    let raw = rx.recv().map_err(|_| "probe thread died".to_string())??;
    let mut lines = raw.splitn(3, '\n');
    let version = lines.next().unwrap_or("").trim().to_string();
    let zts = lines.next().unwrap_or("0").trim() == "1";
    let extensions = lines
        .next()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(PhpInfo {
        version,
        zts,
        extensions,
    })
}

#[cfg(target_os = "linux")]
fn platform_check() {
    println!("platform: linux");
    // io_uring appeared in 5.1; report the running kernel.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } == 0 {
        let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let ok = kernel_at_least(&release, 5, 1);
        mark(ok, &format!("kernel {release} (io_uring needs ≥ 5.1)"));
    }

    // Actually probe io_uring — a recent kernel can still have it disabled via
    // `sysctl kernel.io_uring_disabled`. Not being available isn't fatal: Askr
    // falls back to the epoll/tokio I/O path.
    match probe_io_uring() {
        Ok(()) => mark(true, "io_uring: available (probed io_uring_setup)"),
        Err(reason) => mark(
            true,
            &format!("io_uring: unavailable ({reason}) — using the epoll/tokio path"),
        ),
    }
}

/// Probe io_uring by attempting `io_uring_setup(2)`; closes the ring on success.
#[cfg(target_os = "linux")]
fn probe_io_uring() -> Result<(), String> {
    // A zeroed `struct io_uring_params` (120 bytes on all current ABIs).
    let mut params = [0u8; 120];
    // SAFETY: raw syscall with 1 SQ entry and a correctly-sized params buffer.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            1 as libc::c_uint,
            params.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ret >= 0 {
        unsafe { libc::close(ret as libc::c_int) };
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        // ENOSYS = kernel too old; EPERM = disabled by sysctl/seccomp.
        Err(err.to_string())
    }
}

#[cfg(target_os = "linux")]
fn kernel_at_least(release: &str, major: u32, minor: u32) -> bool {
    let mut it = release.split(['.', '-']);
    let maj: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let min: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (maj, min) >= (major, minor)
}

#[cfg(not(target_os = "linux"))]
fn platform_check() {
    let os = std::env::consts::OS;
    println!("platform: {os}");
    mark(
        true,
        "io_uring: n/a on this OS (dev target; production is Linux with io_uring)",
    );
}

fn check(ok: &mut bool, pass: bool, label: &str) {
    mark(pass, label);
    if !pass {
        *ok = false;
    }
}

fn mark(pass: bool, label: &str) {
    println!("  {} {label}", if pass { "✓" } else { "✗" });
}

/// An observation, not a verified check.
///
/// A `✓` on "this needs --cache-large-slots" claims something was confirmed when nothing
/// was — doctor cannot see the flags a later `serve` will get. A tick that means "noted"
/// teaches you to skim ticks, which defeats the point of a tool built to break silence.
fn note(label: &str) {
    println!("  • {label}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.env` is the source that *loses*. Laravel's Dotenv skips a variable that already
    /// exists, so in any container deployment the real environment decides — and doctor
    /// reading only `.env` meant reporting confidently on the wrong one.
    #[test]
    fn a_real_environment_variable_beats_dot_env() {
        let mut file = std::collections::HashMap::new();
        file.insert("ASKR_T_QUEUE".to_string(), "sync".to_string());

        let (v, s) = resolve(&file, "ASKR_T_QUEUE").expect("from .env when nothing is set");
        assert_eq!(v, "sync");
        assert!(s == Source::DotEnv && s.label() == ".env");

        // SAFETY: single-threaded test, and the name is unique to this test.
        unsafe { std::env::set_var("ASKR_T_QUEUE", "askr") };
        let (v, s) = resolve(&file, "ASKR_T_QUEUE").expect("the environment wins");
        assert_eq!(v, "askr");
        assert_eq!(s.label(), "environment");

        // Set but empty is what the application will see — not the .env value behind it.
        // This is the shape that left an admin plane unauthenticated while its .env looked
        // populated: `TOKEN: ${TOKEN}` in compose with an empty entry in compose's own .env.
        unsafe { std::env::set_var("ASKR_T_QUEUE", "") };
        assert!(
            resolve(&file, "ASKR_T_QUEUE").is_none(),
            "an empty real variable must not fall back to .env — Dotenv would not overwrite it"
        );

        unsafe { std::env::remove_var("ASKR_T_QUEUE") };
        assert!(
            resolve(&file, "ASKR_T_QUEUE").is_some(),
            "back to .env once removed"
        );
    }

    #[test]
    fn dot_env_quoting_is_handled() {
        let dir = std::env::temp_dir().join(format!(
            "askr-doctor-env-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env");
        std::fs::write(
            &f,
            "# comment\nPLAIN=one\nQUOTED=\"two words\"\nSINGLE='three'\nEMPTY=\nNOEQUALS\n",
        )
        .unwrap();
        let m = read_env(&f);
        assert_eq!(m.get("PLAIN").map(String::as_str), Some("one"));
        assert_eq!(m.get("QUOTED").map(String::as_str), Some("two words"));
        assert_eq!(m.get("SINGLE").map(String::as_str), Some("three"));
        assert_eq!(m.get("EMPTY").map(String::as_str), Some(""));
        assert!(!m.contains_key("NOEQUALS"));
        assert!(!m.contains_key("# comment"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
