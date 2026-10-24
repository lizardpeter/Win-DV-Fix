//! Stages the OpenAI tunnel runtime that `server.exe` embeds on Windows.
//!
//! The pinned release archive is vendored in
//! `third_party/openai-tunnel-client/`. This script verifies its SHA-256 and
//! extracts the runtime executable, LICENSE and NOTICE into `OUT_DIR`, where
//! `src/bin/server.rs` `include_bytes!`s them. Nothing is downloaded.

use std::{
    env,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

const ARCHIVE: &str = "tunnel-client-runtime-v0.0.15-windows-amd64.zip";
const ARCHIVE_SHA256: &str = "aa5ddb14dddd602fa59f3e6f4401aa8a79a218e341466226b7434127dff65dbc";
/// (name inside the archive, name written to OUT_DIR)
const MEMBERS: &[(&str, &str)] = &[
    ("tunnel-client-runtime.exe", "tunnel-client-runtime.exe"),
    ("LICENSE", "openai-tunnel-LICENSE.txt"),
    ("NOTICE", "openai-tunnel-NOTICE.txt"),
];

fn main() {
    // Only Windows builds embed the tunnel runtime. FALKORDB_STAGE_TUNNEL_RUNTIME=1
    // stages it on any target, to exercise this script off Windows.
    println!("cargo:rerun-if-env-changed=FALKORDB_STAGE_TUNNEL_RUNTIME");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        && env::var_os("FALKORDB_STAGE_TUNNEL_RUNTIME").is_none()
    {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let archive = manifest_dir
        .join("..")
        .join("third_party")
        .join("openai-tunnel-client")
        .join(ARCHIVE);
    println!("cargo:rerun-if-changed={}", archive.display());

    let bytes = fs::read(&archive)
        .unwrap_or_else(|e| panic!("read vendored tunnel archive {}: {e}", archive.display()));
    let actual = hex(&Sha256::digest(&bytes));
    assert_eq!(
        actual, ARCHIVE_SHA256,
        "vendored tunnel archive {} does not match its pinned SHA-256",
        archive.display()
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("open tunnel archive");
    for (member, output) in MEMBERS {
        extract(&mut zip, member, &out_dir.join(output));
    }
}

fn extract(
    zip: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    member: &str,
    target: &Path,
) {
    // Members may sit at the archive root or under one directory.
    let index = (0..zip.len())
        .find(|&i| {
            zip.by_index(i)
                .ok()
                .and_then(|f| f.enclosed_name())
                .is_some_and(|p| p.file_name().is_some_and(|n| n == member))
        })
        .unwrap_or_else(|| panic!("tunnel archive has no {member}"));
    let mut file = zip.by_index(index).expect("tunnel archive member");
    let mut data = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut data).expect("read tunnel archive member");
    fs::write(target, data).unwrap_or_else(|e| panic!("write {}: {e}", target.display()));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
