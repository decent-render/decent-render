//! Worker-token shape checks shared by every front end that accepts a
//! pasted token (the `decent` CLI's `login`/`--token`, the Tauri app).
//! No signature verification happens on the node — dispatch holds the key;
//! this only refuses pastes that cannot possibly be a fleet token, with a
//! message that names WHAT was wrong and never echoes the token.

/// PACKET 40 (audit-api-ux): a worker token is a JWT — three
/// dot-separated base64url segments, each non-empty, header/payload
/// decodable as JSON, and a payload carrying the claims this fleet mints
/// (service / tenant / platform family). Placeholder strings ("paste-your-
/// token-here", shell leftovers) and truncations fail with a message that
/// names WHAT was wrong — never the token itself.
pub fn validate_worker_token_shape(token: &str) -> anyhow::Result<()> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!(
            "not a worker token: expected three dot-separated parts, got {} \
             (a JWT looks like <header>.<payload>.<signature>)",
            parts.len()
        );
    }
    if parts.iter().any(|p| p.is_empty()) {
        anyhow::bail!("not a worker token: one of the three parts is empty (truncated paste?)");
    }
    if token.len() < 40 {
        anyhow::bail!(
            "not a worker token: {} characters is too short for any JWT this fleet issues",
            token.len()
        );
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' || b == b'=')
    {
        anyhow::bail!(
            "not a worker token: contains characters outside base64url + dots \
             (whitespace, quotes, or a paste artifact)"
        );
    }
    // Header + payload must be real base64url JSON.
    let decode = |s: &str| {
        base64url_decode(s).ok_or_else(|| anyhow::anyhow!("a segment is not valid base64url"))
    };
    let header = decode(parts[0])?;
    let payload = decode(parts[1])?;
    serde_json::from_slice::<serde_json::Value>(&header)
        .map_err(|_| anyhow::anyhow!("token header is not JSON (wrong paste?)"))?;
    let payload: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|_| anyhow::anyhow!("token payload is not JSON (wrong paste?)"))?;
    // Fleet tokens carry worker identity claims; a JWT without ANY of them
    // is some other system's token pasted by mistake.
    let has_claim = [
        "service",
        "tenant",
        "workerId",
        "worker_id",
        "platform",
        "deviceId",
    ]
    .iter()
    .any(|k| payload.get(*k).is_some());
    if !has_claim {
        anyhow::bail!(
            "this JWT carries no worker claims (service/tenant/workerId) — \
             it is probably a token for a different system"
        );
    }
    Ok(())
}

/// Minimal base64url decode (no padding requirement) for shape validation.
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        if b == b'=' {
            break;
        }
        let v = ALPHABET.iter().position(|&a| a == b)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal base64url encode for JWT fixtures (no padding).
    pub(crate) fn jwt_with(payload_json: &str) -> String {
        fn b64(s: &str) -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let bytes = s.as_bytes();
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let mut buf = [0u8; 3];
                buf[..chunk.len()].copy_from_slice(chunk);
                let n = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | buf[2] as u32;
                let count = chunk.len() + 1;
                for i in 0..count {
                    out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        }
        format!(
            "{}.{}.{}",
            b64("{\"alg\":\"HS256\",\"typ\":\"JWT\"}"),
            b64(payload_json),
            "sig-sig-sig-sig-sig-sig-sig-sig-sig"
        )
    }

    #[test]
    fn a_fleet_token_passes_and_round_trips_through_the_decoder() {
        let tok = jwt_with("{\"service\":\"render-worker\",\"tenant\":\"t\",\"workerId\":\"w\"}");
        validate_worker_token_shape(&tok).unwrap();
        let payload = base64url_decode(tok.split('.').nth(1).unwrap()).unwrap();
        assert!(String::from_utf8(payload)
            .unwrap()
            .contains("render-worker"));
    }

    #[test]
    fn placeholders_truncations_and_foreign_jwts_are_refused_without_echo() {
        for (bad, why) in [
            ("paste-your-token-here", "three dot-separated parts"),
            ("a.b.", "one of the three parts is empty"),
            ("abc.def.ghi", "too short"),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJmb28iOiJiYXIifQ.c2lnbmF0dXJlLXNpZ25hdHVyZS1zaWduYXR1cmU",
                "no worker claims",
            ),
        ] {
            let err = validate_worker_token_shape(bad).unwrap_err().to_string();
            assert!(err.contains(why), "{bad}: {err}");
            assert!(!err.contains(bad), "the token must never be echoed: {err}");
        }
        let quoted = format!("\"{}\"", jwt_with("{\"service\":\"render-worker\"}"));
        let err = validate_worker_token_shape(&quoted)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside base64url"), "{err}");
    }

    #[test]
    fn base64url_decode_rejects_alphabet_violations_and_stops_at_padding() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi");
        assert!(base64url_decode("aG+k").is_none());
    }
}
