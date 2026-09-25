#!/usr/bin/env python3
from pathlib import Path
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: apply_windows_foundation.py PATH_TO_FALKORDB")

root = Path(sys.argv[1]).resolve()


def patch_module_init() -> None:
    """The Redis shell is not our runtime, but keep the upstream root crate Windows-compilable."""
    p = root / "src/module_init.rs"
    s = p.read_text(encoding="utf-8")

    old = '''unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<unsafe extern "C" fn()>,
        parent: Option<unsafe extern "C" fn()>,
        child: Option<unsafe extern "C" fn()>,
    ) -> c_int;
}'''
    new = '''#[cfg(unix)]
unsafe extern "C" {
    fn pthread_atfork(
        prepare: Option<unsafe extern "C" fn()>,
        parent: Option<unsafe extern "C" fn()>,
        child: Option<unsafe extern "C" fn()>,
    ) -> c_int;
}'''
    if old in s and new not in s:
        s = s.replace(old, new, 1)

    old_call = '''        pthread_atfork(
            Some(crate::redis_type::pre_fork_prepare),
            None,
            Some(on_fork_child),
        );'''
    new_call = '''        #[cfg(unix)]
        pthread_atfork(
            Some(crate::redis_type::pre_fork_prepare),
            None,
            Some(on_fork_child),
        );'''
    if old_call in s and new_call not in s:
        s = s.replace(old_call, new_call, 1)

    p.write_text(s, encoding="utf-8")


def patch_graph_build() -> None:
    p = root / "graph/build.rs"
    s = p.read_text(encoding="utf-8")

    old = '        std::os::unix::fs::symlink(src, &main_a).expect("failed to create libredisearch.a symlink");'
    new = '        create_archive_alias(&src, &main_a).expect("failed to create libredisearch.a alias");'
    if old in s:
        s = s.replace(old, new, 1)

    marker = "/// RediSearch's Rust archive carries a second copy of the `redis-module` crate"
    helper = r'''
#[cfg(unix)]
fn create_archive_alias(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_archive_alias(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    // Windows symlinks may require Developer Mode/elevation. This alias is a
    // build artifact only, so copying is deterministic and permission-free.
    std::fs::copy(src, dst).map(|_| ())
}

'''
    if "fn create_archive_alias" not in s:
        idx = s.find(marker)
        if idx < 0:
            raise RuntimeError("graph/build.rs: helper insertion marker not found")
        s = s[:idx] + helper + s[idx:]

    # Expose LAGraph location to the standalone Windows host.
    old_lagraph = '''    let lagraph_dir = std::path::Path::new(&manifest_dir).join("../lagraph_lib");
    println!("cargo:rustc-link-search=native={}", lagraph_dir.display());
    println!("cargo:rustc-link-search=native=/data/lagraph_lib");'''
    new_lagraph = '''    let lagraph_dir = std::env::var("LAGRAPH_LIB_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::Path::new(&manifest_dir).join("../lagraph_lib"));
    println!("cargo:rerun-if-env-changed=LAGRAPH_LIB_DIR");
    println!("cargo:rustc-link-search=native={}", lagraph_dir.display());
    #[cfg(not(windows))]
    println!("cargo:rustc-link-search=native=/data/lagraph_lib");'''
    if old_lagraph in s:
        s = s.replace(old_lagraph, new_lagraph, 1)

    # SuiteSparse deliberately gives its static MSVC libraries `_static`
    # output names to avoid colliding with import libraries for DLL builds.
    # FalkorDB upstream currently links the Unix names unconditionally.
    old_native_libs = """    println!("cargo:rustc-link-lib=static=lagraphx");
    println!("cargo:rustc-link-lib=static=lagraph");
    println!("cargo:rustc-link-lib=static=graphblas");"""
    new_native_libs = """    #[cfg(windows)]
    {
        println!("cargo:rustc-link-lib=static=lagraphx_static");
        println!("cargo:rustc-link-lib=static=lagraph_static");
        println!("cargo:rustc-link-lib=static=graphblas_static");
    }
    #[cfg(not(windows))]
    {
        println!("cargo:rustc-link-lib=static=lagraphx");
        println!("cargo:rustc-link-lib=static=lagraph");
        println!("cargo:rustc-link-lib=static=graphblas");
    }"""
    if old_native_libs in s:
        s = s.replace(old_native_libs, new_native_libs, 1)

    # MSVC's OpenMP runtime is vcomp. GraphBLAS v10.4.1 fixed cl/OpenMP on
    # Windows; v10.5.0 is our pinned native dependency.
    old_omp = '''    if libomp_static {
        println!("cargo:rustc-link-lib=static=omp");
    } else {
        println!("cargo:rustc-link-lib=omp");
    }'''
    new_omp = '''    if libomp_static {
        println!("cargo:rustc-link-lib=static=omp");
    } else {
        #[cfg(windows)]
        {
            let name = std::env::var_os("OPENMP_LIB_NAME").unwrap_or_else(|_| "vcomp".to_string());
            println!("cargo:rerun-if-env-changed=OPENMP_LIB_NAME");
            println!("cargo:rustc-link-lib={name}");
        }
        #[cfg(not(windows))]
        println!("cargo:rustc-link-lib=omp");
    }'''
    if old_omp in s:
        s = s.replace(old_omp, new_omp, 1)

    # During bring-up, do not discover/link the Unix-centric RediSearch build.
    # The native host supplies aborting link stubs and rejects index plans.
    rs_marker = "    // ---- RediSearch 8.6, embedded as a static library ----"
    skip = r'''    // Native Windows bring-up only. This permits parser/planner/runtime/MVCC
    // linking before the current RediSearch backend is replaced by the in-repo
    // graph/src/index/falkordb backend. The native host supplies aborting link
    // stubs and explicitly rejects every plan that requires an index backend.
    println!("cargo:rerun-if-env-changed=FALKORDB_SKIP_REDISEARCH");
    if cfg!(windows) || std::env::var_os("FALKORDB_SKIP_REDISEARCH").is_some() {
        println!("cargo:warning=native Windows bring-up: skipping RediSearch link discovery");
        return;
    }

'''
    if "native Windows bring-up: skipping RediSearch link discovery" not in s:
        idx = s.find(rs_marker)
        if idx < 0:
            raise RuntimeError("graph/build.rs: RediSearch marker not found")
        s = s[:idx] + skip + s[idx:]

    p.write_text(s, encoding="utf-8")


def patch_graphblas_runtime() -> None:
    p = root / "graph/src/graph/graphblas/matrix.rs"
    s = p.read_text(encoding="utf-8")

    old = '''        #[cfg(not(feature = "prejit_harvest"))]
        let (jit_level, jit_name) = (GxB_JIT_Control::GxB_JIT_RUN, "JIT_RUN");'''
    new = '''        #[cfg(all(not(feature = "prejit_harvest"), windows))]
        let (jit_level, jit_name) = (GxB_JIT_Control::GxB_JIT_OFF, "JIT_OFF (Windows bring-up)");
        #[cfg(all(not(feature = "prejit_harvest"), not(windows)))]
        let (jit_level, jit_name) = (GxB_JIT_Control::GxB_JIT_RUN, "JIT_RUN");'''
    if old in s:
        s = s.replace(old, new, 1)

    p.write_text(s, encoding="utf-8")


patch_module_init()
patch_graph_build()
patch_graphblas_runtime()
print("Windows foundation patches applied")
