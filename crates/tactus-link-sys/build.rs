//! Build script for `tactus-link-sys` (Ableton Link C99 `abl_link` FFI bindings).
//!
//! Resolution order (strictly offline; never runs `git` or downloads sources):
//! 1. `ABL_LINK_LIB_DIR`: explicit prebuilt library directory (Yocto offline BitBake build).
//!    Optional overrides: `ABL_LINK_LIB_NAME` (default `abl_link`), `ABL_LINK_LINK_KIND`
//!    (`static` or `dylib`, auto-detected from `lib<name>.a`).
//! 2. `pkg-config`: probes `abl-link` and `abl_link` in the active sysroot/distro environment.
//! 3. Vendored C++ build: compiles `vendor/ableton-link` (or `ABL_LINK_VENDOR_DIR`) via `cc::Build`
//!    with bundled standalone Asio headers (`ASIO_STANDALONE=1`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    for var in [
        "ABL_LINK_LIB_DIR",
        "ABL_LINK_LIB_NAME",
        "ABL_LINK_LINK_KIND",
        "ABL_LINK_VENDOR_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS not set");
    let target_pointer_width =
        env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_else(|_| "64".to_string());

    // 1. Explicit prebuilt library directory override (Yocto offline BitBake build path).
    if let Some(lib_dir_os) = env::var_os("ABL_LINK_LIB_DIR").filter(|v| !v.is_empty()) {
        let lib_dir = PathBuf::from(&lib_dir_os);
        if !lib_dir.is_dir() {
            panic!(
                "ABL_LINK_LIB_DIR={} does not exist or is not a directory",
                lib_dir.display()
            );
        }

        let lib_name = env::var("ABL_LINK_LIB_NAME")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "abl_link".to_string());
        let link_kind = env::var("ABL_LINK_LINK_KIND")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                if lib_dir.join(format!("lib{lib_name}.a")).exists() {
                    "static".to_string()
                } else {
                    "dylib".to_string()
                }
            });

        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib={link_kind}={lib_name}");

        if link_kind == "static" {
            emit_cpp_stdlib_and_platform_libs(&target_os, &target_pointer_width);
        }
        return;
    }

    // 2. System/sysroot pkg-config probe (Yocto / distro prebuilt package path).
    for pkg_name in ["abl-link", "abl_link"] {
        if pkg_config::Config::new()
            .cargo_metadata(true)
            .probe(pkg_name)
            .is_ok()
        {
            return;
        }
    }

    // 3. Vendored C++ compilation fallback (zero network access, zero git calls).
    compile_vendored_abl_link(&target_os, &target_pointer_width);
}

fn emit_cpp_stdlib_and_platform_libs(target_os: &str, target_pointer_width: &str) {
    match target_os {
        "macos" | "ios" => {
            println!("cargo:rustc-link-lib=c++");
        }
        "linux" => {
            println!("cargo:rustc-link-lib=stdc++");
            println!("cargo:rustc-link-lib=pthread");
            if target_pointer_width == "32" {
                println!("cargo:rustc-link-lib=atomic");
            }
        }
        other => panic!("unsupported target OS for abl_link static linking: {other}"),
    }
}

fn compile_vendored_abl_link(target_os: &str, target_pointer_width: &str) {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vendor_dir = env::var_os("ABL_LINK_VENDOR_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../vendor/ableton-link"));

    let abl_link_cpp = vendor_dir.join("extensions/abl_link/src/abl_link.cpp");
    let abl_link_h = vendor_dir.join("extensions/abl_link/include/abl_link.h");
    if !abl_link_cpp.is_file() || !abl_link_h.is_file() {
        panic!(
            "vendored Ableton Link sources not found at {}. \
             Populate vendor/ableton-link offline (with submodules initialized) or set ABL_LINK_LIB_DIR.",
            vendor_dir.display()
        );
    }

    let asio_include_dir = find_bundled_asio_include_dir(&vendor_dir).unwrap_or_else(|| {
        panic!(
            "bundled networking headers (modules/asio-standalone) not found under {}. \
             Run `git -C vendor/ableton-link submodule update --init --recursive`.",
            vendor_dir.display()
        )
    });
    let version_hpp = asio_include_dir.join("asio/version.hpp");

    println!("cargo:rerun-if-changed={}", abl_link_cpp.display());
    println!("cargo:rerun-if-changed={}", abl_link_h.display());
    println!("cargo:rerun-if-changed={}", version_hpp.display());

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .warnings(false)
        .file(&abl_link_cpp)
        .include(vendor_dir.join("include"))
        .include(vendor_dir.join("extensions/abl_link/include"))
        .include(&asio_include_dir);

    let is_link_4 = vendor_dir.join("include/ableton/LinkAudio.hpp").is_file();
    if is_link_4 {
        let asio_namespace = parse_asio_namespace(&version_hpp);
        build
            .std("c++17")
            .define("ASIO_NO_TYPEID", "1")
            .define("ASIO_STANDALONE", "1")
            .define("ASIO_VERSION_NAMESPACE", asio_namespace.as_str());
    } else {
        build.std("c++11").define("ASIO_STANDALONE", "1");
    }

    match target_os {
        "macos" | "ios" => {
            build
                .define("LINK_PLATFORM_UNIX", "1")
                .define("LINK_PLATFORM_MACOSX", "1");
        }
        "linux" => {
            build
                .define("LINK_PLATFORM_UNIX", "1")
                .define("LINK_PLATFORM_LINUX", "1");
            println!("cargo:rustc-link-lib=pthread");
            if target_pointer_width == "32" {
                println!("cargo:rustc-link-lib=atomic");
            }
        }
        other => panic!("unsupported target OS for vendored abl_link build: {other}"),
    }

    build.compile("abl_link");
}

fn find_bundled_asio_include_dir(vendor_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        vendor_dir.join("modules/asio-standalone/include"),
        vendor_dir.join("modules/asio-standalone/asio/include"),
    ];
    candidates
        .into_iter()
        .find(|dir| dir.join("asio/version.hpp").is_file())
}

fn parse_asio_namespace(version_hpp: &Path) -> String {
    let contents = fs::read_to_string(version_hpp)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", version_hpp.display()));
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#define ASIO_VERSION ") {
            let digits: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(version) = digits.parse::<u32>() {
                let major = version / 100_000;
                let minor = (version / 100) % 1000;
                let sub = version % 100;
                return format!("link_asio_{major}_{minor}_{sub}");
            }
        }
    }
    panic!(
        "could not parse ASIO_VERSION from {}",
        version_hpp.display()
    );
}
