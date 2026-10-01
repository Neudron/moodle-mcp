//! Task 3 Step 3: sync engine end-to-end against the mock fixture.
//!
//! Full sync (tree + frontmatter index + meta.json + receipt), resume from a
//! truncated `.part`, external `.url`+`.md` pointers tracked in state, filters.

#[path = "integration/mock.rs"]
mod mock;

use std::collections::BTreeMap;

use mock::{MockMoodle, COURSE_ID, TOKEN};
use moodle_mcp::moodle::{Course, ModuleFile, Moodle};
use moodle_mcp::state::{CourseState, FileEntry, State};
use moodle_mcp::sync::{sync_course, RunContext, RunOptions, SyncFilters};

fn client(m: &MockMoodle) -> Moodle {
    Moodle::new(m.base(), TOKEN)
}

fn test_root(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("moodle-mcp-{name}-{}-{nanos}", std::process::id()));
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

    let course_dir = std::path::Path::new(&root).join("mock");
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
    let mod_meta = read_json(&course_dir.join("RA01/001-module-1-0/meta.json"));
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
async fn e2e_filters() {
    let (_m, c, _courses, _root) = setup().await;
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
}

#[tokio::test]
async fn e2e_error_keeps_course_state() {
    let (_m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let before = state.courses.get("101").expect("cs").files.clone();
    assert_eq!(before.len(), 50);

    // A regular file where a section dir must go makes ensure_layout fail.
    let sec = std::path::Path::new(&root).join("mock/RA05");
    std::fs::remove_dir_all(&sec).expect("rm section");
    std::fs::write(&sec, b"x").expect("blocker");

    assert!(sync_course(&c, &mut state, course, &root).await.is_err());
    let after = &state.courses.get("101").expect("entry kept").files;
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn e2e_failed_download_keeps_tracked_url() {
    let (m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let (url, entry) = {
        let f = &state.courses.get("101").expect("cs").files;
        let (u, e) = f.iter().next().expect("entry");
        (u.clone(), e.clone())
    };
    let dest = std::path::Path::new(&root).join(&entry.path);
    std::fs::remove_file(&dest).expect("rm file");

    m.set_files_down(true);
    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("sync");
    assert!(!r.errors.is_empty(), "download must fail");
    let cs = state.courses.get("101").expect("cs");
    assert!(cs.files.contains_key(&url), "url dropped from state");
    assert_eq!(cs.files.len(), 50);

    m.set_files_down(false);
    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("retry");
    assert_eq!(r.files_new, 1, "errors: {:?}", r.errors);
    assert!(dest.is_file(), "restored at original path, no -2 suffix");
}

#[tokio::test]
async fn e2e_inserted_module_keeps_folders() {
    let (m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let dirs = |p: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(std::path::Path::new(&root).join(p))
            .expect("dir")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let before = dirs("mock/RA01");
    let paths: Vec<String> = state.courses["101"]
        .files
        .values()
        .map(|f| f.path.clone())
        .collect();

    m.set_insert_first(true);
    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("resync");
    assert_eq!(r.files_new, 0, "errors: {:?}", r.errors);
    assert!(r.errors.is_empty());
    let after = dirs("mock/RA01");
    assert_eq!(after.len(), before.len() + 1, "{after:?}");
    for d in &before {
        assert!(after.contains(d), "folder {d} lost: {after:?}");
    }
    let now: Vec<String> = state.courses["101"]
        .files
        .values()
        .map(|f| f.path.clone())
        .collect();
    assert_eq!(paths, now, "no moves, no -2 files");
}

fn s1_paths(state: &State) -> Vec<String> {
    state.courses["101"]
        .files
        .values()
        .map(|f| f.path.clone())
        .filter(|p| p.contains("/RA01/"))
        .collect()
}

#[tokio::test]
async fn e2e_upstream_deleted_file_goes_to_trash() {
    let (m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let gone = s1_paths(&state);
    assert!(!gone.is_empty());
    m.set_drop_s1(true);
    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("resync");
    assert_eq!(r.files_removed, gone.len(), "errors: {:?}", r.errors);
    let tracked = s1_paths(&state);
    assert!(tracked.is_empty(), "{tracked:?}");
    let trash = std::fs::read_dir(std::path::Path::new(&root).join(".moodle/trash"))
        .expect("trash")
        .next()
        .expect("sync dir")
        .expect("e")
        .path();
    assert!(trash
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("sync-"));
    for p in &gone {
        assert!(!std::path::Path::new(&root).join(p).exists(), "{p}");
        assert!(trash.join(p).is_file(), "{p}");
    }
}

#[tokio::test]
async fn e2e_edited_stale_file_is_kept() {
    let (m, c, courses, root) = setup().await;
    let course = course_of(&courses);
    let mut state = State::default();
    sync_course(&c, &mut state, course, &root)
        .await
        .expect("seed");
    let gone = s1_paths(&state);
    let edited = std::path::Path::new(&root).join(&gone[0]);
    std::fs::write(&edited, b"my notes").unwrap();
    m.set_drop_s1(true);
    let r = sync_course(&c, &mut state, course, &root)
        .await
        .expect("resync");
    assert_eq!(r.files_removed, gone.len() - 1);
    assert_eq!(std::fs::read(&edited).unwrap(), b"my notes");
}
