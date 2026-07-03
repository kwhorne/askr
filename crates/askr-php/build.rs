//! Build the PHP embed shim and link it against a locally-built `libphp`.
//!
//! The PHP install is discovered via a `php-config` binary. By default we look
//! for the one produced by the M0 build under `vendor/php-build/install`, but it
//! can be overridden with the `ASKR_PHP_CONFIG` env var to point at any
//! embed-enabled (non-ZTS, `--enable-embed`) PHP install.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // workspace root = crates/askr-php/../../
    let workspace = manifest.parent().unwrap().parent().unwrap();

    let php_config = std::env::var("ASKR_PHP_CONFIG").unwrap_or_else(|_| {
        workspace
            .join("vendor/php-build/install/bin/php-config")
            .to_string_lossy()
            .into_owned()
    });

    println!("cargo:rerun-if-changed=csrc/shim.c");
    println!("cargo:rerun-if-env-changed=ASKR_PHP_CONFIG");

    let includes = php_config_output(&php_config, "--includes");
    let prefix = php_config_output(&php_config, "--prefix");
    let prefix = prefix.trim();
    let libdir = format!("{prefix}/lib");

    // Compile the shim against PHP's headers.
    let mut build = cc::Build::new();
    build.file("csrc/shim.c");
    for inc in includes.split_whitespace() {
        if let Some(path) = inc.strip_prefix("-I") {
            build.include(path);
        }
    }
    // php-config --includes omits the SAPI dirs; php_embed.h lives there.
    build.include(format!("{prefix}/include/php/sapi/embed"));
    // PHP headers assume _GNU_SOURCE in a few spots.
    build.define("_GNU_SOURCE", None);
    build.warnings(false);
    build.compile("askr_php_shim");

    // Link the dynamic libphp and make the runtime loader find it via rpath.
    println!("cargo:rustc-link-search=native={libdir}");
    println!("cargo:rustc-link-lib=dylib=php");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{libdir}");
}

fn php_config_output(php_config: &str, arg: &str) -> String {
    let out = Command::new(php_config)
        .arg(arg)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{php_config} {arg}`: {e}"));
    assert!(
        out.status.success(),
        "`{php_config} {arg}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
