//! Task 4 Step 3 (config): defaults, file values, env precedence,
//! clamping, init, redacted show, validate. Env-touching tests share one
//! process-global lock and clean up after themselves.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use moodle_mcp::config::Config;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clear_config_env() {
    for var in [
        "MOODLE_CONCURRENCY",
        "MOODLE_API_TIMEOUT_SECS",
        "MOODLE_FILE_TIMEOUT_SECS",
        "MOODLE_MAX_RETRIES",
        "MOODLE_BACKOFF_BASE_MS",
        "MOODLE_MAX_RPS",
        "MOODLE_INDEX",
        "MOODLE_PDF_TEXT",
        "MOODLE_OCR",
        "MOODLE_ENRICH_LINKS",
        "MOODLE_LOCALE",
        "MOODLE_THEME",
        "MOODLE_MOUSE",
        "MOODLE_STORE",
        "MOODLE_TOKEN",
    ] {
        std::env::remove_var(var);
    }
}

/// Fresh root. Callers must hold [`lock_env`] (env is process-global).
fn test_root() -> String {
    // Counter (not just clock+pid): parallel tests in one process can share
    // a timestamp tick on coarse clocks and would then share a dir.
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "moodle-mcp-config-{}-{nanos}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test root");
    dir.display().to_string()
}

fn write_config(root: &str, body: &str) {
    std::fs::create_dir_all(std::path::Path::new(root).join(".moodle")).expect("dir");
    std::fs::write(std::path::Path::new(root).join(".moodle/config.toml"), body)
        .expect("write config");
}

#[test]
fn config_defaults_when_no_file_no_env() {
    let _guard = lock_env();
    clear_config_env();
    let root = test_root();
    let config = Config::load(&root);
    assert_eq!(config, Config::default());
    assert_eq!(config.concurrency, 8);
    assert!(config.validate().is_empty());
}

#[test]
fn config_file_values_apply() {
    let _guard = lock_env();
    clear_config_env();
    let root = test_root();
    write_config(
        &root,
        "concurrency = 4\nlocale = \"es\"\nocr = true\nmax_rps = 2.5\n",
    );
    let config = Config::load(&root);
    assert_eq!(config.concurrency, 4);
    assert_eq!(config.locale, "es");
    assert!(config.flags.ocr);
    assert_eq!(config.rate_max_rps, 2.5);
    assert!(config.validate().is_empty());
}

#[test]
fn config_env_overrides_file() {
    let _guard = lock_env();
    clear_config_env();
    let root = test_root();
    write_config(&root, "concurrency = 4\nlocale = \"es\"\n");
    std::env::set_var("MOODLE_CONCURRENCY", "12");
    std::env::set_var("MOODLE_LOCALE", "en");
    let config = Config::load(&root);
    assert_eq!(config.concurrency, 12);
    assert_eq!(config.locale, "en");
    clear_config_env();
}

#[test]
fn config_clamps_and_invalid_env_ignored() {
    let _guard = lock_env();
    clear_config_env();
    let root = test_root();
    std::env::set_var("MOODLE_CONCURRENCY", "99");
    std::env::set_var("MOODLE_LOCALE", "xx");
    std::env::set_var("MOODLE_STORE", "tape");
    let config = Config::load(&root);
    assert_eq!(config.concurrency, 32);
    assert_eq!(config.locale, "xx");
    let issues = config.validate().join("\n");
    assert!(issues.contains("locale"), "{issues}");
    assert!(issues.contains("store"), "{issues}");
    // Garbage numerics fall back to defaults, never panic.
    std::env::set_var("MOODLE_CONCURRENCY", "lots");
    std::env::set_var("MOODLE_MAX_RPS", "fast");
    let config = Config::load(&root);
    assert_eq!(config.concurrency, 8);
    assert_eq!(config.rate_max_rps, 8.0);
    clear_config_env();
}

#[test]
fn config_init_show_validate() {
    let _guard = lock_env();
    clear_config_env();
    let root = test_root();
    assert!(Config::init(&root).expect("init creates"));
    assert!(!Config::init(&root).expect("init idempotent"));
    let loaded = Config::load(&root);
    assert_eq!(loaded, Config::default());

    // Redacted show carries no secret, even with a token in the env.
    std::env::set_var("MOODLE_TOKEN", "FAKETOKEN-0123456789abcdef");
    let shown = Config::default().show_redacted();
    assert!(!shown.contains("FAKETOKEN-0123456789abcdef"), "{shown}");
    assert!(!shown.to_ascii_lowercase().contains("token"), "{shown}");
    clear_config_env();
}
