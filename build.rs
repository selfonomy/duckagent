use std::env;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(duckagent_vendored_bwrap)");
    println!("cargo:rerun-if-env-changed=DUCKAGENT_BWRAP_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=DUCKAGENT_SKIP_VENDORED_BWRAP");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_ALLOW_CROSS");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_SYSROOT_DIR");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" || env::var_os("DUCKAGENT_SKIP_VENDORED_BWRAP").is_some() {
        return;
    }

    if let Err(error) = build_vendored_bwrap() {
        panic!("failed to compile vendored bubblewrap for Linux target: {error}");
    }
}

fn build_vendored_bwrap() -> Result<(), String> {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|error| error.to_string())?);
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|error| error.to_string())?);
    let src_dir = resolve_bwrap_source_dir(&manifest_dir)?;

    for file in ["bubblewrap.c", "bind-mount.c", "network.c", "utils.c"] {
        println!("cargo:rerun-if-changed={}", src_dir.join(file).display());
    }

    let libcap = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libcap")
        .map_err(|error| format!("libcap not available via pkg-config: {error}"))?;

    let config_h = out_dir.join("config.h");
    std::fs::write(
        &config_h,
        r#"#pragma once
#define PACKAGE_STRING "bubblewrap 0.11.2 built at duckagent build-time"
"#,
    )
    .map_err(|error| format!("failed to write {}: {error}", config_h.display()))?;

    let mut build = cc::Build::new();
    build
        .file(src_dir.join("bubblewrap.c"))
        .file(src_dir.join("bind-mount.c"))
        .file(src_dir.join("network.c"))
        .file(src_dir.join("utils.c"))
        .include(&out_dir)
        .include(&src_dir)
        .define("_GNU_SOURCE", None)
        .define("main", Some("duckagent_bwrap_main"));
    for include_path in &libcap.include_paths {
        build.flag(format!("-idirafter{}", include_path.display()));
    }
    build.compile("duckagent_build_time_bwrap");
    for link_path in &libcap.link_paths {
        println!("cargo:rustc-link-search=native={}", link_path.display());
    }
    for lib in &libcap.libs {
        println!("cargo:rustc-link-lib={lib}");
    }
    println!("cargo:rustc-cfg=duckagent_vendored_bwrap");
    Ok(())
}

fn resolve_bwrap_source_dir(manifest_dir: &Path) -> Result<PathBuf, String> {
    if let Ok(path) = env::var("DUCKAGENT_BWRAP_SOURCE_DIR") {
        let src_dir = PathBuf::from(path);
        if src_dir.exists() {
            return Ok(src_dir);
        }
        return Err(format!(
            "DUCKAGENT_BWRAP_SOURCE_DIR was set but does not exist: {}",
            src_dir.display()
        ));
    }

    let vendor_dir = manifest_dir.join("src/sandbox/vendor/bubblewrap");
    if vendor_dir.exists() {
        return Ok(vendor_dir);
    }

    Err(format!(
        "expected vendored bubblewrap at {}, but it was not found",
        vendor_dir.display()
    ))
}
