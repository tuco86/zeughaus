//! The `# /// ci` header block at the top of a `.zeughaus-ci/` script.
//!
//! The block is TOML inside comment lines, so the script stays valid for its
//! interpreter and a helper file without a block is simply not a job.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use zeughaus_link::enlist::WhenBusy;

use crate::config::{valid_env_name, valid_name};

/// The spelling of `when_busy` in a job header.
pub fn when_busy_name(when_busy: WhenBusy) -> &'static str {
    match when_busy {
        WhenBusy::Wait => "wait",
        WhenBusy::Freeze => "freeze",
        WhenBusy::Run => "run",
    }
}

/// Serialized into the pipeline record, so a restarted runner starts the
/// pending jobs from what was selected rather than re-reading the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobDef {
    pub name: String,
    /// File name inside `.zeughaus-ci/`, e.g. `check.sh`.
    pub file: String,
    pub on: Vec<String>,
    pub needs: Vec<String>,
    /// The channels a job with `needs` is added in; empty means every channel
    /// that selects a job it needs. Only meaningful together with `needs`.
    #[serde(default)]
    pub channels: Vec<String>,
    pub image: Option<String>,
    pub machine: Option<String>,
    /// A platform of `ci.toml` `[platforms]`: the job runs where that platform
    /// is built. Exclusive with `machine`.
    #[serde(default)]
    pub platform: Option<String>,
    /// A name shared by jobs that run one after another in one workspace,
    /// named after the context, instead of one workspace each. Such jobs
    /// have equal `image`, `machine`, `platform` and `cache`.
    #[serde(default)]
    pub context: Option<String>,
    pub cache: Vec<String>,
    pub secrets: Vec<String>,
    pub when_busy: WhenBusy,
    pub env: BTreeMap<String, String>,
    pub timeout_minutes: u32,
    /// `sh` or `bash` for `.sh` jobs, `powershell` for `.ps1` jobs.
    pub interpreter: String,
}

