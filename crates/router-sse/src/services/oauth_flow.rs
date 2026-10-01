//! OAuth provider flows.
//!
//! Only the providers that carry a `providerOauth` entry in the registry are
//! implemented: codex, grok-cli, kilocode and codebuddy-intl. Each
//! follows the same
//! `{config, flowType, buildAuthUrl, exchangeToken, postExchange, mapTokens}`
//! shape.
//!
//! Provider config comes from `registry.json`'s `providerOauth`, never from a
//! second table here — the same rule the rest of the registry follows.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::executors::http::{ProxyOptions, prepare_send};
use crate::providers::registry::registry;

/// The result of `generateAuthData`.
pub struct AuthData {
    pub auth_url: Option<String>,
    pub state: String,
    pub code_verifier: String,
    pub code_challenge: String,
    pub redirect_uri: String,
    pub flow_type: String,
    pub fixed_port: Option<u16>,
    pub callback_path: String,
}

impl AuthData {
    /// The JSON shape the route returns, key order included. `fixedPort` is
    /// omitted when the provider has none — `JSON.stringify` drops `undefined`.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert(
            "authUrl".into(),
            self.auth_url.clone().map_or(Value::Null, Value::String),
        );
        m.insert("state".into(), Value::String(self.state.clone()));
        m.insert(
            "codeVerifier".into(),
            Value::String(self.code_verifier.clone()),
        );
        m.insert(
            "codeChallenge".into(),
            Value::String(self.code_challenge.clone()),
        );
        m.insert(
            "redirectUri".into(),
            Value::String(self.redirect_uri.clone()),
        );
        m.insert("flowType".into(), Value::String(self.flow_type.clone()));
        if let Some(port) = self.fixed_port {
            m.insert("fixedPort".into(), json!(port));
        }
        m.insert(
            "callbackPath".into(),
            Value::String(self.callback_path.clone()),
        );
        Value::Object(m)
    }
}

/// `generatePKCE(bytes = 32)`.
fn generate_pkce() -> (String, String, String) {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let verifier = URL_SAFE_NO_PAD.encode(raw);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut state_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut state_bytes);
    (verifier, challenge, URL_SAFE_NO_PAD.encode(state_bytes))
}

/// `registry.providerOauth[provider]`, or an error naming the provider.
fn oauth_config(provider: &str) -> Result<Value, String> {
    registry()
        .oauth(provider)
        .cloned()
        .ok_or_else(|| format!("Unknown provider: {provider}"))
}

fn cfg_str<'a>(cfg: &'a Value, key: &str) -> Option<&'a str> {
    cfg.get(key).and_then(Value::as_str)
}

/// `URLSearchParams`-style form encoding.
///
/// A sync fn on purpose: `form_urlencoded::Serializer` is not `Send`, so a
/// local one held across an `await` would make the enclosing future non-`Send`
/// and every axum handler awaiting it would fail its `Handler` bound.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        q.append_pair(k, v);
    }
    q.finish()
}

/// `flowType` per provider. Kept as a table because `registry.json` stores the
/// config, not the flow.
pub fn flow_type(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "codex" => "authorization_code_pkce",
        "grok-cli" | "kilocode" | "codebuddy-intl" => "device_code",
        _ => return None,
    })
}

/// `fixedPort` per provider.
fn fixed_port(provider: &str) -> Option<u16> {
    match provider {
        "codex" => Some(1455),
        _ => None,
    }
}

/// `callbackPath` per provider; the default is `/callback`.
fn callback_path(provider: &str) -> &'static str {
    match provider {
        "codex" => "/auth/callback",
        _ => "/callback",
    }
}

/// `generateAuthData(providerName, redirectUri, meta)`.
pub fn generate_auth_data(
    provider: &str,
    redirect_uri: &str,
    meta: &Map<String, Value>,
) -> Result<AuthData, String> {
    let cfg = oauth_config(provider)?;
    let flow = flow_type(provider).ok_or_else(|| format!("Unknown provider: {provider}"))?;
    let (code_verifier, code_challenge, state) = generate_pkce();

    let auth_url = if flow == "device_code" {
        None
    } else {
        Some(build_auth_url(
            provider,
            &cfg,
            redirect_uri,
            &state,
            &code_challenge,
            meta,
        )?)
    };

    Ok(AuthData {
        auth_url,
        state,
        code_verifier,
        code_challenge,
        redirect_uri: redirect_uri.to_string(),
        flow_type: flow.to_string(),
        fixed_port: fixed_port(provider),
        callback_path: callback_path(provider).to_string(),
    })
}

