//! Task 4 Step 2: locked atomic state v2 behaviours — 0600 perms,
//! pretty+sorted keys, corrupt-state hint, jail, concurrent saves,
//! validate/diff/vacuum/repair, large-state roundtrip.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use moodle_mcp::state::{CourseState, FileEntry, State};

fn test_root(name: &str) -> String {
    // Counter (not just clock+pid): parallel tests in one process can share
    // a timestamp tick on coarse clocks and would then share a dir.
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "moodle-mcp-{name}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test root");
    dir.display().to_string()
}

fn sample_state() -> State {
    let mut state = State {
        user_id: 42,
        ..Default::default()
    };
    state.courses.insert(
        "101".to_string(),
        CourseState {
            fullname: "Mock Course".to_string(),
            shortname: "MOCK101".to_string(),
            synced_at: "2023-11-14T22:13:20Z".to_string(),
            sections: BTreeMap::from([("1".to_string(), "RA01".to_string())]),
            files: BTreeMap::from([(
                "http://h.invalid/webservice/pluginfile.php/1/a.pdf".to_string(),
                FileEntry::new(
                    "MOCK101/RA01/001-m/a.pdf".to_string(),
                    "abc123".to_string(),
                    100,
                    1_700_000_000,
                    "application/pdf".to_string(),
                    moodle_mcp::state::SOURCE_PLUGINFILE,
                ),
            )]),
        },
    );
    state
}

#[test]
fn state_save_locked_atomic_600_pretty_sorted() {
    let root = test_root("state-save");
    let state = sample_state();
    state.save_locked(&root).expect("save");

    let path = std::path::Path::new(&root).join(".moodle/state.json");
    let raw = std::fs::read_to_string(&path).expect("read state");

    // Pretty + sorted keys (stable diff, #986): "courses" sorts before
    // "meta" before "schema_version" before "user_id".
    let courses_pos = raw.find("\"courses\"").expect("courses");
    let meta_pos = raw.find("\"meta\"").expect("meta");
    let schema_pos = raw.find("\"schema_version\"").expect("schema");
    let user_pos = raw.find("\"user_id\"").expect("user");
    assert!(
        courses_pos < meta_pos && meta_pos < schema_pos && schema_pos < user_pos,
        "keys not sorted: {raw}"
    );
    assert!(raw.contains('\n'), "not pretty");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "state file must be 0600");
    }

    // Roundtrip preserves everything, records the owning root, validates.
    let loaded = State::load(&root).expect("load");
    assert_eq!(loaded.schema_version, 2);
    assert_eq!(loaded.user_id, 42);
    assert_eq!(loaded.courses["101"].files.len(), 1);
    assert!(loaded.validate().is_empty(), "{:?}", loaded.validate());
    assert!(!loaded.meta.root.is_empty(), "owning root recorded");
}

#[test]
fn state_corrupt_gives_restore_hint() {
    let root = test_root("state-corrupt");
    let dir = std::path::Path::new(&root).join(".moodle");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("state.json"), "{ not json").expect("seed corrupt");
    // A backup exists to point at.
    std::fs::write(dir.join("state.json.bak.1"), "{}").expect("seed backup");

    let err = State::load(&root).expect_err("corrupt must fail");
    let msg = err.to_string();
    assert!(msg.contains("corrupt"), "{msg}");
    assert!(msg.contains("state.json.bak.1"), "{msg}");
    assert!(msg.contains("cp "), "{msg}");
}

#[test]
fn state_multi_root_guard_refuses_foreign_state() {
    let root_a = test_root("state-root-a");
    sample_state().save_locked(&root_a).expect("save a");
    // Copy the state file (with its recorded root) under another root.
    let root_b = test_root("state-root-b");
    std::fs::create_dir_all(std::path::Path::new(&root_b).join(".moodle")).expect("dir b");
    std::fs::copy(
        std::path::Path::new(&root_a).join(".moodle/state.json"),
        std::path::Path::new(&root_b).join(".moodle/state.json"),
    )
    .expect("copy state");
    let err = State::load(&root_b).expect_err("foreign root must fail");
    assert!(err.to_string().contains("multi-root"), "{err}");
}

