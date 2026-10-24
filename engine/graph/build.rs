//! Builds the vendored native libraries the engine links statically:
//! SuiteSparse:GraphBLAS (`../GraphBLAS`) and LAGraph (`../LAGraph`).
//!
//! `cargo build` needs only a Rust toolchain, CMake and a C compiler (MSVC
//! Build Tools on Windows, gcc or clang elsewhere). Nothing is downloaded or
//! looked up on the system: both libraries are compiled from the sources in
//! this repository into `OUT_DIR` and linked into the final executable.
//!
//! Configuration matches the shipped Windows build:
//! - static libraries only;
//! - compact GraphBLAS (no FactoryKernels), no JIT, no OpenMP, no CUDA —
//!   the executable must not compile or load code at run time, and must not
//!   depend on an OpenMP runtime DLL;
//! - always an optimized (`Release`) build, even for `cargo build` debug
//!   profiles: a debug GraphBLAS is orders of magnitude slower;
//! - on MSVC the C runtime matches Rust's (`/MT` when the crate is built
//!   with `+crt-static`, as the release scripts do).
//!
//! RediSearch is not built or linked: the engine uses the pure-Rust native
//! index in `src/index/mod.rs`.

use std::{
    env,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let engine_dir = manifest_dir
        .parent()
        .expect("graph crate lives under engine/")
        .to_path_buf();
    let graphblas_src = engine_dir.join("GraphBLAS");
    let lagraph_src = engine_dir.join("LAGraph");
    for dir in [&graphblas_src, &lagraph_src] {
        assert!(
            dir.join("CMakeLists.txt").exists(),
            "vendored native source missing: {}",
            dir.display()
        );
        // Cargo re-runs this script when anything under these trees changes.
        println!("cargo:rerun-if-changed={}", dir.display());
    }

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let msvc = target_env == "msvc";
    let crt_static = env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default()
        .split(',')
        .any(|f| f == "crt-static");

    let graphblas = build_graphblas(&graphblas_src, msvc, crt_static);
    let lagraph = build_lagraph(&lagraph_src, &graphblas, msvc, crt_static);

    // Order matters for single-pass linkers: LAGraph references GraphBLAS.
    link_archive(&lagraph, &["lagraphx_static", "lagraphx"]);
    link_archive(&lagraph, &["lagraph_static", "lagraph"]);
    link_archive(&graphblas, &["graphblas_static", "graphblas"]);

    if !msvc {
        // GraphBLAS uses libm (and pthreads for its internal locks).
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=pthread");
    }
}

/// Settings shared by both libraries.
fn base_config(
    src: &Path,
    msvc: bool,
    crt_static: bool,
) -> cmake::Config {
    let mut cfg = cmake::Config::new(src);
    cfg.profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_STATIC_LIBS", "ON")
        .define("BUILD_TESTING", "OFF")
        .define("SUITESPARSE_USE_FORTRAN", "OFF")
        .define("SUITESPARSE_USE_OPENMP", "OFF")
        .define("SUITESPARSE_USE_CUDA", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON");
    if msvc {
        cfg.static_crt(crt_static)
            .define("CMAKE_POLICY_DEFAULT_CMP0091", "NEW")
            .define(
                "CMAKE_MSVC_RUNTIME_LIBRARY",
                if crt_static {
                    "MultiThreaded"
                } else {
                    "MultiThreadedDLL"
                },
            )
            // Parallel compilation within the Visual Studio generator.
            .cflag("/MP");
    } else {
        // gcc 14 and clang 16+ made this an error by default; GraphBLAS
        // v10.5.0 still trips it in Source/extract/GB_I_inverse.c.
        cfg.cflag("-Wno-incompatible-pointer-types");
    }
    cfg
}

fn build_graphblas(
    src: &Path,
    msvc: bool,
    crt_static: bool,
) -> PathBuf {
    base_config(src, msvc, crt_static)
        .define("GRAPHBLAS_BUILD_STATIC_LIBS", "ON")
        .define("GRAPHBLAS_COMPACT", "ON")
        .define("GRAPHBLAS_USE_JIT", "OFF")
        .define("GRAPHBLAS_USE_OPENMP", "OFF")
        .define("GRAPHBLAS_USE_CUDA", "OFF")
        .build()
}

fn build_lagraph(
    src: &Path,
    graphblas: &Path,
    msvc: bool,
    crt_static: bool,
) -> PathBuf {
    let mut cfg = base_config(src, msvc, crt_static);
    cfg.define("LIBRARY_ONLY", "ON")
        .define("LAGRAPH_USE_OPENMP", "OFF")
        .define("CMAKE_PREFIX_PATH", graphblas)
        .define("GraphBLAS_DIR", graphblas.join("lib").join("cmake").join("GraphBLAS"));
    if msvc {
        // MSVC has no separate libm.
        cfg.define("NO_LIBM", "ON");
    }
    cfg.build()
}

/// Links the first archive found under `prefix` whose stem is one of
/// `names` (MSVC builds name them `*_static.lib`, others `lib*.a`).
fn link_archive(
    prefix: &Path,
    names: &[&str],
) {
    for dir in ["lib", "lib64"] {
        let dir = prefix.join(dir);
        for name in names {
            let found = [format!("{name}.lib"), format!("lib{name}.a")]
                .iter()
                .any(|file| dir.join(file).exists());
            if found {
                println!("cargo:rustc-link-search=native={}", dir.display());
                println!("cargo:rustc-link-lib=static={name}");
                return;
            }
        }
    }
    panic!(
        "none of {names:?} was produced under {}; see the CMake output above",
        prefix.display()
    );
}
