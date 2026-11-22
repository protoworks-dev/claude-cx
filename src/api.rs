//! The three Anthropic endpoints cx talks to: token refresh, usage, profile.
//!
//! The addresses, the client id and the request shapes are the ones Claude
//! Code itself uses, as recorded by claude-swap. `CX_API_BASE` and
//! `CX_TOKEN_URL` point them elsewhere for tests.

use crate::store::{ExtraUsage, ModelWindow, Usage, Window};
use serde_json::{Value, json};
use std::time::Duration;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const USER_AGENT: &str = concat!("cx/", env!("CARGO_PKG_VERSION"));

/// Why a call to one of the endpoints failed. The variants separate the
/// cases a caller treats differently: sign in again, wait, or try again.
#[derive(Clone, PartialEq)]
pub enum ApiError {
    /// The server rejected the token itself. For a refresh this means the
    /// refresh token is dead and the account needs a new sign-in.
    Rejected(String),
    /// The server answered 429.
    RateLimited {
        /// Seconds to wait before the next try, from the `Retry-After`
        /// header. `None` when the header is absent or not a number.
        retry_after_s: Option<u64>,
    },
    /// Any other error status, carried as the HTTP status code. The token
    /// is not known to be bad, so the call may be tried again.
    Http(u16),
    /// The request did not complete: no connection, a timeout, or a reply
    /// that broke off. Carries the transport's message.
    Network(String),
    /// The server answered with success but the body was not the JSON cx
    /// expects. Carries a short reason, not the body.
    BadResponse(String),
    /// The token endpoint answered 200 with a body cx could not use. The
    /// refresh token is spent, so the body is carried here to be kept.
    /// The body may hold new tokens: treat it as secret and do not log it.
    UnusableTokenReply(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Rejected(why) => write!(f, "rejected ({why})"),
            ApiError::RateLimited { retry_after_s: Some(s) } => write!(f, "rate limited, retry in {s}s"),
            ApiError::RateLimited { retry_after_s: None } => write!(f, "rate limited"),
            ApiError::Http(code) => write!(f, "HTTP {code}"),
            ApiError::Network(why) => write!(f, "network: {why}"),
            ApiError::BadResponse(why) => write!(f, "unexpected reply: {why}"),
            ApiError::UnusableTokenReply(_) => write!(f, "the token endpoint sent a reply cx could not read"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Who a token belongs to, as the server reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Identity {
    /// The account's id on Anthropic's side, the same in every login of
    /// that account.
    pub uuid: String,
    /// The account's email address. `None` when the reply left it out.
    pub email: Option<String>,
    /// The id of the organization the login is for. One account can sign in
    /// to several organizations. `None` when the reply left it out.
    pub org_uuid: Option<String>,
}

/// A successful reply from the token endpoint. Both tokens are secret and
/// must not be logged. Its `Debug` output leaves the tokens out.
#[derive(Clone)]
pub struct TokenReply {
    /// The new access token, sent as the bearer token on API calls. Secret.
    pub access_token: String,
    /// The refresh token that replaces the one just spent. Secret. `None`
    /// when the server sent none, in which case the old one stays valid.
    pub refresh_token: Option<String>,
    /// Seconds from now until the access token expires. 3600 when the reply
    /// gave no positive number.
    pub expires_in_s: i64,
    /// The granted scopes as one space-separated string, when sent.
    pub scope: Option<String>,
    /// Who the token belongs to. The server does not always say.
    pub identity: Option<Identity>,
}

/// What the profile endpoint says about the account behind an access token.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// The account and organization the token belongs to.
    pub identity: Identity,
    /// The organization's display name, when sent.
    pub org_name: Option<String>,
    /// A short plan name such as `Max 20x`, from [`plan_label`]. `None`
    /// when the reply names no plan.
    pub plan: Option<String>,
}

impl std::fmt::Debug for TokenReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenReply")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<present>"))
            .field("expires_in_s", &self.expires_in_s)
            .field("scope", &self.scope)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// The same words as `Display`: an unreadable token reply holds a token, so
