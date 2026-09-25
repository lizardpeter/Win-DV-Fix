#!/usr/bin/env python3
from pathlib import Path
import sys

if len(sys.argv) != 2:
    raise SystemExit("usage: check_patch_expectations.py PATH_TO_FALKORDB")

root = Path(sys.argv[1]).resolve()
build = (root / "graph/build.rs").read_text(encoding="utf-8")
matrix = (root / "graph/src/graph/graphblas/matrix.rs").read_text(encoding="utf-8")
module_init = (root / "src/module_init.rs").read_text(encoding="utf-8")

checks = {
    "Windows graphblas static name": "cargo:rustc-link-lib=static=graphblas_static" in build,
    "Windows lagraph static name": "cargo:rustc-link-lib=static=lagraph_static" in build,
    "Windows lagraphx static name": "cargo:rustc-link-lib=static=lagraphx_static" in build,
    "Windows OpenMP override": "OPENMP_LIB_NAME" in build and '"vcomp"' in build,
    "RediSearch bring-up skip": "FALKORDB_SKIP_REDISEARCH" in build,
    "portable archive alias": "fn create_archive_alias" in build,
    "Windows JIT disabled": "JIT_OFF (Windows bring-up)" in matrix,
    "pthread fork registration gated": "#[cfg(unix)]\n        pthread_atfork" in module_init,
}

failed = [name for name, ok in checks.items() if not ok]
for name, ok in checks.items():
    print(f"{'PASS' if ok else 'FAIL'}: {name}")
if failed:
    raise SystemExit("Windows foundation patch incomplete: " + ", ".join(failed))
