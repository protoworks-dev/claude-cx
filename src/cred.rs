//! The shape of a Claude Code credential, and nothing about where it lives.
//!
//! Claude Code stores one JSON object. Its `claudeAiOauth` member is the
//! login: access token, refresh token, expiry in epoch milliseconds, scopes.
//! Other members (MCP server logins and the like) belong to the machine, not
//! to the account, so a switch replaces `claudeAiOauth` and leaves them.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// The member of Claude Code's credential object that holds the login.
pub const OAUTH_KEY: &str = "claudeAiOauth";
/// Treat a token as expired this long before it is, the margin Claude Code uses.
pub const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// One login: the `claudeAiOauth` object, kept whole so that members cx does
/// not know about survive a round trip. It holds the access and refresh
/// tokens, so it is secret: do not log it. Its `Debug` output names the
/// members and the expiry but never prints a token.
#[derive(Clone, PartialEq)]
pub struct Oauth(pub Map<String, Value>);

impl std::fmt::Debug for Oauth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Oauth")
            .field("members", &self.0.keys().collect::<Vec<_>>())
            .field("expires_ms", &self.expires_ms())
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

impl Oauth {
    /// Parse a stored credential. `None` when it holds no usable login.
    pub fn from_credential(text: &str) -> Option<Oauth> {
        let value: Value = serde_json::from_str(text).ok()?;
        let oauth = value.get(OAUTH_KEY)?.as_object()?;
        let access = oauth.get("accessToken")?.as_str()?;
        if access.is_empty() {
            return None;
        }
        Some(Oauth(oauth.clone()))
    }

    /// The access token, sent as the bearer token on API calls. Secret.
    /// Empty only when the object was built without one.
    pub fn access(&self) -> &str {
        self.0.get("accessToken").and_then(Value::as_str).unwrap_or("")
    }

