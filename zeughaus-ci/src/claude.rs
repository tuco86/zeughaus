//! The Claude subscription the CI signs in to, keeps renewed and hands out
//! through OpenBao, in place of a login per machine (`[claude]` of
//! `ci.toml`).
//!
//! The sign-in is Claude Code's own OAuth client (authorization code with
//! PKCE), whose only redirect is `localhost`: `zeughaus-runner claude
//! login` on the owner's machine asks the CI to start one
//! (`CiRequest::ClaudeStart`; the CI keeps state and verifier), opens the
//! browser at claude.ai, takes the redirect on `localhost:54545` and hands
//! code and state back (`CiRequest::ClaudeFinish`) over the same `/ci`
//! link editors use. The CI exchanges them, checks the account, and keeps
//! the refresh token in its own OpenBao document. Every refresh hands out
//! a new refresh token, which is written back at once; the grant itself
//! ends about 30 days after the sign-in, after which the owner signs in
//! again.
//!
//! A thread renews the access token well before it expires and patches it
//! into the `publish` documents as `CLAUDE_ACCESS_TOKEN`, where omp on every
//! machine reads it (`get-secret`); the CI's own requests use it from
//! memory. The refresh token never leaves the CI's document.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zeughaus_link::{CLAUDE_CALLBACK, ClaudeView};

use crate::config::ClaudeConfig;
use crate::secrets::{SecretError, Secrets};

/// Claude Code's public OAuth client.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
/// Where the token answer names no account: the account of a token.
const BOOTSTRAP_URL: &str = "https://api.anthropic.com/api/claude_cli/bootstrap?entrypoint=cli";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code \
                      user:mcp_servers user:file_upload";
/// The beta a token request and an account lookup announce.
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// What Claude Code sends as its agent on a refresh.
const REFRESH_AGENT: &str = "anthropic-sdk-typescript/0.112.1 userOAuthProvider";

/// The first system block of every Messages request made with the token:
/// a subscription token is served to Claude Code only.
pub const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// The betas a Messages request with the token announces.
pub const MESSAGES_BETA: &str = "claude-code-20250219,oauth-2025-04-20";

/// The field the access token is published as.
pub const ACCESS_FIELD: &str = "CLAUDE_ACCESS_TOKEN";
/// The CI's own fields: the refresh token, the account, when it signed in.
const REFRESH_FIELD: &str = "CLAUDE_REFRESH_TOKEN";
const ACCOUNT_FIELD: &str = "CLAUDE_ACCOUNT";
const SIGNED_IN_FIELD: &str = "CLAUDE_SIGNED_IN";
/// The fields of the CI's own document the sign-in writes.
pub const OWN_FIELDS: [&str; 3] = [REFRESH_FIELD, ACCOUNT_FIELD, SIGNED_IN_FIELD];

/// How long a started sign-in waits for its code.
const PENDING_FOR: Duration = Duration::from_secs(600);
/// How long before it expires the access token is renewed, so a published
/// one is still good for hours when it is read.
const RENEW_BEFORE: u64 = 2 * 3600;
/// How long a sign-in lasts: the last refresh token stops working then.
pub const GRANT_SECS: u64 = 30 * 24 * 3600;
/// How long a token request may take, in seconds.
/// How long a token request may take, in seconds: a sign-in's exchange and
/// account lookup fit in the editor's 30 s for one question.
const TIMEOUT_SECS: u32 = 12;

/// The sign-in and its tokens.
pub struct Claude {
    config: ClaudeConfig,
    secrets: Arc<Secrets>,
    /// Started sign-ins: state to PKCE verifier.
    pending: Mutex<HashMap<String, (String, Instant)>>,
    /// Held across a refresh, so there is one at a time.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Whether the sign-in was read from OpenBao.
    loaded: bool,
    refresh: Option<String>,
    account: String,
    signed_in: u64,
    /// The access token and when it expires, in seconds since the epoch.
    access: Option<(String, u64)>,
    /// The sign-in has changed since it last reached OpenBao.
    unsaved: bool,
    /// The access token has changed since it last reached every `publish`
    /// document.
    unpublished: bool,
    error: Option<String>,
}

/// What the token endpoint answers.
#[derive(Debug, Deserialize)]
struct TokenAnswer {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    account: Option<Account>,
}

#[derive(Debug, Deserialize)]
struct Account {
    email_address: String,
}

impl Claude {
    pub fn new(config: ClaudeConfig, secrets: Arc<Secrets>) -> Arc<Claude> {
        Arc::new(Claude {
            config,
            secrets,
            pending: Mutex::new(HashMap::new()),
            state: Mutex::new(State::default()),
        })
    }