#[test]
fn state_concurrent_saves_stay_valid() {
    // fs2 exclusive lock serialises writers: no torn files (#988).
    let root = test_root("state-concurrent");
    let mut handles = Vec::new();
    for worker in 0..8 {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            let mut state = sample_state();
            state.user_id = worker;
            for _ in 0..5 {
                state.save_locked(&root).expect("concurrent save");
                let loaded = State::load(&root).expect("concurrent load");
                // No torn writes: structural errors never appear (a user_id
                // warning is fine — another worker may have saved user_id 0).
                let errors: Vec<_> = loaded
                    .validate()
                    .into_iter()
                    .filter(|i| i.starts_with("error:"))
                    .collect();
                assert!(errors.is_empty(), "{errors:?}");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker");
    }
    let final_state = State::load(&root).expect("final load");
    assert_eq!(final_state.courses["101"].files.len(), 1);
    assert!(
        final_state.validate().is_empty(),
        "{:?}",
        final_state.validate()
    );
}

#[test]
fn state_vacuum_and_repair() {
    let root = test_root("state-vacuum-repair");
    let root_path = std::path::Path::new(&root).to_path_buf();
    sample_state().save_locked(&root).expect("save");

    let vacuum = State::vacuum(&root).expect("vacuum");
    assert_eq!(vacuum.courses, 1);
    assert_eq!(vacuum.files, 1);
    assert!(vacuum.bytes_after > 0);

    // Repair with the tracked file missing drops the entry (with backup).
    let repair = State::repair(&root).expect("repair");
    assert_eq!(repair.checked, 1);
    assert_eq!(repair.dropped_missing, 1);
    assert_eq!(repair.kept, 0);
    let backups: Vec<_> = std::fs::read_dir(root_path.join(".moodle"))
        .expect("list")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("state.json.bak."))
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");

    // Now create the file on disk and re-add the entry: repair keeps it.
    let mut state = State::load(&root).expect("load");
    let cs = state.courses.get_mut("101").expect("course");
    cs.files.insert(
        "u".to_string(),
        FileEntry::new(
            "MOCK101/RA01/001-m/a.pdf".to_string(),
            "x".to_string(),
            3,
            0,
            String::new(),
            "",
        ),
    );
    state.save_locked(&root).expect("save");
    std::fs::create_dir_all(root_path.join("MOCK101/RA01/001-m")).expect("dirs");
    std::fs::write(root_path.join("MOCK101/RA01/001-m/a.pdf"), b"abc").expect("file");
    // A path-escape entry is dropped and counted separately.
    let mut state = State::load(&root).expect("load");
    state.courses.get_mut("101").expect("course").files.insert(
        "evil".to_string(),
        FileEntry::new(
            "../evil".to_string(),
            "x".to_string(),
            1,
            0,
            String::new(),
            "",
        ),
    );
    state.save_locked(&root).expect("save");
    let repair = State::repair(&root).expect("repair");
    assert_eq!(repair.dropped_escape, 1);
    assert_eq!(repair.kept, 1);
}

#[test]
fn state_large_state_roundtrip() {
    // >10k files must save+load+validate quickly (#987).
    let root = test_root("state-large");
    let mut state = State {
        user_id: 1,
        ..Default::default()
    };
    let mut files = BTreeMap::new();
    for i in 0..12_000 {
        files.insert(
            format!("http://h.invalid/webservice/pluginfile.php/1/f{i}.pdf"),
            FileEntry::new(
                format!("C/RA01/001-m/f{i}.pdf"),
                format!("{i:064}"),
                i as i64,
                1_700_000_000,
                "application/pdf".to_string(),
                moodle_mcp::state::SOURCE_PLUGINFILE,
            ),
        );
    }
    state.courses.insert(
        "1".to_string(),
        CourseState {
            fullname: "Big".to_string(),
            shortname: "C".to_string(),
            synced_at: "2023-11-14T22:13:20Z".to_string(),
            sections: BTreeMap::new(),
            files,
        },
    );
    let started = std::time::Instant::now();
    state.save_locked(&root).expect("save large");
    let loaded = State::load(&root).expect("load large");
    assert_eq!(loaded.courses["1"].files.len(), 12_000);
    assert!(loaded.validate().is_empty());
    assert!(
        started.elapsed().as_secs() < 30,
        "large-state roundtrip too slow: {:?}",
        started.elapsed()
    );
}
