use tracing_subscriber::EnvFilter;

/// Initialize the process-wide tracing subscriber (call once per binary).
///
/// * `json` — `true` selects JSON line output (`--log-json`); otherwise
///   human-readable `fmt` output.
/// * `level` — default directive (e.g. `"info"`); the `RUST_LOG` environment
///   variable always overrides it.
///
/// Late or repeated calls are no-ops so CLI/TUI/MCP shells can each call
/// this unconditionally at startup. Never logs secret material: this function
/// emits nothing itself and installs no span fields.
pub fn init(json: bool, level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info")));
    if json {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_across_formats() {
        init(false, "info");
        init(true, "debug");
        init(false, "not-a-valid-directive!!!");
    }
}