    /// Starts a sign-in: the address at claude.ai the browser goes to.
    pub fn start(&self) -> Result<String, String> {
        let verifier = random_token()?;
        let state = random_token()?;
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, (_, at)| at.elapsed() < PENDING_FOR);
        let url = authorize_url(&state, &challenge(&verifier));
        pending.insert(state, (verifier, Instant::now()));
        Ok(url)
    }

    /// Finishes a sign-in with the code and state the callback received.
    /// Answers the account.
    pub fn sign_in(&self, code: &str, state: &str, now: u64) -> Result<String, String> {
        let verifier = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.retain(|_, (_, at)| at.elapsed() < PENDING_FOR);
            let (verifier, _) = pending
                .remove(state)
                .ok_or("this sign-in expired or was used; start again")?;
            verifier
        };
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "redirect_uri": CLAUDE_CALLBACK,
            "code_verifier": verifier,
            "state": state,
        });
        let answer: TokenAnswer = post_token(&body.to_string(), &[])?;
        let account = match &answer.account {
            Some(account) => account.email_address.clone(),
            None => account_of(&answer.access_token)?,
        };
        if !account.eq_ignore_ascii_case(&self.config.account) {
            return Err(format!(
                "signed in as {account}, but [claude] accepts {} only; nothing was kept",
                self.config.account
            ));
        }
        let refresh = answer
            .refresh_token
            .ok_or("Anthropic answered no refresh token")?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = State {
            loaded: true,
            refresh: Some(refresh),
            account: account.clone(),
            signed_in: now,
            access: Some((answer.access_token, now + answer.expires_in)),
            unsaved: true,
            unpublished: true,
            error: None,
        };
        self.store(&mut state);
        eprintln!("[ci] claude: signed in as {account}");
        Ok(account)
    }

    /// An access token good for at least a minute, renewed first when it
    /// is not.
    pub fn access_token(&self, now: u64) -> Result<String, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.load(&mut state);
        if let Some((token, expires)) = &state.access
            && *expires > now + 60
        {
            return Ok(token.clone());
        }
        self.renew(&mut state, now)?;
        state
            .access
            .as_ref()
            .map(|(token, _)| token.clone())
            .ok_or_else(|| self.signed_out(&state))
    }

    /// The renewal thread's round: renews a token that expires within
    /// [`RENEW_BEFORE`], and retries what did not reach OpenBao.
    pub fn tick(&self, now: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.load(&mut state);
        if state.refresh.is_none() {
            return;
        }
        if state
            .access
            .as_ref()
            .is_none_or(|(_, expires)| *expires < now + RENEW_BEFORE)
        {
            if let Err(e) = self.renew(&mut state, now) {
                eprintln!("[ci] claude: {e}");
            }
        } else {
            self.store(&mut state);
        }
    }

    /// Where the sign-in stands, for the overview.
    pub fn status(&self) -> ClaudeView {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let signed_in = state.refresh.is_some();
        ClaudeView {
            account: signed_in.then(|| state.account.clone()),
            signed_in: state.signed_in,
            ends: if signed_in {
                state.signed_in + GRANT_SECS
            } else {
                0
            },
            expires: state.access.as_ref().map_or(0, |(_, expires)| *expires),
            error: state.error.clone(),
        }
    }

    /// Reads the sign-in from OpenBao once.
    fn load(&self, state: &mut State) {
        if state.loaded {
            return;
        }
        let field = |name: &str| match self.secrets.get(name) {
            Ok(value) => Ok(Some(value)),
            Err(SecretError::Absent(_)) => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        match field(REFRESH_FIELD) {
            Ok(refresh) => {
                state.loaded = true;
                state.refresh = refresh.filter(|r| !r.is_empty());
                state.account = field(ACCOUNT_FIELD).ok().flatten().unwrap_or_default();
                state.signed_in = field(SIGNED_IN_FIELD)
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
            Err(e) => state.error = Some(format!("the sign-in was not read: {e}")),
        }
    }

    /// Trades the refresh token for a new access token (and a new refresh
    /// token), and stores both. An `invalid_grant` ends the sign-in.
    fn renew(&self, state: &mut State, now: u64) -> Result<(), String> {
        let Some(refresh) = state.refresh.clone() else {
            return Err(self.signed_out(state));
        };
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": refresh,
        });
        let headers = [
            format!("anthropic-beta: {OAUTH_BETA}"),
            format!("User-Agent: {REFRESH_AGENT}"),
        ];
        match post_token(&body.to_string(), &headers) {
            Ok(answer) => {
                if let Some(rotated) = answer.refresh_token {
                    state.refresh = Some(rotated);
                    state.unsaved = true;
                }
                state.access = Some((answer.access_token, now + answer.expires_in));
                state.unpublished = true;
                state.error = None;
                self.store(state);
                Ok(())
            }
            Err(e) if e.contains("invalid_grant") => {
                state.refresh = None;
                state.access = None;
                state.unsaved = true;
                state.error = Some(format!("signed out ({e})"));
                self.store(state);
                Err(self.signed_out(state))
            }
            Err(e) => {
                state.error = Some(format!("renewing: {e}"));
                Err(format!("renewing the Claude token: {e}"))
            }
        }
    }

    /// Writes what changed to OpenBao: the sign-in to the CI's document,
    /// the access token to every `publish` document. What fails is tried
    /// again on the next round.
    fn store(&self, state: &mut State) {
        if state.unsaved {
            let signed_in = state.signed_in.to_string();
            let fields = [
                (REFRESH_FIELD, state.refresh.as_deref().unwrap_or("")),
                (ACCOUNT_FIELD, state.account.as_str()),
                (SIGNED_IN_FIELD, signed_in.as_str()),
            ];
            match self.secrets.patch(None, &fields) {
                Ok(()) => state.unsaved = false,
                Err(e) => {
                    eprintln!("[ci] claude: the sign-in did not reach OpenBao: {e}");
                    state.error = Some(format!("the sign-in did not reach OpenBao: {e}"));
                }
            }
        }
        if state.unpublished {
            let access = state
                .access
                .as_ref()
                .map_or("", |(token, _)| token.as_str());
            let mut all = true;
            for document in &self.config.publish {
                if let Err(e) = self
                    .secrets
                    .patch(Some(document), &[(ACCESS_FIELD, access)])
                {
                    eprintln!("[ci] claude: {document}: {e}");
                    state.error = Some(format!("{document}: {e}"));
                    all = false;
                }
            }
            state.unpublished = !all;
        }
    }

    fn signed_out(&self, state: &State) -> String {
        let why = state.error.as_deref().unwrap_or("not signed in");
        format!("Claude: {why}; sign in with `zeughaus-runner claude login`")
    }
}