/// its body is never printed.
impl std::fmt::Debug for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiError({self})")
    }
}

/// An HTTP client for the three endpoints. Cloning it is cheap and the
/// clones share one connection pool. Every call blocks, for at most 12
/// seconds.
#[derive(Clone)]
pub struct Api {
    base: String,
    token_url: String,
    agent: ureq::Agent,
}

type Reply = ureq::http::Response<ureq::Body>;

fn retry_after(response: &Reply) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|s| s.max(0.0) as u64)
}

/// Turn a transport failure or an error status into an [`ApiError`]. The
/// agent hands back every status as a response, so the body of an error
/// reply can still be read by the caller that needs it.
fn checked(result: Result<Reply, ureq::Error>) -> Result<Reply, ApiError> {
    let response = result.map_err(|e| ApiError::Network(e.to_string()))?;
    match response.status().as_u16() {
        200..=299 => Ok(response),
        429 => Err(ApiError::RateLimited {
            retry_after_s: retry_after(&response),
        }),
        401 => Err(ApiError::Rejected("401".into())),
        code => Err(ApiError::Http(code)),
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

impl Default for Api {
    fn default() -> Api {
        Api::new()
    }
}

impl Api {
    /// Build a client for Anthropic's servers, or for the addresses in
    /// `CX_API_BASE` and `CX_TOKEN_URL` when those are set. Makes no
    /// request. Connecting times out after 6 seconds and a whole call after
    /// 12.
    pub fn new() -> Api {
        let base = std::env::var("CX_API_BASE").unwrap_or_else(|_| "https://api.anthropic.com".into());
        let token_url =
            std::env::var("CX_TOKEN_URL").unwrap_or_else(|_| "https://platform.claude.com/v1/oauth/token".into());
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(6)))
            .timeout_global(Some(Duration::from_secs(12)))
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .build()
            .new_agent();
        Api {
            base: base.trim_end_matches('/').to_string(),
            token_url,
            agent,
        }
    }

    /// Exchange a refresh token for a new access token. The old refresh
    /// token is spent the moment the server answers with a new one, so the
    /// caller must store the reply before doing anything else.
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenReply, ApiError> {
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        });
        let mut response = self
            .agent
            .post(&self.token_url)
            .header("Content-Type", "application/json")
            .send_json(body)
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let code = response.status().as_u16();
        if matches!(code, 400 | 401 | 403) {
            // RFC 6749 5.2: the verdict is the top-level `error` member.
            // Only an explicit grant error counts as a dead token; any
            // other reply may be a gateway and is worth another try.
            let verdict = response
                .body_mut()
                .read_json::<Value>()
                .ok()
                .and_then(|v| text(&v, "error"));
            return Err(match verdict.as_deref() {
                Some("invalid_grant") => ApiError::Rejected("invalid_grant".into()),
                _ => ApiError::Http(code),
            });
        }
        let mut response = checked(Ok(response))?;
        // From here the old refresh token may be spent. Nothing below may
        // drop the body: an unreadable one goes back to the caller whole.
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let Ok(data) = serde_json::from_str::<Value>(&body) else {
            return Err(ApiError::UnusableTokenReply(body));
        };
        let Some(access_token) = text(&data, "access_token") else {
            return Err(ApiError::UnusableTokenReply(body));
        };
        // Claude Code's access tokens last hours; an hour is a safe reading
        // of a reply that does not say.
        let expires_in_s = data
            .get("expires_in")
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .filter(|s| *s > 0)
            .unwrap_or(3600);
        let identity = data.get("account").and_then(|a| text(a, "uuid")).map(|uuid| Identity {
            uuid,
            email: data.get("account").and_then(|a| text(a, "email_address")),
            org_uuid: data.get("organization").and_then(|o| text(o, "uuid")),
        });
        Ok(TokenReply {
            access_token,
            refresh_token: text(&data, "refresh_token"),
            expires_in_s,
            scope: text(&data, "scope"),
            identity,
        })
    }

    /// Ask the server how much of each rate-limit window the account has
    /// used. One network request. Fails with [`ApiError::Rejected`] when the
    /// access token is expired or revoked, and with
    /// [`ApiError::BadResponse`] when the reply holds no usable window.
    pub fn usage(&self, access_token: &str) -> Result<Usage, ApiError> {
        parse_usage(&self.usage_raw(access_token)?)
    }

    /// The usage endpoint's reply as sent, for `cx raw-usage`.
    pub fn usage_raw(&self, access_token: &str) -> Result<Value, ApiError> {
        let result = self
            .agent
            .get(format!("{}/api/oauth/usage", self.base))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-beta", OAUTH_BETA)
            .call();
        checked(result)?
            .body_mut()
            .read_json()
            .map_err(|e| ApiError::BadResponse(e.to_string()))
    }

    /// Ask the server whose access token this is. One network request.
    /// Fails with [`ApiError::Rejected`] when the token is expired or
    /// revoked, and with [`ApiError::BadResponse`] when the reply has no
    /// account uuid.
    pub fn profile(&self, access_token: &str) -> Result<Profile, ApiError> {
        let result = self
            .agent
            .get(format!("{}/api/oauth/profile", self.base))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .call();
        let data: Value = checked(result)?
            .body_mut()
            .read_json()
            .map_err(|e| ApiError::BadResponse(e.to_string()))?;
        let account = data.get("account").cloned().unwrap_or(Value::Null);
        let org = data.get("organization").cloned().unwrap_or(Value::Null);
        let uuid = text(&account, "uuid").ok_or_else(|| ApiError::BadResponse("no account.uuid".into()))?;
        Ok(Profile {
            identity: Identity {
                uuid,
                email: text(&account, "email"),
                org_uuid: text(&org, "uuid"),
            },
            org_name: text(&org, "name"),
            plan: plan_label(&org),
        })
    }
}

