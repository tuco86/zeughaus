//! The CI's secret hand-off of a job (weida's hand-off, decisions/0032 §2):
//! the assignment carries a single-use wrapping token of a short-lived job
//! token. This runner redeems it before anything else of the run happens,
//! reads the granted names from the project document, then the global one,
//! and revokes the job token. A wrapping token someone else redeemed first
//! is an alarm: the job does not start.

use std::time::Duration;

use serde_json::{Map, Value, json};
use weida_openbao::{Auth, Config, Error, HandoffSource, OpenBao, Secret};
use zeughaus_link::enlist::{SecretHandoff, secret_line};

/// Per request, the redemption included.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The values of `h.names`, in their order.
pub fn redeem(h: &SecretHandoff) -> Result<Vec<(String, String)>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("secret hand-off: cannot start a runtime: {e}"))?;
    runtime.block_on(async {
        let bao = OpenBao::new(Config {
            allow_insecure: h.addr.starts_with("http://"),
            timeout: TIMEOUT,
            ..Config::new(&h.addr)
        })
        .map_err(|e| format!("secret hand-off: {e}"))?;
        bao.login(Auth::Handoff(HandoffSource::Token(Secret::new(
            h.wrapping_token.clone(),
        ))))
        .await
        .map_err(|e| match e {
            Error::HandoffStolen => "ALARM: the secret hand-off was already redeemed by someone \
                 else; the job did not start"
                .to_owned(),
            e => format!("secret hand-off: {e}"),
        })?;
        let read = async |document: &str| -> Result<Map<String, Value>, String> {
            match bao.get(&format!("{}/data/{document}", h.mount)).await {
                Ok(mut answer) => Ok(match answer.pointer_mut("/data/data") {
                    Some(Value::Object(fields)) => std::mem::take(fields),
                    _ => Map::new(),
                }),
                Err(Error::Api { status: 404, .. }) => Ok(Map::new()),
                Err(e) => Err(format!("secret hand-off: {}/{document}: {e}", h.mount)),
            }
        };
        let fields = async {
            let project = read(&h.project).await?;
            let global = read(&h.global).await?;
            Ok::<_, String>((project, global))
        }
        .await;
        if let Err(e) = bao.post("auth/token/revoke-self", json!({})).await {
            eprintln!("[enlist] revoke-self failed: {e}");
        }
        let (project, global) = fields?;
        pick(h, &project, &global)
    })
}

/// Each granted name's value: the project document's wins over the global
/// one's. A name in neither, or a value over several lines, is refused.
fn pick(
    h: &SecretHandoff,
    project: &Map<String, Value>,
    global: &Map<String, Value>,
) -> Result<Vec<(String, String)>, String> {
    h.names
        .iter()
        .map(|name| {
            let value = project
                .get(name)
                .or_else(|| global.get(name))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "secret {name} is in neither {mount}/{project} nor {mount}/{global}",
                        mount = h.mount,
                        project = h.project,
                        global = h.global
                    )
                })?;
            let value =
                secret_line(value).ok_or_else(|| format!("secret {name} spans several lines"))?;
            Ok((name.clone(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff(names: &[&str]) -> SecretHandoff {
        SecretHandoff {
            addr: "http://10.8.0.1:8200".to_owned(),
            mount: "secret".to_owned(),
            project: "zeughaus/git.example.org/o/r/shell".to_owned(),
            global: "zeughaus/shell".to_owned(),
            names: names.iter().map(|n| (*n).to_owned()).collect(),
            wrapping_token: "w".to_owned(),
        }
    }

    fn doc(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn the_project_value_wins_and_the_global_one_is_the_fallback() {
        let project = doc(json!({ "A": "project-a\n" }));
        let global = doc(json!({ "A": "global-a", "B": "global-b" }));
        assert_eq!(
            pick(&handoff(&["B", "A"]), &project, &global).unwrap(),
            [
                ("B".to_owned(), "global-b".to_owned()),
                ("A".to_owned(), "project-a".to_owned()),
            ]
        );
    }

    #[test]
    fn a_name_in_neither_document_names_both() {
        let error = pick(&handoff(&["C"]), &Map::new(), &Map::new()).unwrap_err();
        assert_eq!(
            error,
            "secret C is in neither secret/zeughaus/git.example.org/o/r/shell nor secret/zeughaus/shell"
        );
    }

    #[test]
    fn a_value_over_several_lines_is_refused_without_being_shown() {
        let project = doc(json!({ "A": "line1\nline2" }));
        let error = pick(&handoff(&["A"]), &project, &Map::new()).unwrap_err();
        assert_eq!(error, "secret A spans several lines");
    }
}
