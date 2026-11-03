//! The CI's OpenBao access: its own document (forge tokens, webhook secrets,
//! the Claude sign-in) read when the values are used, and the secret
//! hand-off of a job.
//!
//! The CI logs in with its own AppRole, whose secret_id is sealed with
//! `systemd-creds` to this machine and the CI user, and keeps the token in
//! memory. A job is never handed a value or the CI's token: the CI mints a
//! job token through the token role `[secrets] job_role` that reads only the
//! job's project document and the global one, wraps it (single use, short
//! TTL) and hands the wrapping token to the runner, which redeems it, reads
//! the granted names and revokes it. The CI revokes it by its accessor once
//! the runner started, failed to start or finished the run. The runner, the
//! hook and the CLI each build their own [`Secrets`] against the same role.
//!
//! The own document is read as a whole and answered from memory for
//! `cache_ttl_seconds`, so a rotation in OpenBao takes effect within that
//! time without a restart. A failed read drops the cached document: a value
//! that OpenBao no longer vouches for is never served. Nothing is renewed; a
//! token the server no longer accepts is replaced by one new login.
//!
//! No error text and no `Debug` output carries a secret value, token or
//! secret_id.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use zeughaus_link::enlist::SecretHandoff;

use crate::config::{CiConfig, SecretsConfig};

/// How long a job's wrapping token can be redeemed.
const WRAP_TTL: &str = "120s";

/// How long a job token lives; its token role caps it too.
const JOB_TOKEN_TTL: &str = "10m";

/// The policy of the job tokens of repository `repo`.
pub fn job_policy_name(repo: &str) -> String {
    format!("zeughaus-job-{repo}")
}

/// The sealed secret_id, relative to the state directory.
pub const CRED_FILE: &str = "bao-secret-id.cred";

/// `systemd-creds --name`: the name a credential is sealed under is part of
/// what it decrypts with.
pub const CRED_NAME: &str = "bao-secret-id";

/// Why a secret could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretError {
    /// OpenBao answered and the secret is not there; asking again changes
    /// nothing until someone writes it.
    Absent(String),
    /// OpenBao or its credentials could not be reached or used; a later
    /// attempt may succeed.
    Unavailable(String),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::Absent(text) | SecretError::Unavailable(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for SecretError {}

/// Where the AppRole's secret_id comes from.
enum SecretId {
    /// A credential file sealed by `deploy/ci/openbao.sh`.
    Sealed(PathBuf),
    #[cfg(test)]
    Fixed(String),
}

struct Document {
    read_at: Instant,
    fields: Map<String, Value>,
}

#[derive(Default)]
struct State {
    token: Option<String>,
    document: Option<Document>,
}

/// The one OpenBao document behind a `ci.toml`, read lazily and cached.
pub struct Secrets {
    config: Option<SecretsConfig>,
    secret_id: SecretId,
    /// Held for a whole `get` or `fields`, so concurrent callers share one
    /// login and one read instead of racing each other's.
    state: Mutex<State>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secrets")
            .field("document", &self.document())
            .finish_non_exhaustive()
    }
}

impl Secrets {
    /// `config` is `ci.toml`'s `[secrets]`; without it every read is `Absent`.
    pub fn new(config: Option<SecretsConfig>, state_dir: &Path) -> Arc<Secrets> {
        Arc::new(Secrets {
            config,
            secret_id: SecretId::Sealed(state_dir.join(CRED_FILE)),
            state: Mutex::new(State::default()),
        })
    }

    /// Tests present a fixed secret_id instead of unsealing one.
    #[cfg(test)]
    pub fn with_secret_id(config: SecretsConfig, secret_id: String) -> Arc<Secrets> {
        Arc::new(Secrets {
            config: Some(config),
            secret_id: SecretId::Fixed(secret_id),
            state: Mutex::new(State::default()),
        })
    }

    /// The value of the secret `name`: one line, without its line end.
    pub fn get(&self, name: &str) -> Result<String, SecretError> {
        let what = format!("{name} not read");
        self.with_document(&what, |config, fields| value_of(config, name, fields))
    }

    /// The names of every field of the document.
    pub fn fields(&self) -> Result<BTreeSet<String>, SecretError> {
        self.with_document("document not read", |_, fields| {
            Ok(fields.keys().cloned().collect())
        })
    }

    /// The document for messages: `secret/zeughaus/ci`.
    pub fn document(&self) -> String {
        match &self.config {
            Some(config) => format!("{}/{}", config.mount, config.path),
            None => "(no [secrets])".to_owned(),
        }
    }

    /// Patches `fields` into `document` (a path below the `[secrets]` mount,
    /// the CI's own document without one) by a KV v2 merge patch, which
    /// leaves every other field as it is. The cached document is dropped,
    /// so the next read sees the fields.
    pub fn patch(
        &self,
        document: Option<&str>,
        fields: &[(&str, &str)],
    ) -> Result<(), SecretError> {
        let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
        let what = format!("{} not stored", names.join(", "));
        let Some(config) = &self.config else {
            return Err(SecretError::Absent(format!(
                "no [secrets] in ci.toml; {what}"
            )));
        };
        let path = document.unwrap_or(&config.path);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let url = format!("{}/v1/{}/data/{path}", config.addr, config.mount);
        let data: Map<String, Value> = fields
            .iter()
            .map(|(name, value)| ((*name).to_owned(), Value::from(*value)))
            .collect();
        let body = json!({ "data": data }).to_string();
        let mut retried = false;
        loop {
            let token = match &state.token {
                Some(token) => token.clone(),
                None => {
                    let token = self.login(config, &what)?;
                    state.token = Some(token.clone());
                    token
                }
            };
            // Token and value go over stdin as curl config, never in argv.
            let stdin = crate::forge::curl_config(
                &[
                    &format!("X-Vault-Token: {token}"),
                    "Content-Type: application/merge-patch+json",
                ],
                Some(&body),
            );
            let (code, _) = bao_curl(
                &["-X", "PATCH", "-K", "-"],
                &stdin,
                &url,
                config.timeout_seconds,
            )
            .map_err(|e| unreachable_error(config, &e, &what))?;
            match code {
                200 | 204 => {
                    state.document = None;
                    return Ok(());
                }
                403 => {
                    state.token = None;
                    if retried {
                        return Err(SecretError::Unavailable(format!(
                            "openbao: {}/{path} denied the patch (policy zeughaus needs patch; rerun deploy/ci/openbao.sh)",
                            config.mount
                        )));
                    }
                    retried = true;
                }
                404 => {
                    return Err(SecretError::Absent(format!(
                        "openbao: no document {}/{path}",
                        config.mount
                    )));
                }
                503 => return Err(sealed_error(config)),
                other => {
                    return Err(SecretError::Unavailable(format!(
                        "openbao {}: HTTP {other} on patch",
                        config.addr
                    )));
                }
            }
        }
    }

