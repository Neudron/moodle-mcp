//! Delivered sync (Moodle `submission_files` -> `submit/_lliurat/`) and the ~/smx organizer
//! with an undo log (`.moodle/moves.jsonl`, deletes go to `.moodle/trash/<run>/`).
use crate::errors::redact_token;
use crate::web::{
    api::{self, ApiErr, R},
    db::{Local, Task},
    files, readme, App,
};
use anyhow::{bail, Result};
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// One writer at a time (a second caller waits): both jobs move files under `submit/`.
static BUSY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const LOG: &str = ".moodle/moves.jsonl";
const LLIURAT: &str = "submit/_lliurat/";

fn ae(e: ApiErr) -> anyhow::Error {
    anyhow::anyhow!(e.1)
}

fn sha_bytes(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn sha_file(p: &Path) -> Result<String> {
    Ok(sha_bytes(&std::fs::read(p)?))
}

fn base(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// `*_Amin.*` -> `*_amin.*`.
fn amin(name: &str) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{e}")),
        None => (name, String::new()),
    };
    match stem.len().checked_sub(5).and_then(|i| stem.get(i..)) {
        Some(t) if t.eq_ignore_ascii_case("_amin") => {
            format!("{}_amin{ext}", &stem[..stem.len() - 5])
        }
        _ => name.to_string(),
    }
}

fn toks(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect()
}

/// (file is one of the task's Moodle submission names, number of shared title words).
fn affinity(t: &Task, name: &str) -> (bool, usize) {
    let n = amin(name).to_lowercase();
    let exact = t
        .submission_files
        .iter()
        .any(|a| amin(&a.filename).to_lowercase() == n);
    let stem = name.rsplit_once('.').map_or(name, |p| p.0);
    let stem = stem.strip_suffix("_amin").unwrap_or(stem);
    (exact, toks(&t.title).intersection(&toks(stem)).count())
}

/// Removes each dir and its parents while they are empty (never the root itself).
fn prune_up(root: &Path, mut dirs: Vec<PathBuf>) {
    dirs.sort();
    dirs.dedup();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let mut d = d.as_path();
        while d != root && std::fs::remove_dir(d).is_ok() {
            d = d.parent().unwrap_or(root);
        }
    }
}

fn safe_rel(rel: &str) -> bool {
    let p = Path::new(rel);
    !p.is_absolute() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

// ---------------------------------------------------------------- delivered sync

#[derive(Debug, Default, Serialize)]
pub struct Delivered {
    pub tasks: usize,
    pub downloaded: usize,
    pub moodle_copies: usize,
    pub errors: Vec<String>,
}

/// Server-supplied names never leave the task folder and are never dotfiles (the jail hides those).
fn safe_name(n: &str) -> String {
    let n = n.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if n.is_empty() || n.starts_with('.') {
        format!("_{}", n.trim_start_matches('.'))
    } else {
        amin(n)
    }
}

fn moodle_copy_name(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => format!("{s}.moodle.{e}"),
        _ => format!("{name}.moodle"),
    }
}

/// For every task Moodle reports as submitted: status `lliurat`, folder under `submit/_lliurat/`,
/// the delivered files downloaded next to the student's own copies.
pub async fn delivered_sync(app: &Arc<App>) -> Result<Delivered> {
    let _g = BUSY.lock().await;
    let mut out = Delivered::default();
    for t in app
        .db
        .all_tasks()?
        .into_iter()
        .filter(|t| t.moodle_status == "submitted" && !t.submission_files.is_empty())
    {
        match deliver(app, &t, &mut out).await {
            Ok(true) => out.tasks += 1,
            Ok(false) => {}
            Err(e) => out
                .errors
                .push(format!("{}: {}", t.title, redact_token(&e.to_string()))),
        }
    }
    if out.tasks > 0 {
        let a = Arc::clone(app);
        tokio::task::spawn_blocking(move || readme::write(&a.db, &a.root)).await??;
    }
    Ok(out)
}

/// Any file under the task's own delivered folder `dir` with these exact bytes (sha compared only when the size matches).
fn same_in_submit(root: &str, dir: &str, bytes: &[u8]) -> bool {
    let root = Path::new(root);
    let mut v = Vec::new();
    files::walk(root, dir, &mut v);
    let sha = sha_bytes(bytes);
    v.iter().any(|f| {
        let p = root.join(f);
        std::fs::metadata(&p).is_ok_and(|m| m.len() == bytes.len() as u64)
            && sha_file(&p).is_ok_and(|s| s == sha)
    })
}

