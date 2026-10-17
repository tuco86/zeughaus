//! The JSON-RPC half of the Model Context Protocol: newline-delimited
//! messages on a reader and a writer, which for the server are its stdin and
//! stdout.
//!
//! Only what a tool server needs is here: `initialize`, `ping`,
//! `tools/list` and `tools/call`. A notification is read and ignored, a
//! method nobody serves is answered with `-32601`, and a batch is refused:
//! the newest protocol revision has none.
//!
//! Every `tools/call` is a task of its own and answers through the one
//! writer, which a lock holds for the length of a message so that two
//! answers never interleave: an agent's subagents share this process and
//! call in parallel, and a tool may wait minutes. At the end of the input
//! the calls still running finish and answer before [`serve`] returns, so a
//! fixed script piped in gets every answer.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::task::{Id, JoinError, JoinSet};

/// The protocol revisions this server speaks, newest last. A client that asks
/// for one of them gets it; any other client gets [`LATEST`] and decides
/// whether it can follow.
const PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST: &str = "2025-06-18";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// What the transport serves.
pub trait Tools: Send + Sync + 'static {
    /// One paragraph a client may show its model next to the tool list.
    fn instructions(&self) -> &'static str;

    /// The `tools` array of `tools/list`.
    fn definitions(&self) -> Vec<Value>;

    /// Runs one tool. `Ok` is the text of the result, `Err` the text of a
    /// result flagged `isError`: a tool that fails still answers.
    fn call(
        &self,
        name: &str,
        arguments: Value,
    ) -> impl Future<Output = Result<String, String>> + Send;
}

/// What one line of input asks of the server.
enum Step {
    /// Nothing to answer: a notification, or a response to nothing we sent.
    Ignore,
    /// Answer now.
    Answer(Value),
    /// A `tools/call`, which runs as a task and answers when it is done.
    Call {
        id: Value,
        name: String,
        arguments: Value,
    },
}

/// Reads messages from `input` until it ends and writes the answers to
/// `output`. Returns when the input has ended and every call has answered;
/// an error is the input failing to be read.
pub async fn serve<T, R, W>(tools: Arc<T>, input: R, output: W) -> std::io::Result<()>
where
    T: Tools,
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let output = Arc::new(Output {
        writer: Mutex::new(output),
    });
    // `split`, not `lines`: a line that is not UTF-8 is one malformed message
    // and must not end the session. Both read cancel-safely.
    let mut segments = input.split(b'\n');
    let mut calls: JoinSet<()> = JoinSet::new();
    // The request each running call answers, for the one that panics.
    let mut asked: HashMap<Id, Value> = HashMap::new();
    let mut read_error = None;

    loop {
        tokio::select! {
            segment = segments.next_segment() => match segment {
                Ok(Some(mut bytes)) => {
                    if bytes.last() == Some(&b'\r') {
                        bytes.pop();
                    }
                    let line = String::from_utf8_lossy(&bytes);
                    if line.trim().is_empty() {
                        continue;
                    }
                    match step(tools.as_ref(), &line) {
                        Step::Ignore => {}
                        Step::Answer(message) => output.send(&message).await,
                        Step::Call { id, name, arguments } => {
                            let tools = Arc::clone(&tools);
                            let output = Arc::clone(&output);
                            let answer_id = id.clone();
                            let handle = calls.spawn(async move {
                                let result = match tools.call(&name, arguments).await {
                                    Ok(text) => tool_result(&text, false),
                                    Err(text) => tool_result(&text, true),
                                };
                                output.send(&success(&answer_id, result)).await;
                            });
                            asked.insert(handle.id(), id);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    read_error = Some(e);
                    break;
                }
            },
            Some(done) = calls.join_next_with_id(), if !calls.is_empty() => {
                reap(done, &mut asked, &output).await;
            }
        }
    }
    while let Some(done) = calls.join_next_with_id().await {
        reap(done, &mut asked, &output).await;
    }
    read_error.map_or(Ok(()), Err)
}

/// Forgets a finished call. One that panicked never wrote its answer, and a
/// client waits for it forever unless it is told.
async fn reap<W: AsyncWrite + Unpin>(
    done: Result<(Id, ()), JoinError>,
    asked: &mut HashMap<Id, Value>,
    output: &Output<W>,
) {
    match done {
        Ok((task, ())) => {
            asked.remove(&task);
        }
        Err(e) => {
            eprintln!("[mcp] a tool call failed: {e}");
            if let Some(id) = asked.remove(&e.id()) {
                output
                    .send(&failure(&id, INTERNAL_ERROR, "the tool call failed"))
                    .await;
            }
        }
    }
}

/// Reads one message and decides what it asks for.
fn step<T: Tools>(tools: &T, line: &str) -> Step {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(e) => {
            return Step::Answer(failure(
                &Value::Null,
                PARSE_ERROR,
                format!("parse error: {e}"),
            ));
        }
    };
    let Value::Object(mut message) = message else {
        return Step::Answer(failure(
            &Value::Null,
            INVALID_REQUEST,
            "a message is a JSON object; batches are not supported",
        ));
    };
    let id = message.remove("id");
    let Some(method) = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        // A response to a request this server never made is none of its
        // business; anything else that names no method is not a message.
        let is_response = message.contains_key("result") || message.contains_key("error");
        return match id {
            Some(id) if !is_response => {
                Step::Answer(failure(&id, INVALID_REQUEST, "the message names no method"))
            }
            _ => Step::Ignore,
        };
    };
    // Without an id it is a notification, which nothing answers.
    let Some(id) = id else {
        return Step::Ignore;
    };
    let params = message.remove("params").unwrap_or(Value::Null);
    match method.as_str() {
        "initialize" => Step::Answer(success(&id, initialize(tools, &params))),
        "ping" => Step::Answer(success(&id, json!({}))),
        "tools/list" => Step::Answer(success(&id, json!({ "tools": tools.definitions() }))),
        "tools/call" => match call_params(params) {
            Ok((name, arguments)) => Step::Call {
                id,
                name,
                arguments,
            },
            Err(message) => Step::Answer(failure(&id, INVALID_PARAMS, message)),
        },
        other => Step::Answer(failure(
            &id,
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        )),
    }
}