    /// Runs `use_document` on the document, read first when the cached one is
    /// older than `cache_ttl_seconds`. `what` ends the message of an error that
    /// means a value could not be had.
    fn with_document<T>(
        &self,
        what: &str,
        use_document: impl FnOnce(&SecretsConfig, &Map<String, Value>) -> Result<T, SecretError>,
    ) -> Result<T, SecretError> {
        let Some(config) = &self.config else {
            return Err(SecretError::Absent(format!(
                "no [secrets] in ci.toml; {what}"
            )));
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ttl = Duration::from_secs(config.cache_ttl_seconds);
        if let Some(document) = state
            .document
            .as_ref()
            .filter(|document| document.read_at.elapsed() < ttl)
        {
            return use_document(config, &document.fields);
        }
        state.document = None;
        let fields = self.read(config, &mut state, what)?;
        let result = use_document(config, &fields);
        state.document = Some(Document {
            read_at: Instant::now(),
            fields,
        });
        result
    }

    /// Reads the document, logging in first without a token. A 403 means the
    /// token is gone or too old: one new login and one retry, then the answer
    /// stands.
    fn read(
        &self,
        config: &SecretsConfig,
        state: &mut State,
        what: &str,
    ) -> Result<Map<String, Value>, SecretError> {
        let url = format!("{}/v1/{}/data/{}", config.addr, config.mount, config.path);
        let mut retried = false;
        loop {
            let token = match &state.token {
                Some(token) => token.clone(),
                None => {
                    let token = self.login(config, what)?;
                    state.token = Some(token.clone());
                    token
                }
            };
            // The token goes over stdin: argv is readable by every local user.
            let header = format!("X-Vault-Token: {token}\n");
            let (code, body) = bao_curl(&["-H", "@-"], &header, &url, config.timeout_seconds)
                .map_err(|e| unreachable_error(config, &e, what))?;
            match code {
                200 => return parse_document(config, &body),
                403 => {
                    state.token = None;
                    if retried {
                        return Err(SecretError::Unavailable(format!(
                            "openbao: {}/{} denied (policy zeughaus)",
                            config.mount, config.path
                        )));
                    }
                    retried = true;
                }
                404 => {
                    return Err(SecretError::Absent(format!(
                        "openbao: no document {}/{}",
                        config.mount, config.path
                    )));
                }
                503 => return Err(sealed_error(config)),
                other => {
                    return Err(SecretError::Unavailable(format!(
                        "openbao {}: HTTP {other} on {}/{}",
                        config.addr, config.mount, config.path
                    )));
                }
            }
        }
    }

    fn login(&self, config: &SecretsConfig, what: &str) -> Result<String, SecretError> {
        let secret_id = self.secret_id()?;
        let body = json!({ "role_id": config.role_id, "secret_id": secret_id }).to_string();
        let url = format!("{}/v1/auth/approle/login", config.addr);
        let (code, answer) = bao_curl(
            &[
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                "@-",
            ],
            &body,
            &url,
            config.timeout_seconds,
        )
        .map_err(|e| unreachable_error(config, &e, what))?;
        match code {
            200 => {}
            400 | 403 => {
                return Err(SecretError::Unavailable(
                    "openbao login rejected (secret_id revoked or expired?)".to_owned(),
                ));
            }
            503 => return Err(sealed_error(config)),
            other => {
                return Err(SecretError::Unavailable(format!(
                    "openbao {}: HTTP {other} on login",
                    config.addr
                )));
            }
        }
        serde_json::from_str::<Value>(&answer)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/auth/client_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                SecretError::Unavailable(format!(
                    "openbao {}: the login answer has no client_token",
                    config.addr
                ))
            })
    }

    fn secret_id(&self) -> Result<String, SecretError> {
        match &self.secret_id {
            #[cfg(test)]
            SecretId::Fixed(secret_id) => Ok(secret_id.clone()),
            SecretId::Sealed(path) => unseal(path),
        }
    }

    /// Mints a job token for `repo` (`origin` its [`normalize_origin`]) that
    /// reads the project document and the global one, wrapped for the
    /// runner of `run`. Answers the hand-off and the accessor of the job
    /// token, which [`Secrets::revoke_accessor`] takes.
    ///
    /// [`normalize_origin`]: zeughaus_link::normalize_origin
    pub fn job_handoff(
        &self,
        repo: &str,
        origin: &str,
        names: &[String],
        run: &str,
    ) -> Result<(SecretHandoff, String), SecretError> {
        let what = format!("{} not handed to {run}", names.join(", "));
        let Some(config) = &self.config else {
            return Err(SecretError::Absent(format!(
                "no [secrets] in ci.toml; {what}"
            )));
        };
        let url = format!("{}/v1/auth/token/create/{}", config.addr, config.job_role);
        let body = json!({
            "policies": [job_policy_name(repo)],
            "meta": { "run": run, "repo": repo },
            "display_name": "job",
            "ttl": JOB_TOKEN_TTL,
        })
        .to_string();
        let wrap = format!("X-Vault-Wrap-TTL: {WRAP_TTL}");
        let (code, answer) = self.authed(
            config,
            "POST",
            &url,
            &[&wrap, "Content-Type: application/json"],
            Some(&body),
            &what,
        )?;
        match code {
            200 => {}
            403 => {
                return Err(SecretError::Unavailable(format!(
                    "openbao: the CI may not mint job tokens (policy zeughaus needs update on auth/token/create/{}; rerun deploy/ci/openbao.sh)",
                    config.job_role
                )));
            }
            400 => {
                return Err(SecretError::Unavailable(format!(
                    "openbao: job token for {repo} refused: {}",
                    api_errors(&answer)
                )));
            }
            503 => return Err(sealed_error(config)),
            other => {
                return Err(SecretError::Unavailable(format!(
                    "openbao {}: HTTP {other} on the job token",
                    config.addr
                )));
            }
        }
        let answer: Value = serde_json::from_str(&answer).unwrap_or(Value::Null);
        let field = |pointer: &str| {
            answer
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    SecretError::Unavailable(format!(
                        "openbao {}: the job token answer has no {pointer}",
                        config.addr
                    ))
                })
        };
        let wrapping_token = field("/wrap_info/token")?;
        let accessor = field("/wrap_info/wrapped_accessor")?;
        let handoff = SecretHandoff {
            addr: config.addr.clone(),
            mount: config.mount.clone(),
            project: config.project_shell(origin),
            global: config.global_shell(),
            names: names.to_vec(),
            wrapping_token,
        };
        Ok((handoff, accessor))
    }

    /// Revokes the token behind `accessor`; one that is already gone counts
    /// as revoked.
    pub fn revoke_accessor(&self, accessor: &str) -> Result<(), SecretError> {
        let what = "job token not revoked";
        let Some(config) = &self.config else {
            return Err(SecretError::Absent(format!(
                "no [secrets] in ci.toml; {what}"
            )));
        };
        let url = format!("{}/v1/auth/token/revoke-accessor", config.addr);
        let body = json!({ "accessor": accessor }).to_string();
        let (code, answer) = self.authed(
            config,
            "POST",
            &url,
            &["Content-Type: application/json"],
            Some(&body),
            what,
        )?;
        match code {
            200 | 204 | 400 => Ok(()),
            503 => Err(sealed_error(config)),
            other => Err(SecretError::Unavailable(format!(
                "openbao {}: HTTP {other} on revoke-accessor: {}",
                config.addr,
                api_errors(&answer)
            ))),
        }
    }

    /// Whether each of `names` is a field of `repo`'s project document or
    /// the global one, read the way a job reads them: through a job token,
    /// which is revoked again. Values are never kept.
    pub fn project_fields(
        &self,
        repo: &str,
        origin: &str,
        names: &[String],
    ) -> Result<BTreeMap<String, bool>, SecretError> {
        let Some(config) = &self.config else {
            return Err(SecretError::Absent(format!(
                "no [secrets] in ci.toml; {} not checked",
                names.join(", ")
            )));
        };
        let (handoff, accessor) = self.job_handoff(repo, origin, names, "secrets-check")?;
        let result = read_with_handoff(config, &handoff, repo);
        // revoke-self ran unless the read failed early; the accessor covers
        // that case and is harmless after it.
        let _ = self.revoke_accessor(&accessor);
        let present = result?;
        Ok(names
            .iter()
            .map(|name| (name.clone(), present.contains(name)))
            .collect())
    }

    /// A request with the CI's own token, logging in first without one. A
    /// 403 means the token is gone or too old: one new login and one retry,
    /// then the answer stands and is returned as it is.
    fn authed(
        &self,
        config: &SecretsConfig,
        method: &str,
        url: &str,
        headers: &[&str],
        body: Option<&str>,
        what: &str,
    ) -> Result<(u16, String), SecretError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut retried = false;
        loop {
            let token = match &state.token {
                Some(token) => token.clone(),
                None => {
                    let token = self.login(config, what)?;
                    state.token = Some(token.clone());
                    token
                }
            };
            let (code, answer) = bao_call(config, method, url, &token, headers, body, what)?;
            if code == 403 {
                state.token = None;
                if !retried {
                    retried = true;
                    continue;
                }
            }
            return Ok((code, answer));
        }
    }
}

