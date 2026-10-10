//! The values of the secrets `ci.toml` names, read from OpenBao (KV v2) when
//! they are used.
//!
//! The runner holds the only OpenBao token: it logs in with its own AppRole,
//! whose secret_id is sealed with `systemd-creds` to this machine and the CI
//! user, and keeps the token in memory. A job is handed the values its
//! `grants` release and never a token. The runner, the hook and the CLI each
//! build their own [`Secrets`] against the same role.
//!
//! One document holds every secret. It is read as a whole and answered from
//! memory for `cache_ttl_seconds`, so a rotation in OpenBao takes effect within
//! that time without a restart. A failed read drops the cached document: a
//! value that OpenBao no longer vouches for is never served. Nothing is renewed;
//! a token the server no longer accepts is replaced by one new login.
//!
//! No error text and no `Debug` output carries a secret value, token or
//! secret_id.

use std::collections::BTreeSet;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::config::{CiConfig, SecretsConfig};

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
                            "openbao: {}/{} denied (policy zeughaus-ci)",
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
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    // A header or a URL built from a value with a line break would be
    // injected into; refuse it where it enters.
    if text.contains('\n') {
        return Err(SecretError::Absent(format!(
            "openbao: {name} in {document} spans several lines"
        )));
    }
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

/// `ci secrets-check`: whether every secret `config` names is a field of the
/// document. The lines are `NAME ok` or `NAME missing` in name order, then
/// `unreferenced: A, B` when the document holds fields nothing names. `Err`
/// when the document cannot be read or a name is missing; a missing name's
/// error carries the lines too, so a caller that prints only the error loses
/// nothing.
pub fn check(secrets: &Secrets, config: &CiConfig) -> Result<Vec<String>, String> {
    let names: BTreeSet<&String> = config
        .repos
        .values()
        .flat_map(|repo| repo.secret_names())
        .collect();
    if names.is_empty() && secrets.config.is_none() {
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
            missing.push(name.as_str());
        }
    }
    let unreferenced: Vec<&str> = fields
        .iter()
        .filter(|field| !names.contains(field))
        .map(String::as_str)
        .collect();
    if !unreferenced.is_empty() {
        lines.push(format!("unreferenced: {}", unreferenced.join(", ")));
    }
    if missing.is_empty() {
        return Ok(lines);
    }
    lines.push(format!(
        "missing in {}: {}",
        secrets.document(),
        missing.join(", ")
    ));
    Err(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::JoinHandle;

    const LOGIN_URL: &str = "/v1/auth/approle/login";
    const READ_URL: &str = "/v1/secret/data/zeughaus/ci";

    /// An OpenBao that answers logins and counts them, and answers the reads of
    /// `zeughaus/ci` as its script says; the script gets the 0-based read number.
    struct Stub {
        addr: String,
        logins: Arc<AtomicUsize>,
        reads: Arc<AtomicUsize>,
        login_body: Arc<Mutex<String>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
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
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (logins, reads, login_body, stop) = (
                    Arc::clone(&logins),
                    Arc::clone(&reads),
                    Arc::clone(&login_body),
                    Arc::clone(&stop),
                );
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let Ok(Some(mut request)) = server.recv_timeout(Duration::from_millis(20))
                        else {
                            continue;
                        };
                        let url = request.url().to_owned();
                        let (code, body) = match url.as_str() {
                            LOGIN_URL => {
                                logins.fetch_add(1, Ordering::SeqCst);
                                let mut text = String::new();
                                let _ = request.as_reader().read_to_string(&mut text);
                                *login_body.lock().unwrap() = text;
                                (200, r#"{"auth":{"client_token":"tok"}}"#.to_owned())
                            }
                            READ_URL => script(reads.fetch_add(1, Ordering::SeqCst)),
                            _ => (500, String::new()),
                        };
                        let _ = request
                            .respond(tiny_http::Response::from_string(body).with_status_code(code));
                    }
                })
            };
            Stub {
                addr,
                logins,
                reads,
                login_body,
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
    }

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
                "openbao: secret/zeughaus/ci denied (policy zeughaus-ci)".to_owned()
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
            let _ = request.respond(tiny_http::Response::from_string("{}").with_status_code(400));
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
            SecretError::Absent("openbao: A in secret/zeughaus/ci spans several lines".to_owned())
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

    #[test]
    fn debug_shows_no_state() {
        let stub = Stub::start(|_| document(json!({ "A": "alpha" })));
        let secrets = stub.secrets(30);
        secrets.get("A").unwrap();
        let text = format!("{secrets:?}");
        assert!(!text.contains("alpha") && !text.contains("tok") && !text.contains("sid-1"));
    }

    fn ci_config(addr: &str) -> CiConfig {
        CiConfig::parse(&format!(
            "[secrets]\naddr = \"{addr}\"\npath = \"zeughaus/ci\"\nrole_id = \"role-1\"\n\
             [repos.a]\nurl = \"x\"\nfetch_token = \"A\"\nstatus_token = \"B\"\n\
             [repos.a.grants]\nC = [\"tag v*\"]\nA = [\"tag v*\"]\n"
        ))
        .unwrap()
    }

    #[test]
    fn check_lists_every_name_and_the_unreferenced_fields() {
        let stub = Stub::start(|_| document(json!({ "A": "1", "B": "2", "C": "3", "Z": "4" })));
        let lines = check(&stub.secrets(30), &ci_config(&stub.addr)).unwrap();
        assert_eq!(lines, ["A ok", "B ok", "C ok", "unreferenced: Z"]);
    }

    #[test]
    fn check_fails_on_a_missing_name() {
        let stub = Stub::start(|_| document(json!({ "A": "1" })));
        let error = check(&stub.secrets(30), &ci_config(&stub.addr)).unwrap_err();
        assert_eq!(
            error,
            "A ok\nB missing\nC missing\nmissing in secret/zeughaus/ci: B, C"
        );
    }

    #[test]
    fn check_fails_with_the_message_when_the_document_cannot_be_read() {
        let stub = Stub::start(|_| (404, "{}".to_owned()));
        let error = check(&stub.secrets(30), &ci_config(&stub.addr)).unwrap_err();
        assert_eq!(error, "openbao: no document secret/zeughaus/ci");
    }

    #[test]
    fn check_without_secrets_or_names_is_empty() {
        let secrets = Secrets::new(None, Path::new("/nonexistent"));
        let config = CiConfig::parse("").unwrap();
        assert_eq!(check(&secrets, &config).unwrap(), Vec::<String>::new());
    }
}