/// A short plan name. The organization type names Team, Enterprise and Pro;
/// the rate-limit tier tells the Max sizes apart.
pub fn plan_label(org: &Value) -> Option<String> {
    let tier = text(org, "rate_limit_tier");
    let label = match text(org, "organization_type").as_deref() {
        Some("claude_team") => Some("Team"),
        Some("claude_enterprise") => Some("Enterprise"),
        Some("claude_pro") => Some("Pro"),
        _ => None,
    };
    if let Some(l) = label {
        return Some(l.into());
    }
    match tier.as_deref() {
        Some("default_claude_max_20x") => Some("Max 20x".into()),
        Some("default_claude_max_5x") => Some("Max 5x".into()),
        Some("default_claude_ai") => Some("Free".into()),
        _ => text(org, "plan_display_name").or(tier),
    }
}

fn window(data: &Value, key: &str) -> Result<Option<Window>, ApiError> {
    let Some(w) = data.get(key).filter(|w| !w.is_null()) else {
        return Ok(None);
    };
    let Some(object) = w.as_object().filter(|o| !o.is_empty()) else {
        return Ok(None);
    };
    let pct = match object.get("utilization") {
        Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.is_finite() && f >= 0.0 => Some(f),
            _ => return Err(ApiError::BadResponse(format!("{key}.utilization is not a percentage"))),
        },
        _ => return Err(ApiError::BadResponse(format!("{key}.utilization is missing"))),
    };
    Ok(Some(Window {
        pct,
        resets_at: text(w, "resets_at"),
    }))
}

