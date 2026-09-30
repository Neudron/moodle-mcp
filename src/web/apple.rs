//! Mirror pending tasks to Apple Calendar + Reminders via the icloud-glass CLI.
use crate::web::{db::Task, now, App};
use serde::Serialize;
use std::time::Duration;
use tokio::process::Command;

const NAME: &str = "Institut";
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default, Serialize)]
pub struct Summary {
    pub created: u32,
    pub updated: u32,
    pub done: u32,
    pub deleted: u32,
    pub errors: Vec<String>,
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run the CLI (60s cap); returns the "uid" of its JSON stdout ("" when it prints none).
async fn cli(args: &[&str]) -> Result<String, String> {
    let bin = std::env::var("ICLOUD_GLASS_CMD").unwrap_or_else(|_| {
        format!(
            "{}/.local/bin/icloud-glass",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let mut cmd = Command::new(&bin);
    cmd.args(args).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| format!("{} timeout", args[0]))?
        .map_err(|e| format!("{}: {e}", args[0]))?;
    if !out.status.success() {
        return Err(format!("{} exit {}", args[0], out.status));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    Ok(clean(v["uid"].as_str().unwrap_or("")))
}

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn pending(t: &Task, lliurat: bool, at: i64) -> bool {
    t.kind != "post"
        && (t.source != "ai" || t.ai_confirmed == Some(true))
        && t.moodle_status != "submitted"
        && !lliurat
        && t.due > at
}

pub async fn sync(app: &App) -> Summary {
    let _g = LOCK.lock().await;
    let mut s = Summary::default();
    if let Err(e) = run(app, &mut s).await {
        s.errors.push(e.to_string());
    }
    s
}

async fn run(app: &App, s: &mut Summary) -> anyhow::Result<()> {
    let at = now();
    let tasks = app.db.all_tasks()?;
    let rows = app.db.caldav_rows()?;
    // Stale mirrors: delivered, submitted, past, discarded or gone.
    for (id, _, ev, todo) in &rows {
        let t = tasks.iter().find(|t| &t.id == id);
        if t.is_some_and(|t| pending(t, app.db.local(id).is_ok_and(|l| l.status == "lliurat"), at))
        {
            continue;
        }
        let mut ok = true;
        if !ev.is_empty() {
            match cli(&["event-delete", "--calendar", NAME, ev]).await {
                Ok(_) => s.deleted += 1,
                Err(e) => (ok, _) = (false, s.errors.push(e)),
            }
        }
        if !todo.is_empty() {
            match cli(&["todo-done", todo, "--json"]).await {
                Ok(_) => s.done += 1,
                Err(e) => (ok, _) = (false, s.errors.push(e)),
            }
        }
        if ok {
            app.db.del_caldav(id)?;
        }
    }
    for t in tasks.iter().filter(|t| {
        pending(
            t,
            app.db.local(&t.id).is_ok_and(|l| l.status == "lliurat"),
            at,
        )
    }) {
        let (_, old, mut ev, mut todo) = rows
            .iter()
            .find(|r| r.0 == t.id)
            .cloned()
            .unwrap_or_default();
        let h = format!("{}|{}|{}|{}", t.due, t.course, t.title, t.url);
        if old == h {
            continue;
        }
        let summary = clean(&format!("{}: {}", t.course, t.title));
        let url = clean(&t.url);
        let (start, due) = (iso(t.due - 3600), iso(t.due));
        let mut a = vec![
            "event-new",
            "--calendar",
            NAME,
            "--summary",
            &summary,
            "--start",
            &start,
            "--end",
            &due,
            "--url",
            &url,
            "--description",
            "Entrega a Moodle",
            "--alarm-minutes",
            "4320",
            "--alarm-minutes",
            "1440",
            "--alarm-minutes",
            "180",
        ];
        let had_ev = !ev.is_empty();
        if had_ev {
            a.extend(["--uid", &ev]);
        }
        let mut ok = true;
        match cli(&a).await {
            Ok(u) if !u.is_empty() => {
                ev = u;
                if had_ev {
                    s.updated += 1
                } else {
                    s.created += 1
                }
            }
            Ok(_) => (ok, _) = (false, s.errors.push("event-new: no uid".into())),
            Err(e) => (ok, _) = (false, s.errors.push(e)),
        }
        // Reminders have no update call: retire the old one, create a new one.
        if !todo.is_empty() {
            match cli(&["todo-done", &todo, "--json"]).await {
                Ok(_) => (todo, s.done) = (String::new(), s.done + 1),
                Err(e) => (ok, _) = (false, s.errors.push(e)),
            }
        }
        if todo.is_empty() {
            match cli(&[
                "todo-new",
                "--list",
                NAME,
                "--summary",
                &summary,
                "--due",
                &due,
                "--notes",
                &url,
                "--json",
            ])
            .await
            {
                Ok(u) => todo = u,
                Err(e) => (ok, _) = (false, s.errors.push(e)),
            }
        }
        // Empty hash forces a retry next run; uids are kept so retries update in place.
        app.db
            .set_caldav(&t.id, if ok { &h } else { "" }, &ev, &todo)?;
    }
    Ok(())
}