fn initialize<T: Tools>(tools: &T, params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|asked| PROTOCOLS.contains(asked))
        .unwrap_or(LATEST);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "zeughaus", "version": env!("CARGO_PKG_VERSION") },
        "instructions": tools.instructions(),
    })
}

/// The tool's name and its arguments, which default to none.
fn call_params(params: Value) -> Result<(String, Value), String> {
    let Value::Object(mut params) = params else {
        return Err("tools/call takes an object with a name and arguments".to_owned());
    };
    let Some(Value::String(name)) = params.remove("name") else {
        return Err("tools/call names no tool".to_owned());
    };
    let arguments = match params.remove("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(arguments @ Value::Object(_)) => arguments,
        Some(_) => return Err("the arguments of a tool are an object".to_owned()),
    };
    Ok((name, arguments))
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn success(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

/// The writer every answer goes through.
struct Output<W> {
    writer: Mutex<W>,
}

impl<W: AsyncWrite + Unpin> Output<W> {
    /// One message and its newline, flushed, under the lock. A writer that
    /// fails means the client is gone, which the end of its input says soon
    /// enough; the answer is lost either way.
    async fn send(&self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        let mut writer = self.writer.lock().await;
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            eprintln!("[mcp] cannot write an answer: {e}");
            return;
        }
        if let Err(e) = writer.flush().await {
            eprintln!("[mcp] cannot flush an answer: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, BufReader};

    use super::*;

    /// Two tools: `echo` answers at once, `slow` after a pause, `fail`
    /// answers with an error and `panic` never answers.
    struct Stub;

    impl Tools for Stub {
        fn instructions(&self) -> &'static str {
            "stub"
        }

        fn definitions(&self) -> Vec<Value> {
            vec![json!({ "name": "echo" }), json!({ "name": "slow" })]
        }

        async fn call(&self, name: &str, arguments: Value) -> Result<String, String> {
            match name {
                "echo" => Ok(arguments.to_string()),
                "slow" => {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    Ok("slow done".to_owned())
                }
                "fail" => Err("it failed".to_owned()),
                "panic" => panic!("the tool panicked"),
                other => Err(format!("unknown tool {other}")),
            }
        }
    }

    /// Feeds `script` to a server and returns every line it wrote, parsed.
    async fn run(script: &str) -> Vec<Value> {
        let (mut read, write) = tokio::io::duplex(1 << 20);
        serve(Arc::new(Stub), BufReader::new(script.as_bytes()), write)
            .await
            .expect("served");
        let mut written = String::new();
        read.read_to_string(&mut written).await.expect("answers");
        written
            .lines()
            .map(|line| serde_json::from_str(line).expect("an answer is JSON"))
            .collect()
    }

    fn by_id(answers: &[Value], id: u64) -> &Value {
        answers
            .iter()
            .find(|answer| answer["id"] == json!(id))
            .unwrap_or_else(|| panic!("no answer for {id}: {answers:?}"))
    }

    #[tokio::test]
    async fn initialize_offers_the_tools_capability_and_the_clients_revision() {
        let answers = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            "\n",
        ))
        .await;
        let first = &by_id(&answers, 1)["result"];
        assert_eq!(first["protocolVersion"], "2025-03-26");
        assert_eq!(first["capabilities"], json!({ "tools": {} }));
        assert_eq!(first["serverInfo"]["name"], "zeughaus");
        assert_eq!(first["instructions"], "stub");
        // A revision it does not know is answered with the newest it does.
        assert_eq!(by_id(&answers, 2)["result"]["protocolVersion"], LATEST);
    }

    #[tokio::test]
    async fn notifications_are_not_answered_and_ping_is() {
        let answers = run(concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n\n",
            r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#,
            "\r\n",
        ))
        .await;
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert_eq!(answers[0]["id"], "p");
        assert_eq!(answers[0]["result"], json!({}));
    }

    #[tokio::test]
    async fn an_unknown_method_is_32601() {
        let answers = run(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#).await;
        assert_eq!(by_id(&answers, 3)["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn malformed_input_is_answered_and_does_not_end_the_session() {
        let answers = run(concat!(
            "not json\n",
            "[1,2]\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"arguments":{}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
            "\n",
        ))
        .await;
        assert_eq!(answers[0]["error"]["code"], -32700);
        assert_eq!(answers[0]["id"], Value::Null);
        assert_eq!(answers[1]["error"]["code"], -32600);
        assert_eq!(by_id(&answers, 4)["error"]["code"], -32602);
        assert_eq!(by_id(&answers, 5)["result"], json!({}));
    }

    #[tokio::test]
    async fn tools_list_and_call_wrap_the_tools_text() {
        let answers = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo","arguments":{"a":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fail"}}"#,
            "\n",
        ))
        .await;
        assert_eq!(by_id(&answers, 1)["result"]["tools"][1]["name"], "slow");
        let echoed = &by_id(&answers, 2)["result"];
        assert_eq!(echoed["isError"], false);
        assert_eq!(echoed["content"][0]["type"], "text");
        assert_eq!(echoed["content"][0]["text"], r#"{"a":1}"#);
        let failed = &by_id(&answers, 3)["result"];
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["content"][0]["text"], "it failed");
    }

    /// The input ends while a call still runs: its answer must still be
    /// written, and a call that started later but finished sooner answers
    /// first -- they do not wait for each other.
    #[tokio::test]
    async fn calls_run_in_parallel_and_answer_after_the_input_ended() {
        let answers = run(concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slow"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"echo"}}"#,
            "\n",
        ))
        .await;
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0]["id"], 2);
        assert_eq!(answers[1]["id"], 1);
        assert_eq!(answers[1]["result"]["content"][0]["text"], "slow done");
    }

    #[tokio::test]
    async fn a_call_that_panics_is_answered_with_an_error() {
        let answers = run(concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"panic"}}"#,
            "\n",
        ))
        .await;
        assert_eq!(by_id(&answers, 7)["error"]["code"], -32603);
    }
}