/// 32 random bytes as base64url without padding.
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random bytes: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE S256 challenge of `verifier`.
fn challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The address at claude.ai that starts a sign-in.
fn authorize_url(state: &str, challenge: &str) -> String {
    let query = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", CLAUDE_CALLBACK),
        ("scope", SCOPES),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let query: Vec<String> = query
        .iter()
        .map(|(name, value)| format!("{name}={}", encode(value)))
        .collect();
    format!("{AUTHORIZE_URL}?{}", query.join("&"))
}

/// `text` percent-encoded for a query: unreserved characters stay.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// POSTs a JSON `body` to the token endpoint.
fn post_token(body: &str, extra: &[String]) -> Result<TokenAnswer, String> {
    let mut headers = vec!["Content-Type: application/json"];
    headers.extend(extra.iter().map(String::as_str));
    let mut command = Command::new("curl");
    command
        .args(["-sS", "--fail-with-body", "--max-time"])
        .arg(TIMEOUT_SECS.to_string())
        .args(["-K", "-", "-X", "POST", TOKEN_URL]);
    let answer = crate::forge::run(command, &crate::forge::curl_config(&headers, Some(body)))?;
    serde_json::from_str(&answer).map_err(|e| format!("the token answer: {e}"))
}

/// The email address of the account `access` belongs to.
fn account_of(access: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Bootstrap {
        oauth_account: OauthAccount,
    }
    #[derive(Deserialize)]
    struct OauthAccount {
        account_email: String,
    }
    let authorization = format!("Authorization: Bearer {access}");
    let beta = format!("anthropic-beta: {OAUTH_BETA}");
    let mut command = Command::new("curl");
    command
        .args(["-sS", "--fail-with-body", "--max-time"])
        .arg(TIMEOUT_SECS.to_string())
        .args(["-K", "-", BOOTSTRAP_URL]);
    let answer = crate::forge::run(
        command,
        &crate::forge::curl_config(&[&authorization, &beta], None),
    )?;
    serde_json::from_str::<Bootstrap>(&answer)
        .map(|b| b.oauth_account.account_email)
        .map_err(|e| format!("the account of the token: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::query_param;

    #[test]
    fn the_sign_in_asks_for_inference_with_a_s256_challenge() {
        // RFC 7636, appendix B.
        let challenge = challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let url = authorize_url("st", &challenge);
        let (base, query) = url.split_once('?').unwrap();
        assert_eq!(base, AUTHORIZE_URL);
        assert_eq!(query_param(query, "scope").as_deref(), Some(SCOPES));
        assert_eq!(
            query_param(query, "redirect_uri").as_deref(),
            Some(CLAUDE_CALLBACK)
        );
        assert_eq!(
            query_param(query, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(query_param(query, "code_challenge"), Some(challenge));
        assert_eq!(query_param(query, "state").as_deref(), Some("st"));
    }
}
