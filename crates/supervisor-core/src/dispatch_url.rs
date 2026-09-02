//! The dispatch URL every front end dials — and the one rule about it.
//!
//! Shared by the `decent` CLI and the Tauri app so neither can drift: the
//! CLI got this gate in packet 40 (audit 17) while the app kept a cleartext
//! `ws://localhost` default and no scheme check for months.

/// The production dispatch WebSocket URL. `start`/`install`/`tui` default to
/// it, `decent doctor` derives its `/health` probe from it, the app pre-fills
/// its settings with it.
pub const DEFAULT_DISPATCH_WS: &str = "wss://decent-render-dispatch.fly.dev/ws";

/// Validate the dispatch URL scheme BEFORE anything dials it. A `ws://` URL
/// to a remote host would ship the worker JWT in CLEARTEXT — refuse it and
/// say exactly why. Plain `ws://` stays allowed for localhost/127.0.0.1 (the
/// e2e harness and local development). Never silently "upgrade" the scheme:
/// the operator must see the mistake.
pub fn validate_dispatch_url(url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| anyhow::anyhow!("--dispatch-url is not a valid URL ({e}): {url}"))?;
    let scheme = parsed.scheme();
    let is_local = matches!(
        parsed.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("[::1]") | None
    );
    match scheme {
        "wss" => Ok(()),
        "ws" if is_local => Ok(()),
        "ws" => anyhow::bail!(
            "refusing to use ws:// to a non-local host: the worker token would be sent in \
             CLEARTEXT. Use wss://{host}{path} (or a localhost URL for local development).",
            host = parsed.host_str().unwrap_or("<unknown>"),
            path = parsed.path(),
        ),
        other => anyhow::bail!(
            "--dispatch-url must be wss:// (or ws:// for localhost); got '{other}://'"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_passes_its_own_gate() {
        assert!(validate_dispatch_url(DEFAULT_DISPATCH_WS).is_ok());
        assert!(DEFAULT_DISPATCH_WS.starts_with("wss://"));
    }

    #[test]
    fn wss_urls_are_accepted_everywhere() {
        assert!(validate_dispatch_url("wss://decent-render-dispatch.fly.dev/ws").is_ok());
        assert!(validate_dispatch_url("wss://example.com/?a=1&b=2").is_ok());
    }

    #[test]
    fn plain_ws_is_allowed_only_for_localhost() {
        // The e2e harness and local development.
        assert!(validate_dispatch_url("ws://localhost:8790/ws").is_ok());
        assert!(validate_dispatch_url("ws://127.0.0.1:8790/ws").is_ok());
        assert!(validate_dispatch_url("ws://[::1]:8790/ws").is_ok());
        // A REMOTE host over ws:// ships the JWT in cleartext — refused,
        // with a message that says why and names the fix.
        let err = validate_dispatch_url("ws://dispatch.example.com/ws")
            .unwrap_err()
            .to_string();
        assert!(err.contains("CLEARTEXT"), "got: {err}");
        assert!(
            err.contains("wss://dispatch.example.com/ws"),
            "must name the fix: {err}"
        );
    }

    #[test]
    fn non_ws_schemes_are_refused_with_the_scheme_named() {
        let err = validate_dispatch_url("http://example.com/ws")
            .unwrap_err()
            .to_string();
        assert!(err.contains("http"), "got: {err}");
        assert!(validate_dispatch_url("not a url at all").is_err());
    }
}
