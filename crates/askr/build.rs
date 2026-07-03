//! Emit the rpath to `libphp.dylib` for the final binary.
//!
//! `askr-php` links libphp and propagates the link-search/link-lib to us, but
//! `cargo:rustc-link-arg` (the rpath) does *not* propagate to dependent crates.
//! So the binary needs its own rpath pointing at the libphp install.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace = manifest.parent().unwrap().parent().unwrap();

    let php_config = std::env::var("ASKR_PHP_CONFIG").unwrap_or_else(|_| {
        workspace
            .join("vendor/php-build/install/bin/php-config")
            .to_string_lossy()
            .into_owned()
    });

    println!("cargo:rerun-if-env-changed=ASKR_PHP_CONFIG");

    if let Ok(out) = Command::new(&php_config).arg("--prefix").output() {
        if out.status.success() {
            let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("cargo:rustc-link-arg=-Wl,-rpath,{prefix}/lib");
        }
    }
}
