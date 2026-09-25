use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use graph::{
    runtime::functions::{GraphFn, flush_udfs, register_udf, unregister_udf},
    udf::get_udf_repo,
};

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("udf-libraries.json")
}

fn backup_path(data_dir: &Path) -> PathBuf {
    data_dir.join("udf-libraries.json.bak")
}

pub fn restore(data_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("create UDF data directory {}: {e}", data_dir.display()))?;

    let primary = store_path(data_dir);
    let backup = backup_path(data_dir);
    let source = if primary.exists() {
        Some(primary)
    } else if backup.exists() {
        Some(backup)
    } else {
        None
    };

    let Some(path) = source else {
        // A fresh data directory should reset any process-global UDF state left
        // by an earlier in-process test/server instance.
        get_udf_repo().flush();
        flush_udfs();
        return Ok(());
    };

    let bytes = fs::read(&path)
        .map_err(|e| format!("read UDF store {}: {e}", path.display()))?;
    let libraries: Vec<(String, String)> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("decode UDF store {}: {e}", path.display()))?;

    let restored = get_udf_repo().deserialize(&libraries)?;
    for library in restored {
        for qname in library.function_names {
            register_udf(&qname, Arc::new(GraphFn::new_udf(&qname)));
        }
    }

    // If recovery used the backup after an interrupted replace, normalize it.
    if path == backup {
        persist(data_dir)?;
    }
    Ok(())
}

pub fn persist(data_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("create UDF data directory {}: {e}", data_dir.display()))?;

    let target = store_path(data_dir);
    let backup = backup_path(data_dir);
    let temp = data_dir.join(format!(
        "udf-libraries.json.tmp-{}",
        std::process::id()
    ));

    let encoded = serde_json::to_vec(&get_udf_repo().serialize())
        .map_err(|e| format!("encode UDF store: {e}"))?;

    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .map_err(|e| format!("create UDF temp store {}: {e}", temp.display()))?;
        file.write_all(&encoded)
            .map_err(|e| format!("write UDF temp store {}: {e}", temp.display()))?;
        file.sync_all()
            .map_err(|e| format!("sync UDF temp store {}: {e}", temp.display()))?;
    }

    let _ = fs::remove_file(&backup);
    if target.exists() {
        fs::rename(&target, &backup).map_err(|e| {
            format!(
                "rotate UDF store {} -> {}: {e}",
                target.display(),
                backup.display()
            )
        })?;
    }

    if let Err(err) = fs::rename(&temp, &target) {
        if backup.exists() && !target.exists() {
            let _ = fs::rename(&backup, &target);
        }
        return Err(format!(
            "publish UDF store {} -> {}: {err}",
            temp.display(),
            target.display()
        ));
    }

    let _ = fs::remove_file(&backup);
    Ok(())
}

pub fn load(
    data_dir: &Path,
    name: &str,
    script: &str,
    replace: bool,
) -> Result<Vec<String>, String> {
    graph::identifier_limits::validate_identifier_len(name, "Library name")?;

    let repo = get_udf_repo();
    let function_names = repo.load(name, script, replace)?;
    for qname in &function_names {
        register_udf(qname, Arc::new(GraphFn::new_udf(qname)));
    }

    if let Err(err) = persist(data_dir) {
        // Restore the durable state as the source of truth if persistence fails.
        let _ = restore(data_dir);
        return Err(err);
    }
    Ok(function_names)
}

pub fn delete(data_dir: &Path, name: &str) -> Result<(), String> {
    let repo = get_udf_repo();
    let removed = repo.delete(name)?;
    for qname in &removed {
        unregister_udf(qname);
    }

    if let Err(err) = persist(data_dir) {
        let _ = restore(data_dir);
        return Err(err);
    }
    Ok(())
}

pub fn flush(data_dir: &Path) -> Result<(), String> {
    get_udf_repo().flush();
    flush_udfs();
    if let Err(err) = persist(data_dir) {
        let _ = restore(data_dir);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udf_store_path_is_inside_data_directory() {
        let base = Path::new("native-data");
        assert_eq!(store_path(base), base.join("udf-libraries.json"));
    }
}
