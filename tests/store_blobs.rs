//! Task 4 Step 3 (store): flat vs content-addressed layouts, hardlink
//! sharing with copy fallback, jail, orphan GC, layout recording.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use moodle_mcp::store::{Store, StoreLayout};

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

#[test]
fn store_flat_put_link_gc() {
    let root = test_root("store-flat");
    let store = Store::open_with_layout(&root, StoreLayout::Flat);
    let sha = store.put_blob(b"hello").expect("put");
    assert_eq!(sha.len(), 64);
    // Idempotent second put.
    assert_eq!(store.put_blob(b"hello").expect("put again"), sha);

    let dest = store.link_into(&sha, "C/RA01/001-m/a.pdf").expect("link");
    assert_eq!(std::fs::read(&dest).expect("read"), b"hello");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&dest).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "copies are 0644 (no exec bit)");
    }

    // Unknown blob and jail escapes fail.
    assert!(store.link_into(&"0".repeat(64), "C/x.pdf").is_err());
    assert!(store.link_into(&sha, "/abs/x.pdf").is_err());
    assert!(store.link_into(&sha, "../evil.pdf").is_err());
    assert!(store.link_into(&sha, "C/../../evil.pdf").is_err());

    // GC keeps live blobs, removes orphans.
    let live: HashSet<String> = [sha.clone()].into_iter().collect();
    let report = store.gc_orphans(&live).expect("gc");
    assert_eq!(report.removed, 0);
    let orphan = store.put_blob(b"orphan").expect("put orphan");
    assert_ne!(orphan, sha);
    let report = store.gc_orphans(&live).expect("gc");
    assert_eq!(report.removed, 1);
    assert!(report.bytes > 0);
    // Non-sha files in the blob dir are left alone.
    std::fs::write(
        std::path::Path::new(&root).join(".moodle/blobs/notes.txt"),
        b"keep",
    )
    .expect("marker");
    let report = store.gc_orphans(&HashSet::new()).expect("gc");
    assert_eq!(report.removed, 1, "only the live sha left to remove");
    assert!(std::path::Path::new(&root)
        .join(".moodle/blobs/notes.txt")
        .is_file());
}

#[test]
fn store_content_addressed_layout_and_sharing() {
    let root = test_root("store-ca");
    let store = Store::open_with_layout(&root, StoreLayout::ContentAddressed);
    let sha = store.put_blob(b"shared-bytes").expect("put");
    let blob = store.blob_path(&sha).expect("blob path");
    let rel = blob
        .strip_prefix(std::path::Path::new(&root))
        .expect("under root")
        .to_string_lossy()
        .replace('\\', "/");
    assert!(
        rel.starts_with(".moodle/blobs/") && rel.matches('/').count() == 4,
        "blobs/ab/cd/<sha> shape, got {rel}"
    );
    assert_eq!(&rel[14..16], &sha[..2]);
    assert_eq!(&rel[17..19], &sha[2..4]);
    assert!(rel.ends_with(&sha));

    let first = store.link_into(&sha, "C/RA01/a.pdf").expect("link 1");
    let second = store.link_into(&sha, "C/RA02/a.pdf").expect("link 2");
    assert_eq!(std::fs::read(&first).expect("r1"), b"shared-bytes");
    assert_eq!(std::fs::read(&second).expect("r2"), b"shared-bytes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let blob_ino = std::fs::metadata(&blob).expect("blob meta").ino();
        let first_ino = std::fs::metadata(&first).expect("first meta").ino();
        // Same filesystem: real hardlink. Cross-device CI would copy instead;
        // accept either, but bytes must match (asserted above).
        if blob_ino == first_ino {
            assert_eq!(
                std::fs::metadata(&second).expect("second meta").ino(),
                blob_ino
            );
        }
    }

    // Orphan GC walks nested dirs and prunes them.
    let report = store.gc_orphans(&HashSet::new()).expect("gc");
    assert_eq!(report.removed, 1);
    assert!(!blob.exists());
}

#[test]
fn store_open_reads_state_meta_and_records_layout() {
    let root = test_root("store-meta");
    // No state yet: flat default, no state file created by open.
    let store = Store::open(&root).expect("open");
    assert_eq!(store.layout(), StoreLayout::Flat);

    let ca = Store::open_with_layout(&root, StoreLayout::ContentAddressed);
    ca.record_layout().expect("record");
    let reopened = Store::open(&root).expect("reopen");
    assert_eq!(reopened.layout(), StoreLayout::ContentAddressed);

    let raw = std::fs::read_to_string(std::path::Path::new(&root).join(".moodle/state.json"))
        .expect("state written");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert_eq!(value["meta"]["store"], "content-addressed");
}