    /// The refresh token. Secret. `None` when the login has none or it is
    /// empty, in which case the login cannot be renewed.
    pub fn refresh(&self) -> Option<&str> {
        self.0
            .get("refreshToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    /// When the access token expires, in milliseconds since the Unix epoch.
    /// 0 when the login gives no numeric expiry, which reads as expired.
    pub fn expires_ms(&self) -> i64 {
        match self.0.get("expiresAt") {
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
            _ => 0,
        }
    }

    /// Whether the access token is still good at `now_ms` (milliseconds
    /// since the Unix epoch), with [`EXPIRY_MARGIN_MS`] to spare.
    pub fn usable_at(&self, now_ms: i64) -> bool {
        self.expires_ms() - now_ms > EXPIRY_MARGIN_MS
    }

    /// A short, stable name for this login that reveals nothing about it.
    /// Keyed on the refresh token, which changes on every refresh.
    pub fn fingerprint(&self) -> String {
        fingerprint(self.refresh().unwrap_or(self.access()))
    }

    /// Whether both logins carry the same access and refresh tokens. Other
    /// members, the expiry included, are not compared.
    pub fn same_tokens(&self, other: &Oauth) -> bool {
        self.access() == other.access() && self.refresh() == other.refresh()
    }

    /// The credential JSON holding only this login, the form
    /// [`Oauth::from_credential`] parses. The text contains the tokens and
    /// is secret. Use [`merge_login`] to keep the other members of a stored
    /// credential.
    pub fn to_credential(&self) -> String {
        json!({ OAUTH_KEY: Value::Object(self.0.clone()) }).to_string()
    }

    /// Apply a token-endpoint reply. A reply without a new refresh token
    /// keeps the old one.
    pub fn apply_refresh(
        &mut self,
        access: &str,
        refresh: Option<&str>,
        expires_in_s: i64,
        scope: Option<&str>,
        now_ms: i64,
    ) {
        self.0.insert("accessToken".into(), json!(access));
        self.0.insert("expiresAt".into(), json!(now_ms + expires_in_s * 1000));
        if let Some(r) = refresh.filter(|r| !r.is_empty()) {
            self.0.insert("refreshToken".into(), json!(r));
        }
        if let Some(s) = scope.filter(|s| !s.is_empty()) {
            let scopes: Vec<&str> = s.split_whitespace().collect();
            self.0.insert("scopes".into(), json!(scopes));
        }
    }
}

/// The first 16 hex digits of the SHA-256 of `token`: safe to log and to
/// compare, and of no use for recovering the token.
pub fn fingerprint(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The credential object to write into a Claude Code home: whatever is there
/// now, with only the login replaced.
pub fn merge_login(existing: Option<&str>, oauth: &Oauth) -> String {
    let mut object = existing
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    object.insert(OAUTH_KEY.into(), Value::Object(oauth.0.clone()));
    Value::Object(object).to_string()
}

/// The kinds of pasted secret cx can tell apart by prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    /// `sk-ant-oat…`: a long-lived inference-only token from `claude setup-token`.
    SetupToken,
    /// Any other `sk-ant-…`: a Console API key, billed per token.
    ApiKey,
}

/// Decide what a pasted secret is from its prefix alone, ignoring
/// surrounding whitespace. `None` when it does not start with `sk-ant-`,
/// or is a refresh token (`sk-ant-ort…`), which cannot run anything.
/// The token is not checked against the server.
pub fn classify_token(text: &str) -> Option<TokenKind> {
    let t = text.trim();
    // A refresh token is neither: it only works at the token endpoint.
    if t.starts_with("sk-ant-ort") {
        None
    } else if t.starts_with("sk-ant-oat") {
        Some(TokenKind::SetupToken)
    } else if t.starts_with("sk-ant-") {
        Some(TokenKind::ApiKey)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(access: &str, refresh: &str, expires: i64) -> Oauth {
        Oauth::from_credential(
            &json!({OAUTH_KEY: {"accessToken": access, "refreshToken": refresh, "expiresAt": expires, "subscriptionType": "max"}})
                .to_string(),
        )
        .unwrap()
    }

    #[test]
    fn parses_and_round_trips() {
        let o = sample("a1", "r1", 1000);
        assert_eq!(o.access(), "a1");
        assert_eq!(o.refresh(), Some("r1"));
        assert_eq!(Oauth::from_credential(&o.to_credential()).unwrap(), o);
    }

    #[test]
    fn rejects_credentials_without_a_login() {
        assert!(Oauth::from_credential("{}").is_none());
        assert!(Oauth::from_credential("not json").is_none());
        assert!(Oauth::from_credential(r#"{"claudeAiOauth":{"accessToken":""}}"#).is_none());
    }

    #[test]
    fn refresh_keeps_unknown_fields_and_old_refresh_token_when_none_is_sent() {
        let mut o = sample("a1", "r1", 1000);
        o.apply_refresh("a2", None, 3600, Some("user:inference user:profile"), 10_000);
        assert_eq!(o.access(), "a2");
        assert_eq!(o.refresh(), Some("r1"));
        assert_eq!(o.expires_ms(), 3_610_000);
        assert_eq!(o.0["subscriptionType"], "max");
        o.apply_refresh("a3", Some("r2"), 3600, None, 10_000);
        assert_eq!(o.refresh(), Some("r2"));
    }

    #[test]
    fn merge_replaces_only_the_login() {
        let live = r#"{"claudeAiOauth":{"accessToken":"old"},"mcpOAuth":{"server":{"t":1}}}"#;
        let merged: Value = serde_json::from_str(&merge_login(Some(live), &sample("new", "r", 5))).unwrap();
        assert_eq!(merged[OAUTH_KEY]["accessToken"], "new");
        assert_eq!(merged["mcpOAuth"]["server"]["t"], 1);
    }

    #[test]
    fn fingerprint_follows_the_refresh_token() {
        let a = sample("a1", "r1", 1);
        let b = sample("a2", "r1", 2);
        let c = sample("a1", "r2", 1);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
    }

    #[test]
    fn token_kinds() {
        assert_eq!(classify_token("sk-ant-oat01-x"), Some(TokenKind::SetupToken));
        assert_eq!(classify_token(" sk-ant-api03-x\n"), Some(TokenKind::ApiKey));
        assert_eq!(classify_token("hello"), None);
    }

    #[test]
    fn debug_output_never_prints_a_token() {
        let oauth = sample("at-secret-value", "rt-secret-value", 5);
        let shown = format!("{oauth:?}");
        assert!(!shown.contains("secret-value"), "{shown}");
        assert!(shown.contains("expires_ms: 5"));
    }

    #[test]
    fn a_refresh_token_is_not_a_key() {
        assert_eq!(classify_token("sk-ant-ort01-abc"), None);
        assert_eq!(classify_token(" sk-ant-oat01-abc "), Some(TokenKind::SetupToken));
        assert_eq!(classify_token("sk-ant-api03-abc"), Some(TokenKind::ApiKey));
    }
}
