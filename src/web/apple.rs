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
    #[serde(skip)]
    pub errs: Vec<AppleErr>,
}

impl Summary {
    pub fn auth(&self) -> bool {
        self.errs.iter().any(|e| e.kind == Kind::Auth)
    }
    fn fail(&mut self, e: AppleErr) {
        self.errors.push(e.msg.clone());
        self.errs.push(e);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Auth,
    Network,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppleErr {
    pub kind: Kind,
    pub msg: String,
}

impl AppleErr {
    fn new(kind: Kind, msg: &str) -> Self {
        let first = msg.lines().next().unwrap_or("");
        Self {
            kind,
            msg: clean(first).chars().take(200).collect(),
        }
    }
}

fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn state_dir() -> std::path::PathBuf {
    std::env::var_os("ICLOUD_GLASS_STATE")
        .map(Into::into)
        .unwrap_or_else(|| {
            std::path::Path::new(&std::env::var_os("HOME").unwrap_or_default())
                .join(".local/share/icloud-glass")
        })
}

/// icloud-glass drops this file when the Reminders session is dead.
fn marker() -> bool {
    state_dir().join("reminders-auth-required").exists()
}

/// Run the CLI (60s cap); returns the "uid" of its JSON stdout ("" when it prints none).
pub async fn cli(args: &[&str]) -> Result<String, AppleErr> {
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
        .map_err(|_| AppleErr::new(Kind::Network, &format!("{} timeout", args[0])))?
        .map_err(|e| AppleErr::new(Kind::Other, &format!("{}: {e}", args[0])))?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if !out.status.success() {
        let kind = match (v["kind"].as_str(), out.status.code()) {
            (Some("auth"), _) | (None, Some(2)) => Kind::Auth,
            (Some("network" | "server"), _) => Kind::Network,
            _ => Kind::Other,
        };
        let msg = v["error"]
            .as_str()
            .map(String::from)
            .or_else(|| {
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .map(String::from)
            })
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| format!("{} exit {}", args[0], out.status));
        return Err(AppleErr::new(kind, &msg));
    }
    Ok(clean(v["uid"].as_str().unwrap_or("")))
}

/// todo-* call that is skipped (no spawn) once Reminders auth is known dead.
async fn todo_cli(args: &[&str], blocked: &mut bool, s: &mut Summary) -> Result<String, ()> {
    if *blocked {
        return Err(());
    }
    cli(args).await.map_err(|e| {
        *blocked |= e.kind == Kind::Auth;
        s.fail(e);
    })
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
        s.fail(AppleErr::new(Kind::Other, &e.to_string()));
    }
    record(app, &s);
    s
}

/// Persist the pass outcome: error_log rows + kv state for /api/status.
fn record(app: &App, s: &Summary) {
    for e in &s.errs {
        let kind = match e.kind {
            Kind::Auth => "apple-auth",
            Kind::Network => "apple-net",
            Kind::Other => "apple",
        };
        app.db.log_error("apple", kind, &e.msg).ok();
    }
    let state = match s.errs.first().map(|e| e.kind) {
        None => "ok",
        Some(_) if s.auth() => "auth",
        Some(_) => "err",
    };
    let at = now().to_string();
    if app.db.kv_get("apple_state").ok().flatten().as_deref() != Some(state) {
        app.db.kv_set("apple_since", &at).ok();
    }
    app.db.kv_set("apple_state", state).ok();
    app.db
        .kv_set("apple_error", s.errors.first().map_or("", |m| m))
        .ok();
    app.db.kv_set("apple_at", &at).ok();
}

async fn run(app: &App, s: &mut Summary) -> anyhow::Result<()> {
    let at = now();
    let tasks = app.db.all_tasks()?;
    let rows = app.db.caldav_rows()?;
    let mut blocked = marker();
    if blocked {
        s.fail(AppleErr::new(
            Kind::Auth,
            "Recordatoris: sessió d'iCloud caducada",
        ));
    }
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
                Err(e) => (ok, _) = (false, s.fail(e)),
            }
        }
        if !todo.is_empty() {
            match todo_cli(&["todo-done", todo, "--json"], &mut blocked, s).await {
                Ok(_) => s.done += 1,
                Err(_) => ok = false,
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
        let changed = old != h;
        // Hash tracks the event only; a missing reminder uid is retried on its own.
        if !changed && !todo.is_empty() {
            continue;
        }
        let summary = clean(&format!("{}: {}", t.course, t.title));
        let url = clean(&t.url);
        let (start, due) = (iso(t.due - 3600), iso(t.due));
        let mut ok = true;
        if changed {
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
            match cli(&a).await {
                Ok(u) if !u.is_empty() => {
                    ev = u;
                    if had_ev {
                        s.updated += 1
                    } else {
                        s.created += 1
                    }
                }
                Ok(_) => {
                    ok = false;
                    s.fail(AppleErr::new(Kind::Other, "event-new: no uid"));
                }
                Err(e) => (ok, _) = (false, s.fail(e)),
            }
            // Reminders have no update call: retire the old one, create a new one.
            if !todo.is_empty() {
                match todo_cli(&["todo-done", &todo, "--json"], &mut blocked, s).await {
                    Ok(_) => (todo, s.done) = (String::new(), s.done + 1),
                    Err(_) => ok = false,
                }
            }
        }
        if todo.is_empty() {
            if let Ok(u) = todo_cli(
                &[
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
                ],
                &mut blocked,
                s,
            )
            .await
            {
                todo = u;
            }
        }
        // Empty hash forces an event retry next run; uids are kept so retries update in place.
        let keep = if changed { "" } else { &old };
        app.db
            .set_caldav(&t.id, if ok { &h } else { keep }, &ev, &todo)?;
    }
    Ok(())
}