impl JobDef {
    /// The directory name of the job's workspace on a runner: its context,
    /// else its own name.
    pub fn workspace(&self) -> &str {
        self.context.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JobHeader {
    #[serde(default)]
    on: Vec<String>,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    channels: Vec<String>,
    image: Option<String>,
    machine: Option<String>,
    platform: Option<String>,
    context: Option<String>,
    #[serde(default)]
    cache: Vec<String>,
    #[serde(default)]
    secrets: Vec<String>,
    #[serde(default)]
    when_busy: WhenBusy,
    #[serde(default)]
    env: BTreeMap<String, String>,
    timeout_minutes: Option<u32>,
}

const START: &str = "# /// ci";
const END: &str = "# ///";

/// The header block's TOML text, `None` when the file has no block.
fn extract(text: &str) -> Result<Option<String>, String> {
    let mut lines = text.lines();
    if !lines.any(|l| l.trim_end() == START) {
        return Ok(None);
    }
    let mut toml = String::new();
    for line in lines {
        if line.trim_end() == END {
            return Ok(Some(toml));
        }
        let Some(rest) = line.strip_prefix('#') else {
            return Err(format!("header line does not start with `#`: {line}"));
        };
        toml.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        toml.push('\n');
    }
    Err(format!("header block is not closed by `{END}`"))
}

fn interpreter(ext: &str, text: &str) -> Result<String, String> {
    if ext == "ps1" {
        return Ok("powershell".into());
    }
    let Some(first) = text.lines().next().filter(|l| l.starts_with("#!")) else {
        return Ok("sh".into());
    };
    match first.trim_end() {
        "#!/bin/bash" | "#!/usr/bin/bash" | "#!/usr/bin/env bash" => Ok("bash".into()),
        "#!/bin/sh" | "#!/usr/bin/sh" | "#!/usr/bin/env sh" => Ok("sh".into()),
        other => Err(format!("unsupported shebang `{other}` (sh or bash)")),
    }
}

fn check_cache(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("cache path is empty".into());
    }
    if path.starts_with('/') || path.starts_with('\\') || path.as_bytes().get(1) == Some(&b':') {
        return Err(format!("cache path `{path}` must be relative"));
    }
    if path.split(['/', '\\']).any(|c| c == "..") {
        return Err(format!("cache path `{path}` must not contain `..`"));
    }
    Ok(())
}

/// Parses the header of one `.zeughaus-ci/` file. `Ok(None)` is a helper file.
pub fn parse_job(file_name: &str, text: &str) -> Result<Option<JobDef>, String> {
    let fail = |msg: String| format!("{}/{file_name}: {msg}", crate::CI_DIR);
    let Some(toml) = extract(text).map_err(fail)? else {
        return Ok(None);
    };
    let header: JobHeader = toml::from_str(&toml).map_err(|e| fail(e.to_string()))?;

    let (name, ext) = file_name
        .rsplit_once('.')
        .ok_or_else(|| fail("job file needs the extension .sh or .ps1".into()))?;
    if ext != "sh" && ext != "ps1" {
        return Err(fail(format!("extension .{ext} is not .sh or .ps1")));
    }
    if !valid_name(name) {
        return Err(fail(format!(
            "job name `{name}` must match [a-z0-9][a-z0-9-]*"
        )));
    }
    if let Some(p) = &header.platform
        && !valid_name(p)
    {
        return Err(fail(format!(
            "platform name `{p}` must match [a-z0-9][a-z0-9-]*"
        )));
    }
    if let Some(c) = &header.context
        && !valid_name(c)
    {
        return Err(fail(format!(
            "context name `{c}` must match [a-z0-9][a-z0-9-]*"
        )));
    }
    for path in &header.cache {
        check_cache(path).map_err(fail)?;
    }
    for secret in &header.secrets {
        if !valid_env_name(secret) {
            return Err(fail(format!(
                "secret name `{secret}` must match [A-Z_][A-Z0-9_]*"
            )));
        }
    }
    for key in header.env.keys() {
        if !valid_env_name(key) {
            return Err(fail(format!(
                "env name `{key}` must match [A-Z_][A-Z0-9_]*"
            )));
        }
        if key.starts_with("CI_") || key.starts_with("ZEUGHAUS_") {
            return Err(fail(format!(
                "env name `{key}` is reserved (CI_, ZEUGHAUS_)"
            )));
        }
    }
    let timeout_minutes = header.timeout_minutes.unwrap_or(240);
    if !(1..=1440).contains(&timeout_minutes) {
        return Err(fail(format!(
            "timeout_minutes {timeout_minutes} is outside 1..=1440"
        )));
    }
    let interpreter = interpreter(ext, text).map_err(fail)?;

    Ok(Some(JobDef {
        name: name.to_string(),
        file: file_name.to_string(),
        on: header.on,
        needs: header.needs,
        image: header.image,
        machine: header.machine,
        platform: header.platform,
        context: header.context,
        cache: header.cache,
        channels: header.channels,
        secrets: header.secrets,
        when_busy: header.when_busy,
        env: header.env,
        timeout_minutes,
        interpreter,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#!/bin/bash\n# build it\n# /// ci\n# on = [\"push main\"]\n# cache = [\".cache\"]\n# when_busy = \"freeze\"\n# image = \"ubuntu\"\n# [env]\n# FOO = \"1\"\n# ///\n# trailing comment\necho hi\n";

    #[test]
    fn extracts_block_between_other_comments() {
        let job = parse_job("check.sh", SAMPLE).unwrap().unwrap();
        assert_eq!(job.name, "check");
        assert_eq!(job.file, "check.sh");
        assert_eq!(job.on, ["push main"]);
        assert_eq!(job.cache, [".cache"]);
        assert_eq!(job.when_busy, WhenBusy::Freeze);
        assert_eq!(job.image.as_deref(), Some("ubuntu"));
        assert_eq!(job.env["FOO"], "1");
        assert_eq!(job.timeout_minutes, 240);
        assert_eq!(job.interpreter, "bash");
    }

    #[test]
    fn file_without_block_is_a_helper() {
        assert_eq!(
            parse_job("lib.sh", "# just a lib\nfoo() { :; }\n").unwrap(),
            None
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let text = "# /// ci\n# on = [\"push main\"]\n# bogus = 1\n# ///\n";
        let err = parse_job("x.sh", text).unwrap_err();
        assert!(err.starts_with(".zeughaus-ci/x.sh: "), "{err}");
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn unterminated_block_is_an_error() {
        let err = parse_job("x.sh", "# /// ci\n# on = []\necho\n").unwrap_err();
        assert!(err.contains("`#`") || err.contains("not closed"), "{err}");
        let err = parse_job("x.sh", "# /// ci\n# on = []\n").unwrap_err();
        assert!(err.contains("not closed"), "{err}");
    }

    #[test]
    fn name_and_extension_are_checked() {
        let text = "# /// ci\n# on = [\"push main\"]\n# ///\n";
        assert!(parse_job("Bad.sh", text).is_err());
        assert!(parse_job("x.py", text).is_err());
        assert!(parse_job("x", text).is_err());
        let ps = parse_job("win.ps1", text).unwrap().unwrap();
        assert_eq!(ps.interpreter, "powershell");
        assert_eq!(
            parse_job("a-1.sh", text).unwrap().unwrap().interpreter,
            "sh"
        );
    }

    #[test]
    fn field_rules() {
        let wrap = |body: &str| format!("# /// ci\n# on = [\"push main\"]\n# {body}\n# ///\n");
        assert!(parse_job("x.sh", &wrap("cache = [\"../x\"]")).is_err());
        assert!(parse_job("x.sh", &wrap("cache = [\"/x\"]")).is_err());
        assert!(parse_job("x.sh", &wrap("cache = [\"\"]")).is_err());
        assert!(parse_job("x.sh", &wrap("secrets = [\"lower\"]")).is_err());
        assert!(parse_job("x.sh", &wrap("env = { CI_X = \"1\" }")).is_err());
        assert!(parse_job("x.sh", &wrap("env = { ZEUGHAUS_X = \"1\" }")).is_err());
        assert!(parse_job("x.sh", &wrap("env = { bad = \"1\" }")).is_err());
        assert!(parse_job("x.sh", &wrap("timeout_minutes = 0")).is_err());
        assert!(parse_job("x.sh", &wrap("timeout_minutes = 1441")).is_err());
        let ok = parse_job("x.sh", &wrap("timeout_minutes = 1440"))
            .unwrap()
            .unwrap();
        assert_eq!(ok.timeout_minutes, 1440);
    }

    #[test]
    fn platform_is_parsed_and_its_name_checked() {
        let wrap = |body: &str| format!("# /// ci\n# on = [\"push main\"]\n# {body}\n# ///\n");
        let job = parse_job("x.sh", &wrap("platform = \"linux-arm64\""))
            .unwrap()
            .unwrap();
        assert_eq!(job.platform.as_deref(), Some("linux-arm64"));
        let err = parse_job("x.sh", &wrap("platform = \"Linux\"")).unwrap_err();
        assert!(err.contains("platform name `Linux`"), "{err}");
    }

    #[test]
    fn a_context_names_the_workspace_and_is_checked() {
        let wrap = |body: &str| format!("# /// ci\n# on = [\"push main\"]\n# {body}\n# ///\n");
        let alone = parse_job("gate.sh", &wrap("")).unwrap().unwrap();
        assert_eq!(alone.context, None);
        assert_eq!(alone.workspace(), "gate");
        let shared = parse_job("gate.sh", &wrap("context = \"linux\""))
            .unwrap()
            .unwrap();
        assert_eq!(shared.context.as_deref(), Some("linux"));
        assert_eq!(shared.workspace(), "linux");
        let err = parse_job("gate.sh", &wrap("context = \"Linux\"")).unwrap_err();
        assert!(err.contains("context name `Linux`"), "{err}");
    }

    #[test]
    fn shebang_selects_interpreter() {
        let body = "# /// ci\n# on = [\"push main\"]\n# ///\n";
        let with = |shebang: &str| format!("{shebang}\n{body}");
        for s in ["#!/bin/bash", "#!/usr/bin/bash", "#!/usr/bin/env bash"] {
            assert_eq!(
                parse_job("x.sh", &with(s)).unwrap().unwrap().interpreter,
                "bash"
            );
        }
        assert_eq!(
            parse_job("x.sh", &with("#!/bin/sh"))
                .unwrap()
                .unwrap()
                .interpreter,
            "sh"
        );
        assert!(parse_job("x.sh", &with("#!/usr/bin/python3")).is_err());
    }
}
