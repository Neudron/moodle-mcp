//! Task 3 Step 1 (failing test first): unchanged files are skipped
//! without any file download.
//!
//! Seeds state + files with one full sync, resets the mock hit counter,
//! then syncs again: all 50 fixture files must be skipped and the only HTTP
//! hit must be the single `core_course_get_contents` call.

#[path = "integration/mock.rs"]
mod mock;

use mock::{MockMoodle, COURSE_ID, TOKEN};
use moodle_mcp::moodle::Moodle;
use moodle_mcp::state::State;
use std::sync::atomic::{AtomicU64, Ordering};

fn client(m: &MockMoodle) -> Moodle {
    Moodle::new(m.base(), TOKEN)
}

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
        "moodle-mcp-{}-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
        n
    ));
    std::fs::create_dir_all(&dir).expect("test root");
    dir.display().to_string()
}

#[tokio::test]
async fn skips_unchanged_by_size_mtime() {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let courses = c.courses(42).await.expect("courses");
    let course = courses
        .iter()
        .find(|x| x.id == COURSE_ID)
        .expect("mock course");
    let root = test_root("skips-unchanged");
    let mut state = State::default();

    let first = moodle_mcp::sync::sync_course(&c, &mut state, course, &root)
        .await
        .expect("seeding sync");
    assert_eq!(first.files_new, 50, "seed must download all 50");

    m.reset_hits();
    let r = moodle_mcp::sync::sync_course(&c, &mut state, course, &root)
        .await
        .expect("second sync");
    assert_eq!(r.files_skipped, 50);
    assert_eq!(m.hits(), 1, "contents only, no file downloads");
}
