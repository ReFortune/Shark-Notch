//! Claude's plan limits (the 5-hour and weekly bars of `/usage`), the pure part: reading Claude
//! Code's saved login and the answer of Anthropic's usage endpoint.
//!
//! Opt-in (`[stats] ai_limits`, off by default). The login token never leaves the service that
//! sends it to `api.anthropic.com`; it is not logged, not put in any event and not printed (the
//! `Debug` of [`Credentials`] hides it). The endpoint is not a documented, supported API: if
//! Anthropic changes it, the bars say "unavailable" and nothing else breaks.

use serde::Deserialize;

use crate::aiusage::parse_time;

/// One limit window: how much is used and when it starts over.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// 0..=100.
    pub used: f32,
    /// UTC seconds since 1970 (`0`: not known).
    pub resets_at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// What is known about the limits.
#[derive(Clone, Debug, PartialEq)]
pub enum LimitsState {
    Known(Limits),
    /// No Claude Code login on this PC (or it has no usable token).
    SignedOut,
    /// The saved token has run out; Claude Code renews it the next time it is used.
    Expired,
    /// The request failed or the answer was not understood.
    Failed,
}

#[derive(Deserialize)]
struct RawWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct RawUsage {
    five_hour: Option<RawWindow>,
    seven_day: Option<RawWindow>,
}

fn window(w: Option<RawWindow>) -> Option<Window> {
    let w = w?;
    let used = w.utilization?;
    if !used.is_finite() {
        return None;
    }
    Some(Window {
        used: used.clamp(0.0, 100.0) as f32,
        resets_at: w.resets_at.as_deref().and_then(parse_time).unwrap_or(0),
    })
}

/// The usage endpoint's answer. `None` when neither window is there.
pub fn parse_usage(body: &[u8]) -> Option<Limits> {
    let raw: RawUsage = serde_json::from_slice(body).ok()?;
    let l = Limits {
        five_hour: window(raw.five_hour),
        seven_day: window(raw.seven_day),
    };
    (l.five_hour.is_some() || l.seven_day.is_some()).then_some(l)
}

/// Claude Code's saved login (`~/.claude/.credentials.json`): only the access token and when it ends.
pub struct Credentials {
    token: String,
    expires_ms: i64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"<hidden>")
            .field("expires_ms", &self.expires_ms)
            .finish()
    }
}

#[derive(Deserialize)]
struct RawOauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct RawCredentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<RawOauth>,
}

impl Credentials {
    pub fn parse(json: &str) -> Option<Credentials> {
        let o = serde_json::from_str::<RawCredentials>(json).ok()?.oauth?;
        let token = o.access_token.filter(|t| !t.is_empty())?;
        Some(Credentials {
            token,
            expires_ms: o.expires_at.unwrap_or(0),
        })
    }

    /// Has the token run out at `now` (UTC seconds)? A missing expiry counts as "not known to have".
    pub fn expired(&self, now: i64) -> bool {
        self.expires_ms > 0 && self.expires_ms / 1000 <= now
    }

    /// The value for an `Authorization` header.
    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

/// "2 h 10 min", "45 min", "now": how long until `at`.
pub fn fmt_until(at: i64, now: i64) -> String {
    let secs = (at - now).max(0);
    let mins = secs / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "now".to_string(),
        (0, m) => format!("{m} min"),
        (h, 0) if h < 48 => format!("{h} h"),
        (h, m) if h < 48 => format!("{h} h {m} min"),
        (h, _) => format!("{} d {} h", h / 24, h % 24),
    }
}

/// The same, as short as it goes: "2h10m", "45m", "3d2h".
pub fn fmt_until_short(at: i64, now: i64) -> String {
    let mins = (at - now).max(0) / 60;
    match (mins / 60, mins % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) if h < 48 => format!("{h}h"),
        (h, m) if h < 48 => format!("{h}h{m:02}m"),
        (h, _) => format!("{}d{}h", h / 24, h % 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USAGE: &str = r#"{"five_hour":{"utilization":37.5,"resets_at":"2026-10-09T08:00:00.123456+00:00"},"seven_day":{"utilization":120.0,"resets_at":"2026-10-12T03:30:00Z"},"seven_day_opus":null}"#;

    #[test]
    fn the_usage_answer_gives_two_windows() {
        let l = parse_usage(USAGE.as_bytes()).unwrap();
        let five = l.five_hour.unwrap();
        assert_eq!(five.used, 37.5);
        assert_eq!(
            five.resets_at,
            crate::civil::unix_from_civil(2026, 10, 9, 8, 0, 0)
        );
        // More than 100 % is clamped; a plain Z stamp works too.
        assert_eq!(l.seven_day.unwrap().used, 100.0);
        assert_eq!(
            l.seven_day.unwrap().resets_at,
            crate::civil::unix_from_civil(2026, 10, 12, 3, 30, 0)
        );
    }

    #[test]
    fn a_missing_or_odd_answer_is_not_a_reading() {
        for body in [
            "",
            "not json",
            "{}",
            r#"{"five_hour":null,"seven_day":null}"#,
            r#"{"five_hour":{"resets_at":"2026-10-09T08:00:00Z"}}"#,
        ] {
            assert_eq!(parse_usage(body.as_bytes()), None, "{body}");
        }
        // One window is enough.
        let one = parse_usage(br#"{"seven_day":{"utilization":5}}"#).unwrap();
        assert!(one.five_hour.is_none() && one.seven_day.unwrap().resets_at == 0);
    }

    #[test]
    fn credentials_are_read_checked_for_expiry_and_never_printed() {
        let c = Credentials::parse(
            r#"{"claudeAiOauth":{"accessToken":"sk-secret-1","refreshToken":"r","expiresAt":2000000,"scopes":["x"]}}"#,
        )
        .unwrap();
        assert_eq!(c.bearer(), "Bearer sk-secret-1");
        assert!(!c.expired(1999) && c.expired(2000) && c.expired(9999));
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("secret") && shown.contains("hidden"),
            "{shown}"
        );
        for bad in [
            "",
            "{}",
            r#"{"claudeAiOauth":{}}"#,
            r#"{"claudeAiOauth":{"accessToken":""}}"#,
        ] {
            assert!(Credentials::parse(bad).is_none(), "{bad}");
        }
        // No expiry given: not assumed to have run out.
        let c = Credentials::parse(r#"{"claudeAiOauth":{"accessToken":"t"}}"#).unwrap();
        assert!(!c.expired(i64::MAX / 2));
    }

    #[test]
    fn times_until_a_reset_read_naturally() {
        for (secs, s) in [
            (0, "now"),
            (59, "now"),
            (60, "1 min"),
            (3600, "1 h"),
            (7800, "2 h 10 min"),
            (3 * 86_400 + 7200, "3 d 2 h"),
        ] {
            assert_eq!(fmt_until(1_000 + secs, 1_000), s, "{secs}");
        }
        assert_eq!(fmt_until(5, 1_000), "now", "already past");
        for (secs, s) in [
            (0, "0m"),
            (45 * 60, "45m"),
            (7800, "2h10m"),
            (3600, "1h"),
            (3 * 86_400 + 7200, "3d2h"),
        ] {
            assert_eq!(fmt_until_short(1_000 + secs, 1_000), s, "{secs}");
        }
    }
}
