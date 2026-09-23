//! Task 4 Step 1: v1 (epoch `synced_at`, 3-field `FileEntry`) migrates to
//! v2 (ISO8601 `synced_at`, backfilled `timemodified`/`mimetype`/`source`)
//! on load, keeping a timestamped backup.

use moodle_mcp::state::State;

const FIXTURE_V1: &str = include_str!("fixtures/state-v1.json");

fn test_root(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("moodle-mcp-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(dir.join(".moodle")).expect("test root");
    dir.display().to_string()
}

#[test]
fn state_migrate_v1_v2() {
    let root = test_root("state-migrate-v1-v2");
    let state_file = std::path::Path::new(&root).join(".moodle/state.json");
    std::fs::write(&state_file, FIXTURE_V1).expect("seed v1 state");

    let state = State::load(&root).expect("load migrates v1");

    // Schema bumped.
    assert_eq!(state.schema_version, 2);
    // Epoch seconds became ISO8601 (1700000000 == 2023-11-14T22:13:20Z).
    let cs = state.courses.get("101").expect("course 101");
    assert_eq!(cs.synced_at, "2023-11-14T22:13:20Z");
    assert_eq!(state.user_id, 42);
    // New FileEntry fields backfilled with serde defaults.
    let entry = cs
        .files
        .get("http://h.invalid/webservice/pluginfile.php/1/a.pdf")
        .expect("file entry");
    assert_eq!(entry.path, "MOCK101/RA01/001-m/a.pdf");
    assert_eq!(entry.sha256, "abc123");
    assert_eq!(entry.size, 100);
    assert_eq!(entry.timemodified, 0);
    assert_eq!(entry.mimetype, "");
    assert_eq!(entry.source, "");

    // Original bytes kept as a rotating backup.
    let backups: Vec<_> = std::fs::read_dir(std::path::Path::new(&root).join(".moodle"))
        .expect("list .moodle")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("state.json.bak."))
        .collect();
    assert_eq!(backups.len(), 1, "one migration backup: {backups:?}");
    let backup_raw = std::fs::read_to_string(
        std::path::Path::new(&root).join(format!(".moodle/{}", backups[0])),
    )
    .expect("read backup");
    assert_eq!(backup_raw, FIXTURE_V1);

    // On-disk state is now v2.
    let raw = std::fs::read_to_string(&state_file).expect("read migrated state");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("migrated json");
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["courses"]["101"]["synced_at"], "2023-11-14T22:13:20Z");

    // Second load is a no-op: no extra backup, same content.
    let again = State::load(&root).expect("reload");
    assert_eq!(again.schema_version, 2);
    assert_eq!(again.courses["101"].synced_at, "2023-11-14T22:13:20Z");
    let backups2: Vec<_> = std::fs::read_dir(std::path::Path::new(&root).join(".moodle"))
        .expect("list .moodle")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("state.json.bak."))
        .collect();
    assert_eq!(backups2.len(), 1, "no duplicate backup: {backups2:?}");
}