async fn deliver(app: &Arc<App>, t: &Task, out: &mut Delivered) -> Result<bool> {
    let mut changed = false;
    if app.db.local(&t.id)?.status != "lliurat" {
        let (a, id) = (Arc::clone(app), t.id.clone());
        // moves an existing task folder into _lliurat
        tokio::task::spawn_blocking(move || api::mark_lliurat(&a, &id).map_err(ae)).await??;
        changed = true;
    }
    let mut l = app.db.local(&t.id)?;
    let rel = readme::submit_dir(&api::module_of(&l, &t.course), &t.title).replacen(
        "submit/",
        "submit/_lliurat/",
        1,
    );
    let dir = Path::new(&app.root).join(&rel);
    let before = l.files.clone();
    let mut delivered = Vec::new();
    for a in &t.submission_files {
        let name = safe_name(&a.filename);
        let dst = dir.join(&name);
        let have = tokio::fs::read(&dst).await.ok();
        // ponytail: same size == same file; a same-size edit is only caught when Moodle omits filesize.
        if a.filesize > 0 && have.as_ref().is_some_and(|h| h.len() as i64 == a.filesize) {
            delivered.push(name);
            continue;
        }
        let bytes = app.moodle.download(&a.fileurl).await?;
        if have.is_none() && same_in_submit(&app.root, &rel, &bytes) {
            continue; // already delivered under another name
        }
        tokio::fs::create_dir_all(&dir).await?;
        match have {
            None => {
                tokio::fs::write(&dst, &bytes).await?;
                out.downloaded += 1;
                changed = true;
            }
            Some(h) if h == bytes => {}
            Some(_) => {
                let m = dir.join(moodle_copy_name(&name));
                if tokio::fs::read(&m).await.ok().as_deref() != Some(&bytes[..]) {
                    tokio::fs::write(&m, &bytes).await?;
                    out.moodle_copies += 1;
                    changed = true;
                }
            }
        }
        delivered.push(name);
    }
    for name in &delivered {
        let r = format!("{rel}/{name}");
        if !l.files.contains(&r) {
            l.files.push(r);
        }
    }
    if l.files != before {
        app.db.set_local(&l)?;
        changed = true;
    }
    // Loose copies of the same file elsewhere (e.g. a course folder) now point at the delivered one.
    let moves: Vec<(String, String)> = delivered
        .iter()
        .flat_map(|name| {
            let r = format!("{rel}/{name}");
            before
                .iter()
                .filter(|f| base(f) == name && **f != r)
                .map(|f| (f.clone(), r.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let a = Arc::clone(app);
    tokio::task::spawn_blocking(move || -> Result<()> {
        for (f, r) in moves {
            api::repoint(&a, &f, &r).map_err(ae)?;
        }
        Ok(())
    })
    .await??;
    Ok(changed)
}

// ---------------------------------------------------------------- organize

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Op {
    pub run: i64,
    pub at: i64,
    /// `move` (file renamed/moved/trashed), `replace` (old copy kept in trash) or `module` (db label).
    pub kind: String,
    pub from: String,
    pub to: String,
    pub sha: String,
    pub task: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub run: i64,
    pub ops: usize,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UndoReport {
    pub run: i64,
    pub restored: usize,
    pub skipped: Vec<String>,
}

fn read_log(root: &Path) -> Result<Vec<Op>> {
    let Ok(s) = std::fs::read_to_string(root.join(LOG)) else {
        return Ok(Vec::new());
    };
    Ok(s.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

fn write_log(root: &Path, ops: &[Op]) -> Result<()> {
    let body: String = ops
        .iter()
        .map(|o| serde_json::to_string(o).map(|l| l + "\n"))
        .collect::<Result<_, _>>()?;
    let tmp = root.join(".moodle/moves.jsonl.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, root.join(LOG))?;
    Ok(())
}
/// Most common non-empty module label per course (ties: alphabetical) is the canonical folder name.
fn canon_labels(app: &App) -> Result<HashMap<String, String>> {
    let mut n: HashMap<(String, String), usize> = HashMap::new();
    for l in app.db.all_local()? {
        if l.module.is_empty() {
            continue;
        }
        if let Some(t) = app.db.task(&l.task_id)? {
            *n.entry((t.course, l.module)).or_default() += 1;
        }
    }
    let mut best: HashMap<String, (usize, String)> = HashMap::new();
    for ((c, m), k) in n {
        let e = best.entry(c).or_insert((0, String::new()));
        if k > e.0 || (k == e.0 && m < e.1) {
            *e = (k, m);
        }
    }
    Ok(best.into_iter().map(|(c, (_, m))| (c, m)).collect())
}

struct Cx {
    app: Arc<App>,
    root: PathBuf,
    run: i64,
    /// Files synced from Moodle (state.json): never moved, renamed or trashed.
    teacher: HashSet<String>,
    canon: HashMap<String, String>,
    ops: Vec<Op>,
    skipped: Vec<String>,
}

impl Cx {
    fn new(app: &Arc<App>) -> Result<Self> {
        let root = Path::new(&app.root).canonicalize()?;
        std::fs::create_dir_all(root.join(".moodle"))?;
        let teacher = crate::state::State::load(&app.root)?
            .courses
            .values()
            .flat_map(|c| c.files.values().map(|f| f.path.clone()))
            .collect();
        let last = read_log(&root)?.iter().map(|o| o.run).max().unwrap_or(0);
        Ok(Self {
            app: Arc::clone(app),
            run: crate::web::now().max(last + 1),
            canon: canon_labels(app)?,
            root,
            teacher,
            ops: Vec::new(),
            skipped: Vec::new(),
        })
    }

    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn protected(&self, rel: &str) -> bool {
        let first = rel.split('/').next().unwrap_or("");
        first.starts_with('.') || first == "moodle-mcp" || self.teacher.contains(rel)
    }

    fn log(&mut self, kind: &str, from: &str, to: &str, sha: String, task: &str) -> Result<()> {
        let op = Op {
            run: self.run,
            at: crate::web::now(),
            kind: kind.into(),
            from: from.into(),
            to: to.into(),
            sha,
            task: task.into(),
        };
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.abs(LOG))?;
        writeln!(f, "{}", serde_json::to_string(&op)?)?;
        self.ops.push(op);
        Ok(())
    }

    fn raw_mv(&mut self, from: &str, to: &str) -> Result<()> {
        let (a, b) = (self.abs(from), self.abs(to));
        let sha = sha_file(&a)?;
        if let Some(d) = b.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::rename(&a, &b)?;
        self.log("move", from, to, sha, "")
    }

    fn trash(&mut self, from: &str) -> Result<()> {
        if self.protected(from) {
            return Ok(());
        }
        let to = format!(".moodle/trash/{}/{from}", self.run);
        self.raw_mv(from, &to)
    }

    /// Move `from` to `to` and repoint task links. Same bytes already at `to`: the copy is trashed.
    /// Different bytes at `to`: nothing moves, the pair is reported.
    fn place(&mut self, from: &str, to: &str) -> Result<()> {
        if from == to || self.protected(from) || self.protected(to) || !self.abs(from).is_file() {
            return Ok(()); // stale link (file deleted or moved by hand)
        }
        let (lf, lt) = (from.starts_with(LLIURAT), to.starts_with(LLIURAT));
        if self.abs(to).exists() {
            if sha_file(&self.abs(from))? == sha_file(&self.abs(to))? {
                // same bytes: the delivered copy is the one that stays
                let (keep, drop) = if lf && !lt { (from, to) } else { (to, from) };
                api::repoint(&self.app, drop, keep).map_err(ae)?;
                return self.trash(drop);
            }
            self.skipped.push(format!("{from}: {to} ja existeix"));
            return Ok(());
        }
        if lf && !lt {
            return Ok(()); // a delivered file never leaves _lliurat
        }
        self.raw_mv(from, to)?;
        api::repoint(&self.app, from, to).map_err(ae)
    }

    /// Everything outside moodle-mcp/ and dotdirs (files::list already hides those).
    fn all_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in files::list(&self.root, "").unwrap_or_default() {
            if e.name == "moodle-mcp" {
                continue;
            }
            if e.is_dir {
                files::walk(&self.root, &e.rel, &mut out);
            } else {
                out.push(e.rel);
            }
        }
        out
    }

    fn task_dir(&self, t: &Task, l: &Local, lliurat: bool) -> String {
        let module = self
            .canon
            .get(&t.course)
            .cloned()
            .unwrap_or_else(|| api::module_of(l, &t.course));
        let d = readme::submit_dir(&module, &t.title);
        if lliurat || l.status == "lliurat" {
            d.replacen("submit/", "submit/_lliurat/", 1)
        } else {
            d
        }
    }

    /// The task a loose file belongs to: best by (same name/sha among its Moodle or linked files,
    /// shared title words). No evidence or a tie: none.
    fn find_task(&self, rel: &str) -> Result<Option<(Task, Local)>> {
        let name = base(rel);
        let sha = sha_file(&self.abs(rel)).ok();
        let mut best: Vec<((bool, usize), Task, Local)> = Vec::new();
        for t in self
            .app
            .db
            .all_tasks()?
            .into_iter()
            .filter(|t| t.kind != "post")
        {
            let l = self.app.db.local(&t.id)?;
            let (exact, ov) = affinity(&t, name);
            let linked = l.files.iter().any(|f| {
                base(f).eq_ignore_ascii_case(name)
                    || sha.is_some() && sha_file(&self.abs(f)).ok() == sha
            });
            let key = (exact || linked, ov);
            if key != (false, 0) {
                best.push((key, t, l));
            }
        }
        best.sort_by_key(|a| std::cmp::Reverse(a.0));
        Ok(match best.as_slice() {
            [] => None,
            [x, y, ..] if x.0 == y.0 => None,
            [x, ..] => Some((x.1.clone(), x.2.clone())),
        })
    }

    /// Rule 3: `*_Amin.*` -> `*_amin.*`.
    fn case_fix(&mut self) -> Result<()> {
        for f in self.all_files() {
            let (dir, name) = f.rsplit_once('/').unwrap_or(("", f.as_str()));
            let fixed = amin(name);
            if fixed != name {
                let to = if dir.is_empty() {
                    fixed
                } else {
                    format!("{dir}/{fixed}")
                };
                self.place(&f, &to)?;
            }
        }
        Ok(())
    }

    /// Rule 4: test files.
    fn prova(&mut self) -> Result<()> {
        let mut v = Vec::new();
        files::walk(&self.root, "submit/_prova", &mut v);
        for f in v {
            self.trash(&f)?;
        }
        Ok(())
    }

    /// Rules 1 and 2: loose `*_amin.*` in course folders and everything in `*/lliuraments/`.
    fn loose(&mut self) -> Result<()> {
        let mut delivered = Vec::new();
        files::walk(&self.root, "submit/_lliurat", &mut delivered);
        let by_sha: HashMap<String, String> = delivered
            .into_iter()
            .filter_map(|f| Some((sha_file(&self.abs(&f)).ok()?, f)))
            .collect();
        let courses: Vec<String> = files::list(&self.root, "")
            .unwrap_or_default()
            .into_iter()
            .filter(|e| {
                e.is_dir
                    && !matches!(e.name.as_str(), "submit" | "_personal" | "moodle-mcp")
                    && self.abs(&format!("{}/index.md", e.rel)).is_file()
            })
            .map(|e| e.rel)
            .collect();
        for c in courses {
            let mut v = Vec::new();
            files::walk(&self.root, &c, &mut v);
            for f in v {
                let name = base(&f).to_string();
                let in_lliuraments = f.split('/').any(|p| p == "lliuraments");
                let stem = name.rsplit_once('.').map_or(name.as_str(), |p| p.0);
                if !(in_lliuraments || stem.ends_with("_amin")) || self.protected(&f) {
                    continue;
                }
                if let Some(keep) = by_sha.get(&sha_file(&self.abs(&f))?) {
                    api::repoint(&self.app, &f, keep).map_err(ae)?;
                    self.trash(&f)?;
                    continue;
                }
                let to = match self.find_task(&f)? {
                    Some((t, l)) => format!("{}/{name}", self.task_dir(&t, &l, in_lliuraments)),
                    None => format!("submit/_solts/{name}"),
                };
                self.place(&f, &to)?;
            }
        }
        Ok(())
    }

    /// Rule 5.
    fn personal(&mut self) -> Result<()> {
        for n in ["decks", "marques", "sistemas", "school-verify.skill"] {
            if self.abs(n).is_dir() {
                let mut v = Vec::new();
                files::walk(&self.root, n, &mut v);
                for f in v {
                    self.place(&f, &format!("_personal/{f}"))?;
                }
            } else if self.abs(n).is_file() {
                self.place(n, &format!("_personal/{n}"))?;
            }
        }
        Ok(())
    }

    /// Rule 6: one module folder name per course, task files follow, `Local.module` follows.
    fn modules(&mut self) -> Result<()> {
        for l in self.app.db.all_local()? {
            let Some(t) = self.app.db.task(&l.task_id)? else {
                continue;
            };
            let Some(label) = self.canon.get(&t.course).cloned() else {
                continue;
            };
            if !l.files.iter().any(|f| f.starts_with("submit/")) {
                continue;
            }
            let dir = self.task_dir(&t, &l, false);
            for f in &l.files {
                if f.starts_with("submit/") && f.rsplit_once('/').map(|p| p.0) != Some(dir.as_str())
                {
                    if let Err(e) = self.place(f, &format!("{dir}/{}", base(f))) {
                        self.skipped.push(format!("{f}: {e}"));
                    }
                }
            }
            // a label is only ever given to a task that has none when a linked file belongs to it
            let owns = |l: &Local| l.files.iter().any(|f| affinity(&t, base(f)) != (false, 0));
            if l.module != label && (!l.module.is_empty() || owns(&l)) {
                let mut l2 = self.app.db.local(&l.task_id)?;
                let from = std::mem::replace(&mut l2.module, label.clone());
                self.app.db.set_local(&l2)?;
                self.log("module", &from, &label, String::new(), &l.task_id)?;
            }
        }
        Ok(())
    }

    /// Rule 7: `.moodle/courses.json` rebuilt from `core_enrol_get_users_courses` (what list_courses returns).
    async fn courses_json(&mut self) -> Result<()> {
        let app = Arc::clone(&self.app);
        let uid = app
            .moodle
            .call_with_retry("core_webservice_get_site_info", &[])
            .await?["userid"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("no userid"))?;
        let v = app
            .moodle
            .call_with_retry(
                "core_enrol_get_users_courses",
                &[("userid", uid.to_string())],
            )
            .await?;
        if v.as_array().is_none_or(|a| a.is_empty()) {
            bail!("llista de cursos buida, courses.json intacte");
        }
        let p = self.abs(".moodle/courses.json");
        let old = std::fs::read(&p).ok();
        if old
            .as_deref()
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .as_ref()
            == Some(&v)
        {
            return Ok(());
        }
        if let Some(old) = old {
            let keep = format!(".moodle/trash/{}/courses.json", self.run);
            let kp = self.abs(&keep);
            if let Some(d) = kp.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&kp, &old)?;
            self.log(
                "replace",
                ".moodle/courses.json",
                &keep,
                sha_bytes(&old),
                "",
            )?;
        }
        let tmp = self.abs(".moodle/courses.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&v)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
}

/// Rules 3, 4, 1+2, 5, 6, then 7. A failing rule is reported and the others still run.
pub async fn run(app: &Arc<App>) -> Result<Report> {
    let _g = BUSY.lock().await;
    let a = Arc::clone(app);
    let (mut cx, mut errors) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut cx = Cx::new(&a)?;
        let errors: Vec<String> = [
            cx.case_fix(),
            cx.prova(),
            cx.loose(),
            cx.personal(),
            cx.modules(),
        ]
        .into_iter()
        .filter_map(Result::err)
        .map(|e| e.to_string())
        .collect();
        let dirs = cx
            .ops
            .iter()
            .filter(|o| o.kind == "move")
            .filter_map(|o| cx.abs(&o.from).parent().map(Path::to_path_buf))
            .collect();
        prune_up(&cx.root, dirs);
        Ok((cx, errors))
    })
    .await??;
    if let Err(e) = cx.courses_json().await {
        errors.push(format!("courses.json: {}", redact_token(&e.to_string())));
    }
    if !cx.ops.is_empty() {
        let a = Arc::clone(app);
        tokio::task::spawn_blocking(move || readme::write(&a.db, &a.root)).await??;
    }
    Ok(Report {
        run: cx.run,
        ops: cx.ops.len(),
        skipped: cx.skipped,
        errors,
    })
}

/// Ops of the newest run: `{run, at, ops}` (`run` 0 when nothing was ever organized).
pub fn last_log(root: &str) -> Result<Value> {
    let ops = read_log(Path::new(root))?;
    let run = ops.iter().map(|o| o.run).max().unwrap_or(0);
    let ops: Vec<Op> = ops.into_iter().filter(|o| o.run == run).collect();
    Ok(json!({"run": run, "at": ops.first().map_or(0, |o| o.at), "ops": ops}))
}

fn undo_op(app: &App, root: &Path, op: &Op) -> Result<()> {
    match op.kind.as_str() {
        "module" => {
            let mut l = app.db.local(&op.task)?;
            if l.module == op.to {
                l.module = op.from.clone();
                app.db.set_local(&l)?;
            }
            Ok(())
        }
        "move" | "replace" => {
            if !safe_rel(&op.from) || !safe_rel(&op.to) {
                bail!("ruta no vàlida");
            }
            let (from, to) = (root.join(&op.from), root.join(&op.to));
            if !to.is_file() {
                bail!("ja no hi és");
            }
            if sha_file(&to)? != op.sha {
                bail!("modificat després d'organitzar");
            }
            if op.kind == "move" && from.exists() {
                bail!("l'origen ja existeix");
            }
            if let Some(d) = from.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::rename(&to, &from)?;
            api::repoint(app, &op.to, &op.from).map_err(ae)
        }
        k => bail!("operació desconeguda: {k}"),
    }
}

/// Reverts the newest run in reverse order. Ops that cannot be reverted stay in the log.
pub async fn undo(app: &Arc<App>) -> Result<UndoReport> {
    let _g = BUSY.lock().await;
    let app = Arc::clone(app);
    tokio::task::spawn_blocking(move || undo_sync(&app)).await?
}

fn undo_sync(app: &App) -> Result<UndoReport> {
    let root = Path::new(&app.root).canonicalize()?;
    let all = read_log(&root)?;
    let Some(run) = all.iter().map(|o| o.run).max() else {
        bail!("res a desfer");
    };
    let mut keep: Vec<Op> = all.iter().filter(|o| o.run != run).cloned().collect();
    let (mut restored, mut skipped, mut stuck, mut dirs) = (0, Vec::new(), Vec::new(), Vec::new());
    for op in all.iter().filter(|o| o.run == run).rev() {
        match undo_op(app, &root, op) {
            Ok(()) => {
                restored += 1;
                dirs.extend(root.join(&op.to).parent().map(Path::to_path_buf));
            }
            Err(e) => {
                skipped.push(format!("{} <- {}: {e}", op.from, op.to));
                stuck.push(op.clone());
            }
        }
    }
    stuck.reverse();
    keep.extend(stuck);
    write_log(&root, &keep)?;
    prune_up(&root, dirs);
    readme::write(&app.db, &app.root)?;
    Ok(UndoReport {
        run,
        restored,
        skipped,
    })
}

type S = State<Arc<App>>;

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/api/delivered-sync", post(delivered_h))
        .route("/api/organize", get(last_h).post(run_h))
        .route("/api/organize/undo", post(undo_h))
}

async fn last_h(State(app): S) -> R<Json<Value>> {
    Ok(Json(last_log(&app.root)?))
}

async fn undo_h(State(app): S) -> R<Json<UndoReport>> {
    Ok(Json(undo(&app).await?))
}

async fn delivered_h(State(app): S) -> R<Json<Delivered>> {
    Ok(Json(delivered_sync(&app).await?))
}

async fn run_h(State(app): S) -> R<Json<Report>> {
    Ok(Json(run(&app).await?))
}

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    #[test]
    fn same_in_submit_is_scoped_to_task_dir() {
        let d = std::env::temp_dir().join(format!("smx-dedupe-{}", std::process::id()));
        for p in ["submit/draft", "submit/_lliurat/t"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("submit/draft/a.docx"), b"x").unwrap();
        let r = d.to_str().unwrap();
        assert!(!same_in_submit(r, "submit/_lliurat/t", b"x"));
        std::fs::write(d.join("submit/_lliurat/t/b.docx"), b"x").unwrap();
        assert!(same_in_submit(r, "submit/_lliurat/t", b"x"));
        std::fs::remove_dir_all(d).ok();
    }
}
