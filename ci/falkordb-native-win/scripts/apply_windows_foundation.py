#!/usr/bin/env python3
from pathlib import Path
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: apply_windows_foundation.py PATH_TO_FALKORDB")

root = Path(sys.argv[1]).resolve()

def patch_module_init():
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


def patch_graphblas_bindings():
    """Replace the Darwin libc FILE layout embedded by upstream bindgen."""
    p = root / "graph/src/graph/graphblas/mod.rs"
    s = p.read_text(encoding="utf-8")

    start_marker = "pub type __int64_t = ::std::os::raw::c_longlong;"
    end_marker = "pub type FILE = __sFILE;"
    start = s.find(start_marker)
    end = s.find(end_marker)
    if start < 0 or end < 0 or end < start:
        if "pub struct FILE {\n    _opaque: [u8; 0],\n}" in s:
            return
        raise RuntimeError("graphblas/mod.rs: Darwin FILE binding block not found")

    end += len(end_marker)
    replacement = (
        "// Cross-platform opaque C FILE type. GraphBLAS only exposes this behind\n"
        "// pointers in diagnostic print functions; FalkorDB never dereferences it.\n"
        "#[repr(C)]\n"
        "pub struct FILE {\n"
        "    _opaque: [u8; 0],\n"
        "}"
    )
    s = s[:start] + replacement + s[end:]

    if "__darwin_" in s or "__sFILE" in s:
        raise RuntimeError("graphblas/mod.rs: Darwin FILE definitions remain after patch")

    p.write_text(s, encoding="utf-8")

def patch_graphblas_matrix():
    p = root / "graph/src/graph/graphblas/matrix.rs"
    s = p.read_text(encoding="utf-8")

    old_call = "        omp_set_num_threads(n);"
    new_call = "        #[cfg(not(windows))]\n        omp_set_num_threads(n);"
    if old_call in s and new_call not in s:
        s = s.replace(old_call, new_call, 1)

    old_decl = 'unsafe extern "C" {\n    fn omp_set_num_threads(num_threads: i32);\n}'
    new_decl = '#[cfg(not(windows))]\nunsafe extern "C" {\n    fn omp_set_num_threads(num_threads: i32);\n}'
    if old_decl in s and new_decl not in s:
        s = s.replace(old_decl, new_decl, 1)

    p.write_text(s, encoding="utf-8")

def install_native_index():
    source = Path(__file__).resolve().parent.parent / "patches" / "native_index_mod.rs"
    if not source.exists():
        raise RuntimeError(f"native index module not found: {source}")
    target = root / "graph/src/index/mod.rs"
    target.write_text(source.read_text(encoding="utf-8"), encoding="utf-8")
    print("installed pure-Rust native graph index backend")

def patch_graph_build():
    p = root / "graph/build.rs"
    s = p.read_text(encoding="utf-8")

    old = '        std::os::unix::fs::symlink(src, &main_a).expect("failed to create libredisearch.a symlink");'
    new = '        create_archive_alias(&src, &main_a).expect("failed to create libredisearch.a alias");'
    if old in s:
        s = s.replace(old, new, 1)

    marker = "/// RediSearch's Rust archive carries a second copy of the `redis-module` crate"
    helper = '''
#[cfg(unix)]
fn create_archive_alias(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn create_archive_alias(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

'''
    if "fn create_archive_alias" not in s:
        idx = s.find(marker)
        if idx < 0:
            raise RuntimeError("graph/build.rs: helper insertion marker not found")
        s = s[:idx] + helper + s[idx:]

    rs_marker = "    // ---- RediSearch 8.6, embedded as a static library ----"
    skip = '''    // Native Windows bring-up: non-index smoke tests link a fail-closed
    // compatibility archive instead of the real RediSearch backend. Every
    // symbol in that archive aborts if called, so an accidental index path can
    // never silently return incorrect data.
    println!("cargo:rerun-if-env-changed=FALKORDB_NATIVE_REDISEARCH_SHIM_DIR");
    if let Ok(dir) = std::env::var("FALKORDB_NATIVE_REDISEARCH_SHIM_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
        println!("cargo:rustc-link-lib=static=redisearch_shim");
        println!("cargo:warning=native bring-up: RediSearch index operations are disabled");
        return;
    }

    // Type-check-only fallback. This intentionally emits no RediSearch library.
    println!("cargo:rerun-if-env-changed=FALKORDB_SKIP_REDISEARCH");
    if std::env::var_os("FALKORDB_SKIP_REDISEARCH").is_some() {
        println!("cargo:warning=FALKORDB_SKIP_REDISEARCH set: skipping RediSearch link discovery");
        return;
    }

'''
    if "FALKORDB_NATIVE_REDISEARCH_SHIM_DIR" not in s:
        idx = s.find(rs_marker)
        if idx < 0:
            raise RuntimeError("graph/build.rs: RediSearch marker not found")
        s = s[:idx] + skip + s[idx:]

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

    old_omp = '''    if libomp_static {
        println!("cargo:rustc-link-lib=static=omp");
    } else {
        println!("cargo:rustc-link-lib=omp");
    }'''
    new_omp = '''    println!("cargo:rerun-if-env-changed=FALKORDB_NATIVE_NO_OPENMP");
    if std::env::var_os("FALKORDB_NATIVE_NO_OPENMP").is_none() {
        if libomp_static {
            println!("cargo:rustc-link-lib=static=omp");
        } else {
            #[cfg(windows)]
            {
                let name = std::env::var("OPENMP_LIB_NAME")
                    .unwrap_or_else(|_| "libomp".to_string());
                println!("cargo:rerun-if-env-changed=OPENMP_LIB_NAME");
                println!("cargo:rustc-link-lib={name}");
            }
            #[cfg(not(windows))]
            println!("cargo:rustc-link-lib=omp");
        }
    }'''
    if old_omp in s:
        s = s.replace(old_omp, new_omp, 1)

    p.write_text(s, encoding="utf-8")

patch_module_init()
patch_graphblas_bindings()
patch_graphblas_matrix()
patch_graph_build()
install_native_index()
print("Windows foundation patches applied")