/// `buildAuthUrl(config, redirectUri, state, codeChallenge, meta)`.
fn build_auth_url(
    provider: &str,
    cfg: &Value,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    _meta: &Map<String, Value>,
) -> Result<String, String> {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    match provider {
        "codex" => {
            q.append_pair("response_type", "code");
            q.append_pair("client_id", cfg_str(cfg, "clientId").unwrap_or_default());
            q.append_pair("redirect_uri", redirect_uri);
            q.append_pair("scope", cfg_str(cfg, "scope").unwrap_or_default());
            q.append_pair("code_challenge", code_challenge);
            q.append_pair(
                "code_challenge_method",
                cfg_str(cfg, "codeChallengeMethod").unwrap_or("S256"),
            );
            if let Some(extra) = cfg.get("extraParams").and_then(Value::as_object) {
                for (k, v) in extra {
                    q.append_pair(k, v.as_str().unwrap_or_default());
                }
            }
            q.append_pair("state", state);
        }
        _ => return Err(format!("Unknown provider: {provider}")),
    }
    Ok(format!(
        "{}?{}",
        cfg_str(cfg, "authorizeUrl").unwrap_or_default(),
        q.finish()
    ))
}

/// `exchangeTokens(providerName, code, redirectUri, codeVerifier, state, meta)`.
///
/// Returns the provider's `mapTokens` output: the fields a connection row takes.
pub async fn exchange_tokens(
    provider: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    _state: &str,
    _meta: &Map<String, Value>,
) -> Result<Value, String> {
    let cfg = oauth_config(provider)?;
    match provider {
        "codex" => {
            let body = encode_form(&[
                ("grant_type", "authorization_code"),
                ("client_id", cfg_str(&cfg, "clientId").unwrap_or_default()),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", code_verifier),
            ]);
            let res = post_form(cfg_str(&cfg, "tokenUrl").unwrap_or_default(), &body, &[]).await?;
            let t = parse_ok(&res, "Token exchange failed")?;
            let id_token = t.get("id_token").and_then(Value::as_str);
            let info = extract_codex_account_info(id_token.unwrap_or_default());
            let mut out = Map::new();
            out.insert(
                "accessToken".into(),
                t.get("access_token").cloned().unwrap_or(Value::Null),
            );
            out.insert(
                "refreshToken".into(),
                t.get("refresh_token").cloned().unwrap_or(Value::Null),
            );
            out.insert(
                "idToken".into(),
                t.get("id_token").cloned().unwrap_or(Value::Null),
            );
            out.insert(
                "expiresIn".into(),
                t.get("expires_in").cloned().unwrap_or(Value::Null),
            );
            out.insert("lastRefreshAt".into(), Value::String(now_iso()));
            let email = info
                .get("email")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    extract_email_from_access_token(
                        t.get("access_token")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
                });
            if let Some(email) = email {
                out.insert("email".into(), Value::String(email));
            }
            if info.get("chatgptAccountId").is_some() || info.get("chatgptPlanType").is_some() {
                out.insert(
                    "providerSpecificData".into(),
                    json!({
                        "chatgptAccountId": info.get("chatgptAccountId").cloned().unwrap_or(Value::Null),
                        "chatgptPlanType": info.get("chatgptPlanType").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
            Ok(Value::Object(out))
        }
        _ => Err(format!("Unknown provider: {provider}")),
    }
}

/// `requestDeviceCode(providerName, codeChallenge, options)`.
pub async fn request_device_code(
    provider: &str,
    _code_challenge: Option<&str>,
) -> Result<Value, String> {
    let cfg = oauth_config(provider)?;
    match provider {
        "grok-cli" => {
            let mut pairs = vec![
                ("client_id", cfg_str(&cfg, "clientId").unwrap_or_default()),
                ("scope", cfg_str(&cfg, "scope").unwrap_or_default()),
            ];
            if let Some(referrer) = cfg_str(&cfg, "referrer") {
                pairs.push(("referrer", referrer));
            }
            let body = encode_form(&pairs);
            let ua = "grok-pager/0.2.93 grok-shell/0.2.93 (linux; x86_64)";
            let res = post_form(
                cfg_str(&cfg, "deviceCodeUrl").unwrap_or_default(),
                &body,
                &[("Accept", "application/json"), ("User-Agent", ua)],
            )
            .await?;
            if !(200..300).contains(&res.status) {
                return Err(format!("Grok CLI device code request failed: {}", res.body));
            }
            serde_json::from_str(&res.body).map_err(|e| e.to_string())
        }
        "kilocode" => {
            let res =
                post_raw_json(cfg_str(&cfg, "initiateUrl").unwrap_or_default(), "{}", &[]).await?;
            if res.status == 429 {
                return Err(
                    "Too many pending authorization requests. Please try again later.".into(),
                );
            }
            if !(200..300).contains(&res.status) {
                return Err(format!("Device auth initiation failed: {}", res.body));
            }
            let data: Value = serde_json::from_str(&res.body).map_err(|e| e.to_string())?;
            let code = data.get("code").cloned().unwrap_or(Value::Null);
            let verification = data.get("verificationUrl").cloned().unwrap_or(Value::Null);
            Ok(json!({
                "device_code": code,
                "user_code": code,
                "verification_uri": verification,
                "verification_uri_complete": verification,
                "expires_in": data.get("expiresIn").and_then(Value::as_i64).unwrap_or(300),
                "interval": 3,
            }))
        }
        "codebuddy-intl" => {
            let url = format!(
                "{}?platform={}",
                cfg_str(&cfg, "stateUrl").unwrap_or_default(),
                cfg_str(&cfg, "platform").unwrap_or("ide")
            );
            let ua = cfg_str(&cfg, "userAgent").unwrap_or_default();
            let res = post_raw_json(
                &url,
                "{}",
                &[
                    ("Accept", "application/json"),
                    ("User-Agent", ua),
                    ("X-Requested-With", "XMLHttpRequest"),
                    ("X-Domain", "www.codebuddy.ai"),
                    ("X-No-Authorization", "true"),
                    ("X-No-User-Id", "true"),
                    ("X-Product", "SaaS"),
                ],
            )
            .await?;
            if !(200..300).contains(&res.status) {
                return Err(format!("CodeBuddy Intl state request failed: {}", res.body));
            }
            let data: Value = serde_json::from_str(&res.body).map_err(|e| e.to_string())?;
            if data.get("code").and_then(Value::as_i64) != Some(0) {
                return Err(format!(
                    "CodeBuddy Intl state error: {}",
                    data.get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("missing state/authUrl")
                ));
            }
            let inner = data.get("data").cloned().unwrap_or_else(|| json!({}));
            let state = inner.get("state").cloned().unwrap_or(Value::Null);
            let auth_url = inner.get("authUrl").cloned().unwrap_or(Value::Null);
            if state.is_null() || auth_url.is_null() {
                return Err("CodeBuddy Intl state error: missing state/authUrl".into());
            }
            let interval = cfg
                .get("pollInterval")
                .and_then(Value::as_f64)
                .unwrap_or(5000.0)
                / 1000.0;
            Ok(json!({
                "device_code": state,
                "verification_uri": auth_url,
                "user_code": "",
                "interval": interval,
            }))
        }
        _ => Err(format!(
            "Provider {provider} does not support device code flow"
        )),
    }
}

/// The outcome of `pollForToken`.
pub struct PollResult {
    pub success: bool,
    pub tokens: Option<Value>,
    pub error: Option<String>,
    pub error_description: Option<String>,
    pub pending: bool,
}

/// `pollForToken(providerName, deviceCode, codeVerifier, extraData)`.
pub async fn poll_for_token(
    provider: &str,
    device_code: &str,
    _code_verifier: Option<&str>,
    _extra_data: Option<&Value>,
) -> Result<PollResult, String> {
    let cfg = oauth_config(provider)?;
    let (ok, data) = match provider {
        "grok-cli" => {
            let ua = "grok-pager/0.2.93 grok-shell/0.2.93 (linux; x86_64)";
            let body = encode_form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", cfg_str(&cfg, "clientId").unwrap_or_default()),
            ]);
            let res = post_form(
                cfg_str(&cfg, "tokenUrl").unwrap_or_default(),
                &body,
                &[("Accept", "application/json"), ("User-Agent", ua)],
            )
            .await?;
            let data = serde_json::from_str::<Value>(&res.body).unwrap_or_else(
                |_| json!({"error": "invalid_response", "error_description": res.body}),
            );
            let pending = matches!(
                data.get("error").and_then(Value::as_str),
                Some("authorization_pending") | Some("slow_down")
            );
            ((200..300).contains(&res.status) || pending, data)
        }
        "kilocode" => {
            let url = format!(
                "{}/{}",
                cfg_str(&cfg, "pollUrlBase").unwrap_or_default(),
                device_code
            );
            let res = get_raw(&url, &[]).await?;
            match res.status {
                202 => (false, json!({"error": "authorization_pending"})),
                403 => (
                    false,
                    json!({"error": "access_denied", "error_description": "Authorization denied by user"}),
                ),
                410 => (
                    false,
                    json!({"error": "expired_token", "error_description": "Authorization code expired"}),
                ),
                s if !(200..300).contains(&s) => (
                    false,
                    json!({"error": "poll_failed", "error_description": format!("Poll failed: {s}")}),
                ),
                _ => {
                    let data: Value = serde_json::from_str(&res.body).map_err(|e| e.to_string())?;
                    if data.get("status").and_then(Value::as_str) == Some("approved")
                        && let Some(token) = data.get("token").and_then(Value::as_str)
                    {
                        let org_id = kilocode_org_id(&cfg, token).await;
                        (
                            true,
                            json!({
                                "access_token": token,
                                "_userEmail": data.get("userEmail").cloned().unwrap_or(Value::Null),
                                "_orgId": org_id,
                            }),
                        )
                    } else {
                        (false, json!({"error": "authorization_pending"}))
                    }
                }
            }
        }
        "codebuddy-intl" => {
            let url = format!(
                "{}?state={}",
                cfg_str(&cfg, "tokenUrl").unwrap_or_default(),
                url::form_urlencoded::byte_serialize(device_code.as_bytes()).collect::<String>()
            );
            let ua = cfg_str(&cfg, "userAgent").unwrap_or_default();
            let res = get_raw(
                &url,
                &[
                    ("Accept", "application/json"),
                    ("User-Agent", ua),
                    ("X-Requested-With", "XMLHttpRequest"),
                    ("X-Domain", "www.codebuddy.ai"),
                    ("X-No-Authorization", "true"),
                    ("X-No-User-Id", "true"),
                    ("X-No-Enterprise-Id", "true"),
                    ("X-No-Department-Info", "true"),
                    ("X-Product", "SaaS"),
                ],
            )
            .await?;
            if !(200..300).contains(&res.status) {
                return Ok(PollResult {
                    success: false,
                    tokens: None,
                    error: Some("request_failed".into()),
                    error_description: None,
                    pending: false,
                });
            }
            let data: Value = serde_json::from_str(&res.body).map_err(|e| e.to_string())?;
            match data.get("code").and_then(Value::as_i64) {
                Some(0) => {
                    let inner = data.get("data").cloned().unwrap_or_else(|| json!({}));
                    if let Some(access) = inner.get("accessToken").and_then(Value::as_str) {
                        (
                            true,
                            json!({
                                "access_token": access,
                                "refresh_token": inner.get("refreshToken").and_then(Value::as_str).unwrap_or(""),
                                "token_type": inner.get("tokenType").and_then(Value::as_str).unwrap_or("Bearer"),
                                "expires_in": inner.get("expiresIn").cloned().unwrap_or(Value::Null),
                            }),
                        )
                    } else {
                        (false, json!({"error": "unknown_error"}))
                    }
                }
                Some(11217) => (true, json!({"error": "authorization_pending"})),
                _ => (
                    false,
                    json!({"error": data.get("msg").and_then(Value::as_str).unwrap_or("unknown_error")}),
                ),
            }
        }
        _ => {
            return Err(format!(
                "Provider {provider} does not support device code flow"
            ));
        }
    };

    // The outer `pollForToken`: a 2xx without an access token is only a success
    // when it is an explicit pending/slow_down; anything else is an error.
    if ok {
        if let Some(access) = data.get("access_token").and_then(Value::as_str) {
            let _ = access;
            let mapped = match provider {
                "grok-cli" => grok_cli_map(&data),
                "kilocode" => json!({
                    "accessToken": data.get("access_token").cloned().unwrap_or(Value::Null),
                    "refreshToken": Value::Null,
                    "expiresIn": Value::Null,
                    "email": data.get("_userEmail").cloned().unwrap_or(Value::Null),
                    "providerSpecificData": if data.get("_orgId").map(|v| !v.is_null()).unwrap_or(false) {
                        json!({ "orgId": data.get("_orgId").cloned().unwrap_or(Value::Null) })
                    } else {
                        Value::Null
                    },
                }),
                "codebuddy-intl" => json!({
                    "accessToken": data.get("access_token").cloned().unwrap_or(Value::Null),
                    "refreshToken": data.get("refresh_token").cloned().unwrap_or(Value::Null),
                    "expiresIn": data.get("expires_in").and_then(Value::as_i64).unwrap_or(86400),
                    "providerSpecificData": {},
                }),
                _ => data.clone(),
            };
            return Ok(PollResult {
                success: true,
                tokens: Some(mapped),
                error: None,
                error_description: None,
                pending: false,
            });
        }
        let err = data
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("no_access_token");
        if err == "authorization_pending" || err == "slow_down" {
            return Ok(PollResult {
                success: false,
                tokens: None,
                error: Some(err.to_string()),
                error_description: data
                    .get("error_description")
                    .or_else(|| data.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                pending: err == "authorization_pending",
            });
        }
        return Ok(PollResult {
            success: false,
            tokens: None,
            error: Some(err.to_string()),
            error_description: Some(
                data.get("error_description")
                    .or_else(|| data.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("No access token received")
                    .to_string(),
            ),
            pending: false,
        });
    }

    Ok(PollResult {
        success: false,
        tokens: None,
        error: data
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
        error_description: data
            .get("error_description")
            .and_then(Value::as_str)
            .map(str::to_string),
        pending: false,
    })
}

/// `grok-cli.mapTokens` for the device-code path.
fn grok_cli_map(tokens: &Value) -> Value {
    let id_token = tokens.get("id_token").and_then(Value::as_str);
    let access = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let email = decode_xai_id_token_email(id_token.unwrap_or_default())
        .or_else(|| extract_email_from_access_token(access));
    let expires_at = tokens.get("expires_in").and_then(Value::as_i64).map(|s| {
        let ms = chrono::Utc::now().timestamp_millis() + s * 1000;
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default()
    });
    let mut out = Map::new();
    out.insert(
        "accessToken".into(),
        tokens.get("access_token").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "refreshToken".into(),
        tokens.get("refresh_token").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "expiresIn".into(),
        tokens.get("expires_in").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "expiresAt".into(),
        expires_at.map_or(Value::Null, Value::String),
    );
    out.insert(
        "scope".into(),
        tokens.get("scope").cloned().unwrap_or(Value::Null),
    );
    if let Some(email) = &email {
        out.insert("email".into(), Value::String(email.clone()));
    }
    out.insert(
        "providerSpecificData".into(),
        json!({
            "authMethod": "device_code",
            "idToken": tokens.get("id_token").cloned().unwrap_or(Value::Null),
            "email": email,
            "userId": Value::Null,
            "hasGrokCodeAccess": Value::Null,
            "subscriptionTier": Value::Null,
        }),
    );
    Value::Object(out)
}

async fn kilocode_org_id(cfg: &Value, token: &str) -> Value {
    let url = format!(
        "{}/api/profile",
        cfg_str(cfg, "apiBaseUrl").unwrap_or_default()
    );
    match get_json_with_bearer(&url, token).await {
        Ok(profile) => profile
            .get("organizations")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|o| o.get("id"))
            .cloned()
            .unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// `extractCodexAccountInfo(idToken)`.
pub fn extract_codex_account_info(id_token: &str) -> Value {
    let Some(payload) = decode_jwt_payload(id_token) else {
        return json!({});
    };
    let chatgpt = payload
        .get("https://api.openai.com/auth")
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "email": payload.get("email").cloned().unwrap_or(Value::Null),
        "chatgptAccountId": chatgpt.get("chatgpt_account_id").cloned()
            .or_else(|| payload.get("account_id").cloned())
            .unwrap_or(Value::Null),
        "chatgptPlanType": chatgpt.get("chatgpt_plan_type").cloned()
            .or_else(|| payload.get("plan_type").cloned())
            .unwrap_or(Value::Null),
    })
}

/// `decodeJwtPayload(jwt)` — the middle segment, base64url-decoded.
pub fn decode_jwt_payload(jwt: &str) -> Option<Value> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let base64 = parts[1].replace('-', "+").replace('_', "/");
    let padding = (4 - (base64.len() % 4)) % 4;
    let padded = format!("{base64}{}", "=".repeat(padding));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `decodeXaiIdTokenEmail(idToken)`.
pub fn decode_xai_id_token_email(id_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(id_token)?;
    payload
        .get("email")
        .or_else(|| payload.get("preferred_username"))
        .or_else(|| payload.get("sub"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `extractEmailFromAccessToken(accessToken)`.
pub fn extract_email_from_access_token(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    payload
        .get("email")
        .or_else(|| payload.get("preferred_username"))
        .or_else(|| payload.get("sub"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn now_iso() -> String {
    router_db::time::now_iso()
}

// ── HTTP plumbing ──────────────────────────────────────────────────────────
//
// Every call goes through `prepare_send` so the outbound proxy setting and the
// MITM bypass apply the same way they do for chat traffic.

struct HttpResult {
    status: u16,
    body: String,
}

async fn send(
    method: reqwest::Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<(String, &str)>,
) -> Result<HttpResult, String> {
    let target = prepare_send(url, &ProxyOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    let mut req = target.client.request(method, &target.url);
    for (k, v) in target.extra_headers.iter() {
        req = req.header(k.as_str(), v.as_str());
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some((payload, content_type)) = body {
        req = req.header("Content-Type", content_type).body(payload);
    }
    let res = req.send().await.map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let body = res.text().await.map_err(|e| e.to_string())?;
    Ok(HttpResult { status, body })
}

async fn post_raw_json(
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<HttpResult, String> {
    send(
        reqwest::Method::POST,
        url,
        headers,
        Some((body.to_string(), "application/json")),
    )
    .await
}

async fn post_form(url: &str, form: &str, headers: &[(&str, &str)]) -> Result<HttpResult, String> {
    send(
        reqwest::Method::POST,
        url,
        headers,
        Some((form.to_string(), "application/x-www-form-urlencoded")),
    )
    .await
}

async fn get_raw(url: &str, headers: &[(&str, &str)]) -> Result<HttpResult, String> {
    send(reqwest::Method::GET, url, headers, None).await
}

async fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<Value, String> {
    let res = get_raw(url, headers).await?;
    serde_json::from_str(&res.body).map_err(|e| e.to_string())
}

async fn get_json_with_bearer(url: &str, token: &str) -> Result<Value, String> {
    let auth = format!("Bearer {token}");
    get_json(url, &[("Authorization", auth.as_str())]).await
}

/// A 2xx gate that surfaces the upstream body as the error text.
fn parse_ok(res: &HttpResult, label: &str) -> Result<Value, String> {
    if !(200..300).contains(&res.status) {
        return Err(format!("{label}: {}", res.body));
    }
    serde_json::from_str(&res.body).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_oauth_provider_has_a_flow() {
        let reg = registry();
        for provider in ["codex", "grok-cli", "kilocode", "codebuddy-intl"] {
            assert!(reg.oauth(provider).is_some(), "{provider} config missing");
            assert!(flow_type(provider).is_some(), "{provider} flow missing");
        }
    }

    #[test]
    fn generate_auth_data_builds_a_pkce_url() {
        let meta = Map::new();
        let data =
            generate_auth_data("codex", "http://localhost:1455/auth/callback", &meta).unwrap();
        let url = data.auth_url.unwrap();
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("state={}", data.state)));
        assert_eq!(
            data.code_verifier.len(),
            43,
            "32 bytes base64url is 43 chars"
        );
    }

    #[test]
    fn device_flow_has_no_auth_url() {
        let data = generate_auth_data("grok-cli", "", &Map::new()).unwrap();
        assert!(data.auth_url.is_none());
        assert_eq!(data.flow_type, "device_code");
    }

    #[test]
    fn codex_carries_its_fixed_port_and_callback_path() {
        let data = generate_auth_data("codex", "http://localhost:1455/auth/callback", &Map::new())
            .unwrap();
        assert_eq!(data.fixed_port, Some(1455));
        assert_eq!(data.callback_path, "/auth/callback");
        let json = data.to_json();
        assert_eq!(json.get("fixedPort").and_then(Value::as_u64), Some(1455));
    }

    #[test]
    fn jwt_payload_decode_reads_codex_claims() {
        // {"email":"a@b.c","https://api.openai.com/auth":{"chatgpt_account_id":"ws_1","chatgpt_plan_type":"plus"}}
        let payload = "eyJlbWFpbCI6ImFAYi5jIiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfYWNjb3VudF9pZCI6IndzXzEiLCJjaGF0Z3B0X3BsYW5fdHlwZSI6InBsdXMifX0";
        let jwt = format!("x.{payload}.y");
        let info = extract_codex_account_info(&jwt);
        assert_eq!(info.get("email").and_then(Value::as_str), Some("a@b.c"));
        assert_eq!(
            info.get("chatgptAccountId").and_then(Value::as_str),
            Some("ws_1")
        );
        assert_eq!(
            info.get("chatgptPlanType").and_then(Value::as_str),
            Some("plus")
        );
    }
}
