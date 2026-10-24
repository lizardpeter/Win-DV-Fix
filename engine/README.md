# Vendored engine sources

Everything the server executable is compiled from lives in this repository.
`cargo build` builds all of it; nothing is cloned or downloaded at build time
except Rust crates from crates.io (pinned by `native_host/Cargo.lock`).

| Directory | What | Upstream | Pinned at | License |
| --- | --- | --- | --- | --- |
| `graph/` | FalkorDB Rust graph engine (Cypher parser, planner, runtime, MVCC storage) | https://github.com/FalkorDB/FalkorDB (`graph/`) | `55204c94bb6c8bc1684ada3d712a61f73f324067` (2026-09-24) | SSPL-1.0 |
| `GraphBLAS/` | SuiteSparse:GraphBLAS sparse-matrix library (subset: core sources only) | https://github.com/DrTimothyAldenDavis/GraphBLAS | `v10.5.0` / `ea79e354877ad5e96fad13e161b8db6d661875e4` | Apache-2.0 |
| `LAGraph/` | LAGraph graph algorithms on GraphBLAS (subset: core sources only) | https://github.com/GraphBLAS/LAGraph | `v1.3.x` / `a96fc4e055b7af6dde5e8a3f4dbb45b87d4bca28` | BSD-2-Clause |

## Local changes

All local changes are ordinary commits on top of the pristine import, so
`git log -- engine/` shows each one. They are ours to maintain; upstream
fixes are brought in by diffing upstream against the pinned commit above.

The import commits are the exact upstream files, minus parts this build never
compiles (RediSearch bindings, the GraphBLAS MATLAB/CUDA/factory-kernel
trees, tests, docs, demos and data sets).
