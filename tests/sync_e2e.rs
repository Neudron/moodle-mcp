//! Task 3 Step 3: sync engine end-to-end against the mock fixture.
//!
//! Full sync (tree + frontmatter index + meta.json + receipt), resume from a
//! truncated `.part`, verify/repair, GC preview→trash→apply with audit,
//! external `.url`+`.md` pointers tracked in state, filters + dry-run.

#[path = "integration/mock.rs"]
mod mock;

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use mock::{MockMoodle, COURSE_ID, TOKEN};
use moodle_mcp::moodle::{Course, ModuleFile, Moodle};
use moodle_mcp::state::{CourseState, FileEntry, State};
use moodle_mcp::sync::{
    plan_course, preview, repair, sync_course, sync_course_with_options, trash, verify, RunContext,
    RunOptions, SyncFilters,
};

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
        "moodle-mcp-{name}-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test root");
    dir.display().to_string()
}

async fn setup() -> (MockMoodle, Moodle, Vec<Course>, String) {
    let m = MockMoodle::healthy().await;
    let c = client(&m);
    let courses = c.courses(42).await.expect("courses");
    assert!(courses.iter().any(|x| x.id == COURSE_ID));
    let root = test_root("sync-e2e");
    (m, c, courses, root)
}

fn course_of(courses: &[Course]) -> &Course {
    courses
        .iter()
        .find(|x| x.id == COURSE_ID)
        .expect("course 101")
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).expect("read json");
    serde_json::from_str(&raw).expect("parse json")
}

#[tokio::test]
async fn e2e_full_sync_writes_tree_and_index() {
    let (_m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("sync");
    assert_eq!(r.files_new, 50, "errors: {:?}", r.errors);
    assert!(r.errors.is_empty());
    assert_eq!(r.files_skipped, 0);

    let course_dir = std::path::Path::new(&root).join("MOCK101");
    assert!(course_dir.is_dir());
    for sec in ["00-intro", "RA01", "RA02", "RA03", "RA04", "RA05"] {
        assert!(course_dir.join(sec).is_dir(), "missing section {sec}");
    }
    // Frontmatter index.
    let index = std::fs::read_to_string(course_dir.join("index.md")).expect("index.md");
    assert!(index.starts_with("---\n"), "frontmatter missing");
    assert!(index.contains("course_id: 101"));
    assert!(index.contains("files: 50"));
    // Per-module meta (2 files) and section rollup (10 files).
    let mod_meta = read_json(&course_dir.join("RA01/001-Module 1.0/meta.json"));
    assert_eq!(mod_meta["files"].as_array().map(Vec::len), Some(2));
    assert_eq!(mod_meta["id"], 2002); // fixture: 2000 + file counter after 2 files
    let sec_meta = read_json(&course_dir.join("RA01/meta.json"));
    assert_eq!(sec_meta["files"], 10);
    assert_eq!(sec_meta["folder"], "RA01");
    // Receipt.
    let receipt = read_json(&course_dir.join("last-sync.json"));
    assert_eq!(receipt["files_new"], 50);
    assert_eq!(receipt["course_id"], 101);
    assert!(receipt["synced_at"]
        .as_str()
        .map(|s| s.ends_with('Z'))
        .unwrap_or(false));
    // State tracks all 50.
    let cs = state.courses.get("101").expect("course state");
    assert_eq!(cs.files.len(), 50);
}

#[tokio::test]
async fn e2e_resume_truncated_part_and_missing_file() {
    let (m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    let first = sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    assert_eq!(first.files_new, 50);

    // Pick one tracked file: delete the final, seed a truncated `.part`.
    let cs = state.courses.get("101").expect("state").files.clone();
    let (url, entry) = cs.iter().next().expect("an entry");
    let dest = std::path::Path::new(&root).join(&entry.path);
    let full = {
        let plugin_path = url
            .split("/webservice/pluginfile.php")
            .nth(1)
            .expect("plugin path");
        m.pluginfile(plugin_path).expect("fixture bytes").to_vec()
    };
    assert_eq!(full.len() as i64, entry.size);
    std::fs::remove_file(&dest).expect("delete final");
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    std::fs::write(&part, &full[..full.len() / 2]).expect("seed part");

    // Delete a second file outright (no .part): plain re-download path.
    let (url2, entry2) = cs.iter().nth(1).expect("second entry");
    assert_ne!(url, url2);
    std::fs::remove_file(std::path::Path::new(&root).join(&entry2.path)).expect("delete 2nd");

    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("resume");
    assert_eq!(r.files_new, 2, "errors: {:?}", r.errors);
    assert_eq!(r.files_skipped, 48);
    assert_eq!(
        std::fs::read(&dest).expect("restored"),
        full,
        "resume must complete bytes"
    );
}

#[tokio::test]
async fn e2e_verify_and_repair() {
    let (_m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let root_path = std::path::Path::new(&root).to_path_buf();
    let files = state.courses.get("101").expect("state").files.clone();

    assert!(
        verify(&root_path, &files).await.is_empty(),
        "fresh sync must verify clean"
    );

    let entry = files.values().next().expect("entry");
    let dest = root_path.join(&entry.path);
    let raw = std::fs::read(&dest).expect("read");
    std::fs::write(&dest, &raw[..raw.len() / 2]).expect("corrupt");
    let mismatches = verify(&root_path, &files).await;
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].reason, "size_mismatch");

    let rep = repair(&c, &root_path, &mismatches, "test").await;
    assert_eq!(rep.fixed, 1, "errors: {:?}", rep.errors);
    assert_eq!(rep.failed, 0);
    assert!(
        verify(&root_path, &files).await.is_empty(),
        "repaired tree must verify clean"
    );
}

#[tokio::test]
async fn e2e_gc_preview_trash_apply_with_audit() {
    use moodle_mcp::sync::apply_trash;
    let (_m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let root_path = std::path::Path::new(&root).to_path_buf();

    // Inject a stale entry: real file on disk + state row, unknown URL.
    let stale_rel = "MOCK101/RA01/001-Module 1.0/stale-note.txt";
    std::fs::write(root_path.join(stale_rel), b"stale").expect("stale file");
    let cs = state.courses.get_mut("101").expect("state");
    cs.files.insert(
        "http://127.0.0.1:9/webservice/pluginfile.php/9/stale.txt".to_string(),
        FileEntry {
            path: stale_rel.to_string(),
            sha256: "deadbeef".to_string(),
            size: 5,
            ..Default::default()
        },
    );
    let files = cs.files.clone();
    let active: HashSet<String> = files
        .keys()
        .filter(|u| !u.contains("/9/stale.txt"))
        .cloned()
        .collect();
    let candidates = preview(&files, &active);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].path, stale_rel);

    let trash_dir = trash(&candidates, &root_path, "test").await.expect("trash");
    assert!(!root_path.join(stale_rel).exists(), "trashed away");
    assert!(trash_dir.join(stale_rel).is_file(), "preserved under trash");
    let audit = std::fs::read_to_string(root_path.join(".moodle/audit.jsonl")).expect("audit");
    assert!(audit.contains("\"action\":\"trash\""), "{audit}");

    apply_trash(&trash_dir, &root_path, "test")
        .await
        .expect("apply");
    assert!(!trash_dir.exists());
}