/// Read a usage reply (see [`Api::usage_raw`]) into a [`Usage`]. Touches no
/// network. Extra-usage amounts arrive in hundredths of the currency unit
/// and are converted to whole units. Fails with [`ApiError::BadResponse`]
/// when a window is malformed or the reply holds no window at all.
pub fn parse_usage(data: &Value) -> Result<Usage, ApiError> {
    let mut usage = Usage {
        five_hour: window(data, "five_hour")?,
        seven_day: window(data, "seven_day")?,
        ..Usage::default()
    };
    if let Some(limits) = data.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let name = limit.pointer("/scope/model/display_name").and_then(Value::as_str);
            let pct = limit.get("percent").and_then(Value::as_f64);
            if let (Some(name), Some(pct)) = (name, pct) {
                usage.models.push(ModelWindow {
                    name: name.to_string(),
                    window: Window {
                        pct: Some(pct),
                        resets_at: text(limit, "resets_at"),
                    },
                });
            }
        }
    }
    if let Some(extra) = data
        .get("extra_usage")
        .filter(|e| e.get("is_enabled").and_then(Value::as_bool) == Some(true))
    {
        let number = |key: &str| extra.get(key).and_then(Value::as_f64);
        if let (Some(used), Some(limit), Some(pct)) =
            (number("used_credits"), number("monthly_limit"), number("utilization"))
        {
            usage.extra = Some(ExtraUsage {
                used: used / 100.0,
                limit: limit / 100.0,
                pct,
                currency: text(extra, "currency").unwrap_or_else(|| "USD".into()),
            });
        }
    }
    if usage.five_hour.is_none() && usage.seven_day.is_none() && usage.models.is_empty() {
        return Err(ApiError::BadResponse("no usage windows in the reply".into()));
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_models_and_extra_usage() {
        let data = json!({
            "five_hour": {"utilization": 25.0, "resets_at": "2026-06-22T23:29:59Z"},
            "seven_day": {"utilization": 16, "resets_at": "2026-06-26T17:59:59Z"},
            "limits": [
                {"kind": "weekly_scoped", "percent": 80.5, "resets_at": "2026-06-26T17:59:59Z",
                 "scope": {"model": {"display_name": "Fable"}}},
                {"kind": "weekly", "percent": 16}
            ],
            "extra_usage": {"is_enabled": true, "used_credits": 1250, "monthly_limit": 5000, "utilization": 25.0}
        });
        let u = parse_usage(&data).unwrap();
        assert_eq!(u.five_hour.as_ref().unwrap().pct, Some(25.0));
        assert_eq!(u.seven_day.as_ref().unwrap().pct, Some(16.0));
        assert_eq!(u.models.len(), 1);
        assert_eq!(u.models[0].name, "Fable");
        let extra = u.extra.unwrap();
        assert_eq!((extra.used, extra.limit), (12.5, 50.0));
    }

    #[test]
    fn an_account_without_a_five_hour_window_is_still_a_reading() {
        let u = parse_usage(&json!({"five_hour": null, "seven_day": {"utilization": null}})).unwrap();
        assert!(u.five_hour.is_none());
        assert_eq!(u.seven_day.unwrap().pct, None);
    }

    #[test]
    fn a_malformed_window_fails_the_whole_reading() {
        assert!(parse_usage(&json!({"five_hour": {"resets_at": "x"}, "seven_day": {"utilization": 1}})).is_err());
        assert!(parse_usage(&json!({"five_hour": {"utilization": "many"}})).is_err());
        assert!(parse_usage(&json!({})).is_err());
    }

    #[test]
    fn plan_labels() {
        assert_eq!(
            plan_label(&json!({"organization_type": "claude_team", "rate_limit_tier": "x"})).as_deref(),
            Some("Team")
        );
        assert_eq!(
            plan_label(&json!({"organization_type": "claude_max", "rate_limit_tier": "default_claude_max_20x"}))
                .as_deref(),
            Some("Max 20x")
        );
        assert_eq!(
            plan_label(&json!({"rate_limit_tier": "something_new"})).as_deref(),
            Some("something_new")
        );
    }
}
