use anyhow::{bail, Context, Result};
use moodle_mcp::{moodle::Moodle, state::State, sync::sync_course};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok(); // .env optional; real env always wins
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut all = false;
    let mut course_id: Option<i64> = None;
    for a in &args {
        match a.as_str() {
            "--all" => all = true,
            "--course" => course_id = None,
            s if s.parse::<i64>().is_ok() => course_id = Some(s.parse().unwrap()),
            _ => {}
        }
    }
    if !all && course_id.is_none() {
        eprintln!("usage: moodle-sync --all | --course <id>");
        std::process::exit(2);
    }
    let root = Moodle::moodle_root();
    let m = Moodle::from_env()?;
    let info = m.site_info().await.context("site info")?;
    let uid = info["userid"].as_i64().context("no userid")?;
    let mut state = State::load(&root)?;
    state.user_id = uid;

    let courses = m.courses(uid).await?;
    let picked: Vec<_> = match (all, course_id) {
        (true, _) => courses,
        (false, Some(id)) => {
            let c: Vec<_> = courses.into_iter().filter(|c| c.id == id).collect();
            if c.is_empty() {
                bail!("course {id} not found");
            }
            c
        }
        _ => unreachable!(),
    };
    for course in &picked {
        eprintln!("syncing {} ({})...", course.fullname, course.id);
        match sync_course(&m, &mut state, course, &root).await {
            Ok(r) => {
                eprintln!(
                    "  ok: {} new, {} skipped, {} errors",
                    r.files_new,
                    r.files_skipped,
                    r.errors.len()
                );
                for e in &r.errors {
                    eprintln!("    err: {e}");
                }
            }
            Err(e) => eprintln!("  FAILED: {e:#}"),
        }
        State::save(&state, &root).ok();
    }
    Ok(())
}
