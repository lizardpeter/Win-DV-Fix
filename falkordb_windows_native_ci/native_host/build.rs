fn main() {
    println!("cargo:rerun-if-changed=src/redisearch_link_stubs.c");

    if std::env::var_os("CARGO_FEATURE_REDISEARCH_LINK_STUBS").is_none() {
        return;
    }

    // The shim is deliberately link-only. Every symbol aborts if called.
    // This gives the Windows bring-up build a way to prove parser/planner/
    // runtime/MVCC/GraphBLAS operation for graphs that have no secondary
    // indexes, without silently pretending RediSearch works.
    cc::Build::new()
        .file("src/redisearch_link_stubs.c")
        .warnings(false)
        .compile("falkordb_redisearch_link_stubs");
}