/// One request with `token` and `headers`, which go over stdin as curl
/// config together with the body, never in argv.
fn bao_call(
    config: &SecretsConfig,
    method: &str,
    url: &str,
    token: &str,
    headers: &[&str],
    body: Option<&str>,
    what: &str,
) -> Result<(u16, String), SecretError> {
    let auth = format!("X-Vault-Token: {token}");
    let mut all = Vec::with_capacity(headers.len() + 1);
    all.push(auth.as_str());
    all.extend_from_slice(headers);
    let stdin = crate::forge::curl_config(&all, body);
    bao_curl(
        &["-X", method, "-K", "-"],
        &stdin,
        url,
        config.timeout_seconds,
    )
    .map_err(|e| unreachable_error(config, &e, what))
}

/// Redeems `handoff` of `repo`, reads the field names of both documents with
/// the job token and revokes it. A document that does not exist has no fields.
fn read_with_handoff(
    config: &SecretsConfig,
    handoff: &SecretHandoff,
    repo: &str,
) -> Result<BTreeSet<String>, SecretError> {
    let what = "project secrets not checked";
    let json = ["Content-Type: application/json"];
    let unwrap = format!("{}/v1/sys/wrapping/unwrap", config.addr);
    let (code, answer) = bao_call(
        config,
        "POST",
        &unwrap,
        &handoff.wrapping_token,
        &json,
        Some("{}"),
        what,
    )?;
    if code != 200 {
        return Err(SecretError::Unavailable(format!(
            "openbao {}: HTTP {code} on unwrap: {}",
            config.addr,
            api_errors(&answer)
        )));
    }
    let token = serde_json::from_str::<Value>(&answer)
        .ok()
        .and_then(|value| {
            value
                .pointer("/auth/client_token")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            SecretError::Unavailable(format!(
                "openbao {}: the unwrapped answer has no client_token",
                config.addr
            ))
        })?;
    let mut fields = BTreeSet::new();
    let mut failure = None;
    for document in [&handoff.project, &handoff.global] {
        let url = format!("{}/v1/{}/data/{document}", config.addr, config.mount);
        match bao_call(config, "GET", &url, &token, &[], None, what) {
            Ok((200, body)) => {
                let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                if let Some(Value::Object(data)) = value.pointer("/data/data") {
                    fields.extend(data.keys().cloned());
                }
            }
            Ok((404, _)) => {}
            Ok((403, _)) => {
                failure = Some(SecretError::Unavailable(format!(
                    "openbao: the job token may not read {}/{document} (policy {} missing? run deploy/ci/openbao.sh on sadala)",
                    config.mount,
                    job_policy_name(repo)
                )));
                break;
            }
            Ok((code, body)) => {
                failure = Some(SecretError::Unavailable(format!(
                    "openbao {}: HTTP {code} on {}/{document}: {}",
                    config.addr,
                    config.mount,
                    api_errors(&body)
                )));
                break;
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    let revoke = format!("{}/v1/auth/token/revoke-self", config.addr);
    let _ = bao_call(config, "POST", &revoke, &token, &json, Some("{}"), what);
    match failure {
        Some(e) => Err(e),
        None => Ok(fields),
    }
}

/// OpenBao's `errors` of an error answer, joined; never a value.
fn api_errors(body: &str) -> String {
    let errors: Vec<String> = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value.get("errors").and_then(Value::as_array).map(|errors| {
                errors
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
        })
        .unwrap_or_default();
    if errors.is_empty() {
        "no reason given".to_owned()
    } else {
        errors.join("; ")
    }
}

/// `zeughaus-ci job-policies`: the policy of each repository's job tokens,
/// for every repository with at least one grant. Each reads the repository's
/// project document and the global one.
pub fn job_policies(config: &CiConfig) -> Result<BTreeMap<String, String>, String> {
    let Some(secrets) = &config.secrets else {
        return Err("no [secrets] in ci.toml".to_owned());
    };
    Ok(config
        .repos
        .iter()
        .filter(|(_, repo)| !repo.grants.is_empty())
        .map(|(name, repo)| {
            let origin = zeughaus_link::normalize_origin(&repo.url);
            let rule = |document: String| {
                format!(
                    "path \"{}/data/{document}\" {{\n  capabilities = [\"read\"]\n}}\n",
                    secrets.mount
                )
            };
            let policy = format!(
                "{}\n{}",
                rule(secrets.project_shell(&origin)),
                rule(secrets.global_shell())
            );
            (job_policy_name(name), policy)
        })
        .collect())
}

/// `systemd-creds decrypt` of the sealed secret_id; only the user it was
/// sealed for, on the machine it was sealed on, can do this.
fn unseal(path: &Path) -> Result<String, SecretError> {
    let fail = |detail: &str| {
        SecretError::Unavailable(format!(
            "cannot decrypt {}: {detail} (deploy/ci/openbao.sh seals it)",
            path.display()
        ))
    };
    let output = Command::new("systemd-creds")
        .args(["decrypt", "--user"])
        .arg(format!("--name={CRED_NAME}"))
        .arg(path)
        .arg("-")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| fail(&format!("cannot run systemd-creds: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(fail(stderr.trim()));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| fail("the secret_id is not UTF-8"))?;
    let secret_id = text.trim_end_matches(['\n', '\r']);
    if secret_id.is_empty() {
        return Err(fail("the secret_id is empty"));
    }
    Ok(secret_id.to_owned())
}

fn unreachable_error(config: &SecretsConfig, stderr: &str, what: &str) -> SecretError {
    SecretError::Unavailable(format!(
        "openbao {} unreachable ({stderr}); {what}",
        config.addr
    ))
}

fn sealed_error(config: &SecretsConfig) -> SecretError {
    SecretError::Unavailable(format!("openbao {} is sealed", config.addr))
}

/// The KV v2 answer's `.data.data`.
fn parse_document(config: &SecretsConfig, body: &str) -> Result<Map<String, Value>, SecretError> {
    let mut value: Value = serde_json::from_str(body).map_err(|_| {
        SecretError::Unavailable(format!(
            "openbao {}: unexpected answer for {}/{}",
            config.addr, config.mount, config.path
        ))
    })?;
    match value.pointer_mut("/data/data") {
        Some(Value::Object(fields)) => Ok(std::mem::take(fields)),
        _ => Err(SecretError::Unavailable(format!(
            "openbao {}: the answer for {}/{} has no data",
            config.addr, config.mount, config.path
        ))),
    }
}

fn value_of(
    config: &SecretsConfig,
    name: &str,
    fields: &Map<String, Value>,
) -> Result<String, SecretError> {
    let document = format!("{}/{}", config.mount, config.path);
    let Some(value) = fields.get(name) else {
        return Err(SecretError::Absent(format!(
            "openbao: no field {name} in {document}"
        )));
    };
    let Some(text) = value.as_str() else {
        return Err(SecretError::Absent(format!(
            "openbao: {name} in {document} is not a string"
        )));
    };
    // A header or a URL built from a value with a line break would be
    // injected into; refuse it where it enters.
    let Some(text) = zeughaus_link::enlist::secret_line(text) else {
        return Err(SecretError::Absent(format!(
            "openbao: {name} in {document} spans several lines"
        )));
    };
    Ok(text.to_owned())
}

/// Runs `curl -sS --max-time <timeout> -w '\n%{http_code}' <args> <url>` with
/// `stdin` piped in, and returns the status code and the body. `Err` is curl's
/// stderr when it exits non-zero, which means no HTTP answer came.
fn bao_curl(args: &[&str], stdin: &str, url: &str, timeout: u64) -> Result<(u16, String), String> {
    let mut child = Command::new("curl")
        .args(["-sS", "--max-time"])
        .arg(timeout.to_string())
        .args(["-w", "\n%{http_code}"])
        .args(args)
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        // A curl that failed before reading its stdin closes the pipe; its exit
        // status says why.
        let _ = pipe.write_all(stdin.as_bytes());
    }
    let output = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(if stderr.is_empty() {
            format!("curl {}", output.status)
        } else {
            stderr.to_owned()
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, code) = stdout
        .rsplit_once('\n')
        .ok_or_else(|| "curl printed no status code".to_owned())?;
    let code = code
        .trim()
        .parse::<u16>()
        .map_err(|_| "curl printed no status code".to_owned())?;
    Ok((code, body.to_owned()))
}

/// `ci secrets-check`: whether every secret `config` names can be had. The
/// CI's own names (fetch and status tokens, webhook secrets, the onboarding
/// tokens) are checked against its document as `NAME ok|missing` in name
/// order; each repository's grants against its project documents, read
/// through a job token as a job reads them, as `<repo>: NAME ok|missing`.
/// Then `unreferenced: A, B` when the own document holds fields nothing
/// names. `Err` when a document cannot be read or a name is missing; a
/// missing name's error carries the lines too, so a caller that prints only
/// the error loses nothing.
pub fn check(secrets: &Secrets, config: &CiConfig) -> Result<Vec<String>, String> {
    let names: BTreeSet<&String> = config
        .repos
        .values()
        .flat_map(|repo| repo.own_secret_names())
        .chain(
            config
                .onboarding
                .iter()
                .flat_map(|onb| [&onb.bot_token, &onb.admin_token]),
        )
        .collect();
    let granting: Vec<(&String, &crate::config::RepoConfig)> = config
        .repos
        .iter()
        .filter(|(_, repo)| !repo.grants.is_empty())
        .collect();
    if names.is_empty() && granting.is_empty() && secrets.config.is_none() {
        return Ok(Vec::new());
    }
    let fields = secrets.fields().map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    let mut missing = Vec::new();
    for name in &names {
        if fields.contains(*name) {
            lines.push(format!("{name} ok"));
        } else {
            lines.push(format!("{name} missing"));
            missing.push(name.to_string());
        }
    }
    for (repo_name, repo) in granting {
        let granted: Vec<String> = repo.grants.keys().cloned().collect();
        let origin = zeughaus_link::normalize_origin(&repo.url);
        let present = secrets
            .project_fields(repo_name, &origin, &granted)
            .map_err(|e| format!("{repo_name}: {e}"))?;
        for (name, ok) in present {
            if ok {
                lines.push(format!("{repo_name}: {name} ok"));
            } else {
                lines.push(format!("{repo_name}: {name} missing"));
                missing.push(format!("{repo_name}/{name}"));
            }
        }
    }
    // The fields the CI writes itself are kept, not named.
    let kept = |field: &str| config.claude.is_some() && crate::claude::OWN_FIELDS.contains(&field);
    let unreferenced: Vec<&str> = fields
        .iter()
        .filter(|field| !names.contains(field) && !kept(field))
        .map(String::as_str)
        .collect();
    if !unreferenced.is_empty() {
        lines.push(format!("unreferenced: {}", unreferenced.join(", ")));
    }
    if missing.is_empty() {
        return Ok(lines);
    }
    lines.push(format!("missing: {}", missing.join(", ")));
    Err(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn without_a_config_every_read_is_absent() {
        let secrets = Secrets::new(None, Path::new("/nonexistent"));
        assert_eq!(
            secrets.get("X").unwrap_err(),
            SecretError::Absent("no [secrets] in ci.toml; X not read".to_owned())
        );
        assert_eq!(
            secrets.fields().unwrap_err(),
            SecretError::Absent("no [secrets] in ci.toml; document not read".to_owned())
        );
        assert_eq!(secrets.document(), "(no [secrets])");
    }

    /// The runner every parsed config needs.
    const RUNNERS: &str = "default_runner = \"r\"\n[runners.r]\n\
        fingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n";

    #[test]
    fn check_without_secrets_or_names_is_empty() {
        let secrets = Secrets::new(None, Path::new("/nonexistent"));
        let config = CiConfig::parse(RUNNERS).unwrap();
        assert_eq!(check(&secrets, &config).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn patching_without_a_config_is_absent() {
        let secrets = Secrets::new(None, Path::new("/nonexistent"));
        assert_eq!(
            secrets.patch(None, &[("X", "v")]).unwrap_err(),
            SecretError::Absent("no [secrets] in ci.toml; X not stored".to_owned())
        );
    }

    #[test]
    fn job_policies_read_the_project_and_the_global_shell_of_granting_repos() {
        let config = CiConfig::parse(&format!(
            "{RUNNERS}[secrets]\naddr = \"http://bao\"\npath = \"zeughaus/zeughaus\"\nrole_id = \"r\"\n\
             [repos.a]\nurl = \"https://git.example.org/o/a.git\"\n\
             [repos.a.grants]\nT = [\"push main\"]\n\
             [repos.b]\nurl = \"https://git.example.org/o/b.git\"\n"
        ))
        .unwrap();
        let policies = job_policies(&config).unwrap();
        assert_eq!(
            policies.keys().collect::<Vec<_>>(),
            ["zeughaus-job-a"],
            "a repository without grants gets no policy"
        );
        assert_eq!(
            policies["zeughaus-job-a"],
            "path \"secret/data/zeughaus/git.example.org/o/a/shell\" {\n  capabilities = [\"read\"]\n}\n\n\
             path \"secret/data/zeughaus/shell\" {\n  capabilities = [\"read\"]\n}\n"
        );
        let bare = CiConfig::parse(RUNNERS).unwrap();
        assert_eq!(job_policies(&bare).unwrap_err(), "no [secrets] in ci.toml");
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::thread::JoinHandle;

        const LOGIN_URL: &str = "/v1/auth/approle/login";

        const READ_URL: &str = "/v1/secret/data/zeughaus/ci";

        /// An OpenBao that answers logins and counts them, and answers the reads of
        /// `zeughaus/ci` as its script says; the script gets the 0-based read number.
        /// Every other request is answered from `answers` (`METHOD url`) and logged.
        struct Stub {
            addr: String,
            logins: Arc<AtomicUsize>,
            reads: Arc<AtomicUsize>,
            login_body: Arc<Mutex<String>>,
            /// The bodies of the PATCHes of the document, which answer 204.
            patches: Arc<Mutex<Vec<String>>>,
            answers: Arc<Mutex<BTreeMap<String, (u16, String)>>>,
            requests: Arc<Mutex<Vec<Request>>>,
            stop: Arc<AtomicBool>,
            thread: Option<JoinHandle<()>>,
        }

        /// A request the stub answered from `answers`.
        #[derive(Debug, Clone)]
        struct Request {
            /// `METHOD url`.
            line: String,
            token: String,
            wrap_ttl: Option<String>,
            body: String,
        }

        impl Stub {
            fn start(script: impl Fn(usize) -> (u16, String) + Send + 'static) -> Stub {
                let server = tiny_http::Server::http("127.0.0.1:0").expect("bind the stub");
                let addr = format!(
                    "http://{}",
                    server.server_addr().to_ip().expect("an ip address")
                );
                let logins = Arc::new(AtomicUsize::new(0));
                let reads = Arc::new(AtomicUsize::new(0));
                let login_body = Arc::new(Mutex::new(String::new()));
                let patches = Arc::new(Mutex::new(Vec::new()));
                let answers = Arc::new(Mutex::new(BTreeMap::new()));
                let requests = Arc::new(Mutex::new(Vec::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let thread = {
                    let (logins, reads, login_body, patches, stop) = (
                        Arc::clone(&logins),
                        Arc::clone(&reads),
                        Arc::clone(&login_body),
                        Arc::clone(&patches),
                        Arc::clone(&stop),
                    );
                    let (answers, requests) = (Arc::clone(&answers), Arc::clone(&requests));
                    std::thread::spawn(move || {
                        while !stop.load(Ordering::SeqCst) {
                            let Ok(Some(mut request)) =
                                server.recv_timeout(Duration::from_millis(20))
                            else {
                                continue;
                            };
                            let url = request.url().to_owned();
                            let patch = request.method() == &tiny_http::Method::Patch;
                            let (code, body) = match url.as_str() {
                                LOGIN_URL => {
                                    logins.fetch_add(1, Ordering::SeqCst);
                                    let mut text = String::new();
                                    let _ = request.as_reader().read_to_string(&mut text);
                                    *login_body.lock().unwrap() = text;
                                    (200, r#"{"auth":{"client_token":"tok"}}"#.to_owned())
                                }
                                READ_URL if patch => {
                                    let mut text = String::new();
                                    let _ = request.as_reader().read_to_string(&mut text);
                                    patches.lock().unwrap().push(text);
                                    (204, String::new())
                                }
                                READ_URL => script(reads.fetch_add(1, Ordering::SeqCst)),
                                _ => {
                                    let line = format!("{} {url}", request.method());
                                    let header = |name: &'static str| {
                                        request
                                            .headers()
                                            .iter()
                                            .find(|h| h.field.equiv(name))
                                            .map(|h| h.value.as_str().to_owned())
                                    };
                                    let token = header("X-Vault-Token").unwrap_or_default();
                                    let wrap_ttl = header("X-Vault-Wrap-TTL");
                                    let mut body = String::new();
                                    let _ = request.as_reader().read_to_string(&mut body);
                                    requests.lock().unwrap().push(Request {
                                        line: line.clone(),
                                        token,
                                        wrap_ttl,
                                        body,
                                    });
                                    answers
                                        .lock()
                                        .unwrap()
                                        .get(&line)
                                        .cloned()
                                        .unwrap_or((500, String::new()))
                                }
                            };
                            let _ = request.respond(
                                tiny_http::Response::from_string(body).with_status_code(code),
                            );
                        }
                    })
                };
                Stub {
                    addr,
                    logins,
                    reads,
                    login_body,
                    patches,
                    answers,
                    requests,
                    stop,
                    thread: Some(thread),
                }
            }

            fn secrets(&self, ttl: u64) -> Arc<Secrets> {
                Secrets::with_secret_id(config(&self.addr, ttl), "sid-1".to_owned())
            }

            fn logins(&self) -> usize {
                self.logins.load(Ordering::SeqCst)
            }

            fn reads(&self) -> usize {
                self.reads.load(Ordering::SeqCst)
            }

            /// `Value::Null` answers an empty body.
            fn answer(&self, line: &str, code: u16, body: Value) {
                let text = if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                };
                self.answers
                    .lock()
                    .unwrap()
                    .insert(line.to_owned(), (code, text));
            }

            fn requests(&self) -> Vec<Request> {
                self.requests.lock().unwrap().clone()
            }

            /// A token role that mints, an unwrap that answers `job-tok`, and
            /// the project and global documents of repo `a` as given (`None`:
            /// the document does not exist).
            fn job_token_answers(&self, project: Option<Value>, global: Option<Value>) {
                self.answer(
                    TOKEN_CREATE,
                    200,
                    json!({ "wrap_info": { "token": "wrap-1", "wrapped_accessor": "acc-1" } }),
                );
                self.answer(
                    "POST /v1/sys/wrapping/unwrap",
                    200,
                    json!({ "auth": { "client_token": "job-tok" } }),
                );
                for (line, document) in [(PROJECT_READ, project), (GLOBAL_READ, global)] {
                    match document {
                        Some(data) => self.answer(line, 200, json!({ "data": { "data": data } })),
                        None => self.answer(line, 404, json!({ "errors": [] })),
                    }
                }
                self.answer("POST /v1/auth/token/revoke-self", 204, json!(null));
                self.answer("POST /v1/auth/token/revoke-accessor", 204, json!(null));
            }
        }

        const TOKEN_CREATE: &str = "POST /v1/auth/token/create/zeughaus-job";
        const PROJECT_READ: &str = "GET /v1/secret/data/zeughaus/git.example.org/o/a/shell";
        const GLOBAL_READ: &str = "GET /v1/secret/data/zeughaus/shell";

        impl Drop for Stub {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }

        fn config(addr: &str, ttl: u64) -> SecretsConfig {
            SecretsConfig {
                addr: addr.to_owned(),
                mount: "secret".to_owned(),
                path: "zeughaus/ci".to_owned(),
                role_id: "role-1".to_owned(),
                cache_ttl_seconds: ttl,
                timeout_seconds: 5,
                projects: "zeughaus".to_owned(),
                job_role: "zeughaus-job".to_owned(),
            }
        }

        fn document(data: Value) -> (u16, String) {
            (200, json!({ "data": { "data": data } }).to_string())
        }

        #[test]
        fn logs_in_and_reads_a_value() {
            let stub = Stub::start(|_| document(json!({ "A": "alpha" })));
            let secrets = stub.secrets(30);
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            assert_eq!(stub.logins(), 1);
            assert_eq!(stub.reads(), 1);
            let body: Value = serde_json::from_str(&stub.login_body.lock().unwrap()).unwrap();
            assert_eq!(body, json!({ "role_id": "role-1", "secret_id": "sid-1" }));
        }

        #[test]
        fn a_forbidden_read_logs_in_once_more() {
            let stub = Stub::start(|n| {
                if n == 0 {
                    (403, r#"{"errors":["permission denied"]}"#.to_owned())
                } else {
                    document(json!({ "A": "alpha" }))
                }
            });
            let secrets = stub.secrets(30);
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            assert_eq!(stub.logins(), 2);
            assert_eq!(stub.reads(), 2);
        }

        #[test]
        fn a_read_forbidden_after_the_second_login_is_denied() {
            let stub = Stub::start(|_| (403, "{}".to_owned()));
            let secrets = stub.secrets(30);
            assert_eq!(
                secrets.get("A").unwrap_err(),
                SecretError::Unavailable(
                    "openbao: secret/zeughaus/ci denied (policy zeughaus)".to_owned()
                )
            );
            assert_eq!(stub.logins(), 2);
            assert_eq!(stub.reads(), 2);
        }

        #[test]
        fn a_missing_document_is_absent() {
            let stub = Stub::start(|_| (404, r#"{"errors":[]}"#.to_owned()));
            assert_eq!(
                stub.secrets(30).get("A").unwrap_err(),
                SecretError::Absent("openbao: no document secret/zeughaus/ci".to_owned())
            );
        }

        #[test]
        fn a_missing_field_is_absent() {
            let stub = Stub::start(|_| document(json!({ "A": "alpha" })));
            assert_eq!(
                stub.secrets(30).get("B").unwrap_err(),
                SecretError::Absent("openbao: no field B in secret/zeughaus/ci".to_owned())
            );
        }

        #[test]
        fn a_sealed_openbao_is_unavailable() {
            let stub = Stub::start(|_| (503, r#"{"errors":["Vault is sealed"]}"#.to_owned()));
            let error = stub.secrets(30).get("A").unwrap_err();
            assert_eq!(
                error,
                SecretError::Unavailable(format!("openbao {} is sealed", stub.addr))
            );
        }

        #[test]
        fn a_closed_port_is_unreachable() {
            let port = {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                listener.local_addr().unwrap().port()
            };
            let addr = format!("http://127.0.0.1:{port}");
            let secrets = Secrets::with_secret_id(config(&addr, 30), "sid-1".to_owned());
            match secrets.get("A").unwrap_err() {
                SecretError::Unavailable(text) => {
                    assert!(
                        text.starts_with(&format!("openbao {addr} unreachable (")),
                        "{text}"
                    );
                    assert!(text.ends_with("); A not read"), "{text}");
                }
                other => panic!("unexpected {other:?}"),
            }
            match secrets.fields().unwrap_err() {
                SecretError::Unavailable(text) => {
                    assert!(text.ends_with("); document not read"), "{text}");
                }
                other => panic!("unexpected {other:?}"),
            }
        }

        #[test]
        fn a_rejected_login_is_unavailable() {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let addr = format!("http://{}", server.server_addr().to_ip().unwrap());
            let thread = std::thread::spawn(move || {
                let request = server.recv().unwrap();
                let _ =
                    request.respond(tiny_http::Response::from_string("{}").with_status_code(400));
            });
            let secrets = Secrets::with_secret_id(config(&addr, 30), "sid-1".to_owned());
            assert_eq!(
                secrets.get("A").unwrap_err(),
                SecretError::Unavailable(
                    "openbao login rejected (secret_id revoked or expired?)".to_owned()
                )
            );
            thread.join().unwrap();
        }

        #[test]
        fn a_value_over_several_lines_is_refused_without_being_shown() {
            let stub = Stub::start(|_| document(json!({ "A": "a\nb" })));
            let error = stub.secrets(30).get("A").unwrap_err();
            assert_eq!(
                error,
                SecretError::Absent(
                    "openbao: A in secret/zeughaus/ci spans several lines".to_owned()
                )
            );
            assert!(!error.to_string().contains("a\nb"));
        }

        #[test]
        fn one_line_end_is_stripped() {
            let stub = Stub::start(|_| document(json!({ "A": "x\n", "B": "y\r\n", "C": "z\n\n" })));
            let secrets = stub.secrets(30);
            assert_eq!(secrets.get("A").unwrap(), "x");
            assert_eq!(secrets.get("B").unwrap(), "y");
            assert!(matches!(secrets.get("C"), Err(SecretError::Absent(_))));
        }

        #[test]
        fn a_value_that_is_not_a_string_is_absent() {
            let stub = Stub::start(|_| document(json!({ "A": 7 })));
            assert_eq!(
                stub.secrets(30).get("A").unwrap_err(),
                SecretError::Absent("openbao: A in secret/zeughaus/ci is not a string".to_owned())
            );
        }

        #[test]
        fn the_document_is_cached_for_its_ttl() {
            let stub = Stub::start(|_| document(json!({ "A": "alpha", "B": "beta" })));
            let secrets = stub.secrets(30);
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            assert_eq!(secrets.get("B").unwrap(), "beta");
            assert_eq!(secrets.fields().unwrap().len(), 2);
            assert_eq!(stub.reads(), 1);
            assert_eq!(stub.logins(), 1);
        }

        #[test]
        fn a_zero_ttl_reads_every_time_and_reuses_the_token() {
            let stub = Stub::start(|_| document(json!({ "A": "alpha" })));
            let secrets = stub.secrets(0);
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            assert_eq!(stub.reads(), 2);
            assert_eq!(stub.logins(), 1);
        }

        #[test]
        fn a_failed_read_does_not_serve_the_old_value() {
            let stub = Stub::start(|n| {
                if n == 0 {
                    document(json!({ "A": "alpha" }))
                } else {
                    (503, "{}".to_owned())
                }
            });
            let secrets = stub.secrets(0);
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            assert!(matches!(secrets.get("A"), Err(SecretError::Unavailable(_))));
        }

        #[test]
        fn debug_shows_no_state() {
            let stub = Stub::start(|_| document(json!({ "A": "alpha" })));
            let secrets = stub.secrets(30);
            secrets.get("A").unwrap();
            let text = format!("{secrets:?}");
            assert!(!text.contains("alpha") && !text.contains("tok") && !text.contains("sid-1"));
        }

        #[test]
        fn an_added_field_is_patched_in_and_read_again() {
            let stub = Stub::start(|_| document(json!({ "A": "alpha" })));
            let secrets = stub.secrets(30);
            assert_eq!(secrets.get("A").unwrap(), "alpha");
            secrets.patch(None, &[("X_WEBHOOK_SECRET", "v")]).unwrap();
            let patches = stub.patches.lock().unwrap().clone();
            let body: Value = serde_json::from_str(&patches[0]).unwrap();
            assert_eq!(body, json!({ "data": { "X_WEBHOOK_SECRET": "v" } }));
            assert_eq!(patches.len(), 1);
            secrets.get("A").unwrap();
            assert_eq!(stub.reads(), 2);
            assert_eq!(stub.logins(), 1);
        }

        fn ci_config(addr: &str) -> CiConfig {
            CiConfig::parse(&format!(
                "{RUNNERS}[secrets]\naddr = \"{addr}\"\npath = \"zeughaus/ci\"\nrole_id = \"role-1\"\n\
                 [repos.a]\nurl = \"https://git.example.org/o/a.git\"\nfetch_token = \"A\"\nstatus_token = \"B\"\n\
                 [repos.a.grants]\nD = [\"tag v*\"]\nC = [\"tag v*\"]\n"
            ))
            .unwrap()
        }

        #[test]
        fn check_lists_own_and_project_names_and_the_unreferenced_fields() {
            let stub = Stub::start(|_| document(json!({ "A": "1", "B": "2", "Z": "4" })));
            stub.job_token_answers(Some(json!({ "C": "3" })), Some(json!({ "D": "5" })));
            let lines = check(&stub.secrets(30), &ci_config(&stub.addr)).unwrap();
            assert_eq!(
                lines,
                ["A ok", "B ok", "a: C ok", "a: D ok", "unreferenced: Z"]
            );
            // The project documents are read as a job reads them: with the
            // unwrapped job token, which is revoked afterwards.
            let requests = stub.requests();
            let lines: Vec<(&str, &str)> = requests
                .iter()
                .map(|r| (r.line.as_str(), r.token.as_str()))
                .collect();
            assert_eq!(
                lines,
                [
                    (TOKEN_CREATE, "tok"),
                    ("POST /v1/sys/wrapping/unwrap", "wrap-1"),
                    (PROJECT_READ, "job-tok"),
                    (GLOBAL_READ, "job-tok"),
                    ("POST /v1/auth/token/revoke-self", "job-tok"),
                    ("POST /v1/auth/token/revoke-accessor", "tok"),
                ]
            );
        }

        #[test]
        fn check_fails_on_a_missing_own_or_project_name() {
            let stub = Stub::start(|_| document(json!({ "A": "1" })));
            stub.job_token_answers(None, Some(json!({ "C": "3" })));
            let error = check(&stub.secrets(30), &ci_config(&stub.addr)).unwrap_err();
            assert_eq!(
                error,
                "A ok\nB missing\na: C ok\na: D missing\nmissing: B, a/D"
            );
        }

        #[test]
        fn check_fails_with_the_message_when_the_document_cannot_be_read() {
            let stub = Stub::start(|_| (404, "{}".to_owned()));
            let error = check(&stub.secrets(30), &ci_config(&stub.addr)).unwrap_err();
            assert_eq!(error, "openbao: no document secret/zeughaus/ci");
        }

        #[test]
        fn a_job_handoff_is_a_wrapped_token_of_the_repos_policy() {
            let stub = Stub::start(|_| document(json!({})));
            stub.job_token_answers(None, None);
            let names = vec!["C".to_owned(), "D".to_owned()];
            let (handoff, accessor) = stub
                .secrets(30)
                .job_handoff("a", "git.example.org/o/a", &names, "a.7.build")
                .unwrap();
            assert_eq!(accessor, "acc-1");
            assert_eq!(
                handoff,
                SecretHandoff {
                    addr: stub.addr.clone(),
                    mount: "secret".to_owned(),
                    project: "zeughaus/git.example.org/o/a/shell".to_owned(),
                    global: "zeughaus/shell".to_owned(),
                    names,
                    wrapping_token: "wrap-1".to_owned(),
                }
            );
            let request = &stub.requests()[0];
            assert_eq!(request.wrap_ttl.as_deref(), Some("120s"));
            let body: Value = serde_json::from_str(&request.body).unwrap();
            assert_eq!(
                body,
                json!({
                    "policies": ["zeughaus-job-a"],
                    "meta": { "run": "a.7.build", "repo": "a" },
                    "display_name": "job",
                    "ttl": "10m",
                })
            );
        }

        #[test]
        fn a_job_token_forbidden_after_the_second_login_names_the_policy() {
            let stub = Stub::start(|_| document(json!({})));
            stub.answer(
                TOKEN_CREATE,
                403,
                json!({ "errors": ["permission denied"] }),
            );
            let error = stub
                .secrets(30)
                .job_handoff("a", "o", &["C".to_owned()], "a.1.j")
                .unwrap_err();
            assert_eq!(
                error,
                SecretError::Unavailable(
                    "openbao: the CI may not mint job tokens (policy zeughaus needs update on auth/token/create/zeughaus-job; rerun deploy/ci/openbao.sh)".to_owned()
                )
            );
            assert_eq!(stub.logins(), 2);
        }

        #[test]
        fn a_refused_job_token_carries_openbaos_reason() {
            let stub = Stub::start(|_| document(json!({})));
            stub.answer(TOKEN_CREATE, 400, json!({ "errors": ["unknown role"] }));
            let error = stub
                .secrets(30)
                .job_handoff("a", "o", &["C".to_owned()], "a.1.j")
                .unwrap_err();
            assert_eq!(
                error,
                SecretError::Unavailable(
                    "openbao: job token for a refused: unknown role".to_owned()
                )
            );
        }

        #[test]
        fn revoking_a_token_that_is_gone_succeeds() {
            let stub = Stub::start(|_| document(json!({})));
            stub.answer(
                "POST /v1/auth/token/revoke-accessor",
                400,
                json!({ "errors": ["invalid accessor"] }),
            );
            stub.secrets(30).revoke_accessor("acc-1").unwrap();
            let body: Value = serde_json::from_str(&stub.requests()[0].body).unwrap();
            assert_eq!(body, json!({ "accessor": "acc-1" }));
        }
    }
}