#[tokio::test]
async fn e2e_external_pointers_tracked_in_state() {
    let (m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let root_path = std::path::Path::new(&root).to_path_buf();

    // Real contents plus one injected external link (Google Docs).
    let mut contents = c.contents(COURSE_ID).await.expect("contents");
    contents[1].modules[0]
        .contents
        .as_mut()
        .expect("module contents")
        .push(ModuleFile {
            filename: "reading list".to_string(),
            filesize: 0,
            mimetype: "text/html".to_string(),
            fileurl: Some("https://docs.google.com/document/d/abc123".to_string()),
            timemodified: 0,
            contenthash: String::new(),
        });

    let empty_cs = CourseState::default();
    let filters = SyncFilters::default();
    let plan = moodle_mcp::sync::planner::plan(&contents, &filters, &empty_cs, "MOCK101");
    let ext = plan
        .fetch
        .iter()
        .find(|f| f.is_external)
        .expect("external planned");
    assert!(ext.dest_rel.ends_with(".url"));

    let ctx = RunContext {
        course_id: course.id,
        shortname: course.shortname.clone(),
        course_dir: root_path.join("MOCK101"),
        root: root_path.clone(),
    };
    let old: BTreeMap<String, FileEntry> = BTreeMap::new();
    let (report, new_files) =
        moodle_mcp::sync::executor::run(&c, &plan, &ctx, &RunOptions::default(), &old).await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let url = "https://docs.google.com/document/d/abc123".to_string();
    let entry = new_files.get(&url).expect("external tracked in state");
    assert!(entry.path.ends_with(".url"));
    assert!(root_path.join(&entry.path).is_file());
    let md_rel = entry
        .path
        .strip_suffix(".url")
        .expect("url suffix")
        .to_string()
        + ".md";
    let md = std::fs::read_to_string(root_path.join(&md_rel)).expect("companion md");
    assert!(md.contains("Google Drive/Docs"), "{md}");
    let _ = m;
}

#[tokio::test]
async fn e2e_filters_and_dry_run() {
    let (_m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let contents = c.contents(COURSE_ID).await.expect("contents");
    let empty = State::default();
    let empty_cs = CourseState::default();
    let cs = empty.courses.get("101").unwrap_or(&empty_cs);

    let only = SyncFilters {
        only: vec!["RA01".to_string()],
        ..Default::default()
    };
    assert_eq!(
        moodle_mcp::sync::planner::plan(&contents, &only, cs, "MOCK101")
            .fetch
            .len(),
        10
    );

    let except = SyncFilters {
        only: vec!["RA01".to_string()],
        except: vec!["*.mp4".to_string()],
        ..Default::default()
    };
    assert_eq!(
        moodle_mcp::sync::planner::plan(&contents, &except, cs, "MOCK101")
            .fetch
            .len(),
        9
    );

    let mime = SyncFilters {
        only: vec!["RA01".to_string()],
        mime: Some("application/pdf".to_string()),
        ..Default::default()
    };
    assert_eq!(
        moodle_mcp::sync::planner::plan(&contents, &mime, cs, "MOCK101")
            .fetch
            .len(),
        1
    );

    let plan = plan_course(&c, &empty, course, &only)
        .await
        .expect("plan_course");
    assert_eq!(plan.fetch.len(), 10);
    assert!(plan.to_json().is_ok());

    // Dry run downloads nothing and leaves the tree empty.
    let mut state = State::default();
    let dry = SyncFilters {
        dry_run: true,
        ..Default::default()
    };
    let r = sync_course_with_options(
        &c,
        &mut state,
        course,
        &root,
        &dry,
        &RunOptions::default(),
        "test",
    )
    .await
    .expect("dry run");
    assert_eq!(r.files_new, 0);
    assert!(r.errors.is_empty());
    let course_dir = std::path::Path::new(&root).join("MOCK101");
    let entries = std::fs::read_dir(&course_dir).expect("course dir").count();
    assert_eq!(entries, 0, "dry run must not write files");
}
