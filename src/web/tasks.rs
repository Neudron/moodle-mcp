//! Moodle -> Task normalisation, sync orchestration and change events.
use crate::errors::CoreError;
use crate::moodle::{Course, Moodle};
use crate::web::db::{Attachment, Db, Task};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    NewTask(String),
    DueChanged(String),
    NewGrade(String),
    NewPost(String),
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}
fn i(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn attachments(v: &Value) -> Vec<Attachment> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|a| Attachment {
            filename: s(&a["filename"]),
            fileurl: s(&a["fileurl"]),
            mimetype: s(&a["mimetype"]),
            filesize: i(&a["filesize"]),
            timemodified: i(&a["timemodified"]),
        })
        .collect()
}

/// `lastattempt.submission.plugins[type=file].fileareas[].files[]`
fn submission_files(st: &Value) -> Vec<Attachment> {
    st["lastattempt"]["submission"]["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == "file")
        .flat_map(|p| p["fileareas"].as_array().into_iter().flatten())
        .flat_map(|fa| attachments(&fa["files"]))
        .collect()
}

pub fn from_assign(c: &Course, a: &Value, st: &Value, base: &str) -> Task {
    let feedback = st["feedback"]["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["type"] == "comments")
        .map(|p| s(&p["editorfields"][0]["text"]))
        .unwrap_or_default();
    Task {
        id: format!("assign:{}", a["id"]),
        course_id: c.id,
        course: c.shortname.clone(),
        kind: "assign".into(),
        title: s(&a["name"]),
        body_html: s(&a["intro"]),
        attachments: attachments(&a["introattachments"]),
        submission_files: submission_files(st),
        url: format!("{base}/mod/assign/view.php?id={}", a["cmid"]),
        opens: i(&a["allowsubmissionsfromdate"]),
        due: i(&a["duedate"]),
        cutoff: i(&a["cutoffdate"]),
        moodle_status: st["lastattempt"]["submission"]["status"]
            .as_str()
            .unwrap_or("new")
            .into(),
        grade: s(&st["feedback"]["gradefordisplay"]),
        feedback_html: feedback,
        source: "moodle".into(),
        ai_confirmed: None,
    }
}

pub fn from_quiz(c: &Course, q: &Value, attempts: &Value, base: &str) -> Task {
    let list = attempts["attempts"].as_array().cloned().unwrap_or_default();
    let status = if list.iter().any(|a| a["state"] == "finished") {
        "submitted"
    } else if list.is_empty() {
        "new"
    } else {
        "draft"
    };
    Task {
        id: format!("quiz:{}", q["id"]),
        course_id: c.id,
        course: c.shortname.clone(),
        kind: "quiz".into(),
        title: s(&q["name"]),
        body_html: s(&q["intro"]),
        url: format!("{base}/mod/quiz/view.php?id={}", q["coursemodule"]),
        opens: i(&q["timeopen"]),
        due: i(&q["timeclose"]),
        moodle_status: status.into(),
        source: "moodle".into(),
        ..Default::default()
    }
}

pub fn from_discussion(c: &Course, d: &Value, base: &str) -> Task {
    Task {
        id: format!("post:{}", d["discussion"]),
        course_id: c.id,
        course: c.shortname.clone(),
        kind: "post".into(),
        title: s(&d["name"]),
        body_html: s(&d["message"]),
        url: format!("{base}/mod/forum/discuss.php?d={}", d["discussion"]),
        opens: i(&d["created"]),
        moodle_status: "new".into(),
        source: "moodle".into(),
        ..Default::default()
    }
}

pub fn diff(old: &HashMap<String, Task>, new: &[Task]) -> Vec<Event> {
    let mut ev = Vec::new();
    for t in new {
        match old.get(&t.id) {
            None if t.kind == "post" => ev.push(Event::NewPost(t.id.clone())),
            None => ev.push(Event::NewTask(t.id.clone())),
            Some(o) => {
                if o.due != t.due {
                    ev.push(Event::DueChanged(t.id.clone()));
                }
                if o.grade != t.grade && !t.grade.is_empty() {
                    ev.push(Event::NewGrade(t.id.clone()));
                }
            }
        }
    }
    ev
}

/// RFC 3986 unreserved kept, everything else %XX (UTF-8 bytes).
pub fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn proxy_urls(html: &str) -> String {
    // ponytail: regex over HTML attributes; fine for Moodle-generated markup.
    let re = regex::Regex::new(r#"(["'])(https?://[^"']*pluginfile\.php[^"']*)(["'])"#)
        .expect("static regex");
    re.replace_all(html, |c: &regex::Captures| {
        let url = c[2].replace("&amp;", "&");
        format!("{}/api/moodle-file?url={}{}", &c[1], pct(&url), &c[3])
    })
    .into_owned()
}

/// Tolerates per-course failures (logged, Null) but aborts on AUTH.
async fn ws(m: &Moodle, f: &str, p: &[(&str, String)]) -> Result<Value, CoreError> {
    match m.call_with_retry(f, p).await {
        Ok(v) => Ok(v),
        Err(CoreError::Auth(e)) => Err(CoreError::Auth(e)),
        Err(e) => {
            tracing::warn!(function = f, error = %e, "ws call failed");
            Ok(Value::Null)
        }
    }
}

pub async fn sync_all(
    m: &Moodle,
    db: &Db,
    root: &str,
    files: bool,
) -> Result<Vec<Event>, CoreError> {
    let info = m
        .call_with_retry("core_webservice_get_site_info", &[])
        .await?;
    let uid = info["userid"]
        .as_i64()
        .ok_or_else(|| CoreError::Invalid("no userid".into()))?;
    let courses: Vec<Course> = serde_json::from_value(
        m.call_with_retry(
            "core_enrol_get_users_courses",
            &[("userid", uid.to_string())],
        )
        .await?,
    )
    .map_err(|e| CoreError::Invalid(e.to_string()))?;
    let base = m.base().to_string();
    let mut out = Vec::new();
    for c in &courses {
        let cid = c.id.to_string();
        let a = ws(
            m,
            "mod_assign_get_assignments",
            &[("courseids[0]", cid.clone())],
        )
        .await?;
        for course in a["courses"].as_array().into_iter().flatten() {
            for asg in course["assignments"].as_array().into_iter().flatten() {
                let st = ws(
                    m,
                    "mod_assign_get_submission_status",
                    &[("assignid", asg["id"].to_string())],
                )
                .await?;
                let mut t = from_assign(c, asg, &st, &base);
                if st.is_null() {
                    // transient failure: keep cached status/grade/feedback/delivered files
                    if let Some(o) = db.task(&t.id).map_err(CoreError::Other)? {
                        (
                            t.moodle_status,
                            t.grade,
                            t.feedback_html,
                            t.submission_files,
                        ) = (
                            o.moodle_status,
                            o.grade,
                            o.feedback_html,
                            o.submission_files,
                        );
                    }
                }
                out.push(t);
            }
        }
        let q = ws(
            m,
            "mod_quiz_get_quizzes_by_courses",
            &[("courseids[0]", cid.clone())],
        )
        .await?;
        for quiz in q["quizzes"].as_array().into_iter().flatten() {
            let at = ws(
                m,
                "mod_quiz_get_user_attempts",
                &[("quizid", quiz["id"].to_string()), ("status", "all".into())],
            )
            .await?;
            let mut t = from_quiz(c, quiz, &at, &base);
            if at.is_null() {
                if let Some(o) = db.task(&t.id).map_err(CoreError::Other)? {
                    t.moodle_status = o.moodle_status;
                }
            }
            out.push(t);
        }
        let f = ws(
            m,
            "mod_forum_get_forums_by_courses",
            &[("courseids[0]", cid.clone())],
        )
        .await?;
        for forum in f.as_array().into_iter().flatten() {
            let d = ws(
                m,
                "mod_forum_get_forum_discussions",
                &[("forumid", forum["id"].to_string())],
            )
            .await?;
            for disc in d["discussions"].as_array().into_iter().flatten() {
                out.push(from_discussion(c, disc, &base));
            }
        }
        let g = ws(
            m,
            "gradereport_user_get_grade_items",
            &[("courseid", cid.clone()), ("userid", uid.to_string())],
        )
        .await?;
        if !g["usergrades"][0].is_null() {
            // Keep the name here: courses with no tasks have nowhere else to get it from.
            let mut ug = g["usergrades"][0].clone();
            ug["course_name"] = Value::String(c.shortname.clone());
            db.set_grades(c.id, &ug).map_err(CoreError::Other)?;
        }
    }
    let old: HashMap<String, Task> = db
        .all_tasks()
        .map_err(CoreError::Other)?
        .into_iter()
        .filter(|t| t.source == "moodle")
        .map(|t| (t.id.clone(), t))
        .collect();
    let events = if old.is_empty() {
        Vec::new()
    } else {
        diff(&old, &out)
    };
    db.upsert_tasks(&out).map_err(CoreError::Other)?;
    if files {
        let mut state = crate::state::State::load(root).map_err(CoreError::Other)?;
        state.user_id = uid;
        for c in &courses {
            if let Err(e) = crate::sync::sync_course(m, &mut state, c, root).await {
                tracing::warn!(course = c.id, error = %e, "file sync failed");
            }
            crate::state::State::save(&state, root).ok();
        }
    }
    db.kv_set("last_sync", &chrono::Utc::now().timestamp().to_string())
        .map_err(CoreError::Other)?;
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c() -> Course {
        Course {
            id: 1,
            fullname: "Curs".into(),
            shortname: "2627_DAX_0373".into(),
            summary: String::new(),
        }
    }

    #[test]
    fn assign_normalised() {
        let a = json!({"id": 7, "cmid": 70, "name": "A", "intro": "<p>x</p>",
            "introattachments": [{"filename": "e.pdf", "fileurl": "u", "mimetype": "application/pdf"}],
            "allowsubmissionsfromdate": 1, "duedate": 2, "cutoffdate": 3});
        let st = json!({"lastattempt": {"submission": {"status": "submitted"}},
            "feedback": {"gradefordisplay": "9", "plugins": [{"type": "comments", "editorfields": [{"text": "ok"}]}]}});
        let t = from_assign(&c(), &a, &st, "https://m");
        assert_eq!(t.id, "assign:7");
        assert_eq!(t.url, "https://m/mod/assign/view.php?id=70");
        assert_eq!((t.opens, t.due, t.cutoff), (1, 2, 3));
        assert_eq!(t.moodle_status, "submitted");
        assert_eq!(t.grade, "9");
        assert_eq!(t.feedback_html, "ok");
        assert_eq!(t.attachments[0].filename, "e.pdf");
        assert_eq!(t.course, "2627_DAX_0373");
    }

    #[test]
    fn assign_zero_due_and_missing_status() {
        let t = from_assign(
            &c(),
            &json!({"id": 8, "cmid": 80, "name": "B"}),
            &serde_json::Value::Null,
            "https://m",
        );
        assert_eq!(t.due, 0);
        assert_eq!(t.moodle_status, "new");
        assert!(t.grade.is_empty());
    }

    #[test]
    fn quiz_status_from_attempts() {
        let q = json!({"id": 5, "coursemodule": 50, "name": "Q", "timeclose": 9});
        assert_eq!(
            from_quiz(&c(), &q, &json!({"attempts": [{"state": "finished"}]}), "b").moodle_status,
            "submitted"
        );
        assert_eq!(
            from_quiz(
                &c(),
                &q,
                &json!({"attempts": [{"state": "inprogress"}]}),
                "b"
            )
            .moodle_status,
            "draft"
        );
        assert_eq!(
            from_quiz(&c(), &q, &json!({"attempts": []}), "b").moodle_status,
            "new"
        );
        assert_eq!(from_quiz(&c(), &q, &json!({}), "b").due, 9);
    }

    #[test]
    fn discussion_is_post() {
        let t = from_discussion(
            &c(),
            &json!({"discussion": 30, "name": "N", "message": "m", "created": 5}),
            "b",
        );
        assert_eq!(
            (t.id.as_str(), t.kind.as_str(), t.opens),
            ("post:30", "post", 5)
        );
    }

    #[test]
    fn diff_events() {
        let mut old = HashMap::new();
        let base = Task {
            id: "assign:1".into(),
            kind: "assign".into(),
            due: 10,
            ..Default::default()
        };
        old.insert(base.id.clone(), base.clone());
        let moved = Task {
            due: 20,
            ..base.clone()
        };
        let graded = Task {
            grade: "9".into(),
            ..base.clone()
        };
        let fresh = Task {
            id: "assign:2".into(),
            kind: "assign".into(),
            ..Default::default()
        };
        let post = Task {
            id: "post:1".into(),
            kind: "post".into(),
            ..Default::default()
        };
        assert_eq!(
            diff(&old, &[moved]),
            vec![Event::DueChanged("assign:1".into())]
        );
        assert_eq!(
            diff(&old, &[graded]),
            vec![Event::NewGrade("assign:1".into())]
        );
        assert_eq!(
            diff(&old, &[fresh, post]),
            vec![
                Event::NewTask("assign:2".into()),
                Event::NewPost("post:1".into())
            ]
        );
        assert!(diff(&old, &[base]).is_empty());
    }

    #[test]
    fn proxies_pluginfile_urls() {
        let h = r#"<img src="https://m/webservice/pluginfile.php/1/a b.png?x=1"> <a href="https://other/x">"#;
        let out = proxy_urls(h);
        assert!(out.contains(r#"src="/api/moodle-file?url=https%3A%2F%2Fm%2Fwebservice%2Fpluginfile.php%2F1%2Fa%20b.png%3Fx%3D1""#), "{out}");
        assert!(out.contains("https://other/x"));
    }

    #[test]
    fn submission_files_and_sizes_parsed() {
        let a = json!({"id": 7, "cmid": 70, "name": "A",
            "introattachments": [{"filename": "e.pdf", "fileurl": "u", "mimetype": "application/pdf", "filesize": 55}]});
        let st = json!({"lastattempt": {"submission": {"status": "submitted", "plugins": [
            {"type": "comments"},
            {"type": "file", "fileareas": [{"area": "submission_files", "files": [
                {"filename": "a_amin.docx", "fileurl": "https://m/pluginfile.php/1/a_amin.docx",
                 "mimetype": "application/x", "filesize": 1234, "timemodified": 99}]}]}]}}});
        let t = from_assign(&c(), &a, &st, "https://m");
        assert_eq!(t.attachments[0].filesize, 55);
        assert_eq!(t.submission_files.len(), 1);
        let f = &t.submission_files[0];
        assert_eq!(
            (f.filename.as_str(), f.filesize, f.timemodified),
            ("a_amin.docx", 1234, 99)
        );
    }

    #[test]
    fn no_submission_plugins_means_no_files() {
        let st = json!({"lastattempt": {"submission": {"status": "draft"}}});
        assert!(from_assign(&c(), &json!({"id": 9, "cmid": 90}), &st, "b")
            .submission_files
            .is_empty());
        assert!(
            from_assign(&c(), &json!({"id": 9, "cmid": 90}), &Value::Null, "b")
                .submission_files
                .is_empty()
        );
    }
}
