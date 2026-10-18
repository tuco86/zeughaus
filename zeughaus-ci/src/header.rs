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
/// Keys a stored record carries that this struct does not know are skipped:
/// records outlive the code that wrote them.
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
    pub secrets: Vec<String>,
    pub when_busy: WhenBusy,
    pub env: BTreeMap<String, String>,
    pub timeout_minutes: u32,
    /// `sh` or `bash` for `.sh` jobs, `powershell` for `.ps1` jobs.
    pub interpreter: String,
    /// Header keys that parse and do nothing, for `zeughaus-ci check` to
    /// name. Never stored.
    #[serde(skip)]
    pub ignored_keys: Vec<&'static str>,
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
    /// `context` and `cache` parse and are ignored: a repository's jobs at
    /// one place share one workspace, and a checkout keeps ignored files.
    /// Other repositories' headers still carry them.
    #[serde(default)]
    context: Option<toml::Value>,
    #[serde(default)]
    cache: Option<toml::Value>,
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
    // The image names the job's workspace on a runner, a directory there.
    if let Some(image) = &header.image
        && !valid_name(image)
    {
        return Err(fail(format!(
            "image name `{image}` must match [a-z0-9][a-z0-9-]*"
        )));
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
    let ignored_keys = [
        ("context", header.context.is_some()),
        ("cache", header.cache.is_some()),
    ]
    .into_iter()
    .filter_map(|(key, present)| present.then_some(key))
    .collect();

    Ok(Some(JobDef {
        name: name.to_string(),
        file: file_name.to_string(),
        on: header.on,
        needs: header.needs,
        image: header.image,
        machine: header.machine,
        platform: header.platform,
        channels: header.channels,
        secrets: header.secrets,
        when_busy: header.when_busy,
        env: header.env,
        timeout_minutes,
        interpreter,
        ignored_keys,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#!/bin/bash\n# build it\n# /// ci\n# on = [\"push main\"]\n# when_busy = \"freeze\"\n# image = \"ubuntu\"\n# [env]\n# FOO = \"1\"\n# ///\n# trailing comment\necho hi\n";

    #[test]
    fn extracts_block_between_other_comments() {
        let job = parse_job("check.sh", SAMPLE).unwrap().unwrap();
        assert_eq!(job.name, "check");
        assert_eq!(job.file, "check.sh");
        assert_eq!(job.on, ["push main"]);
        assert!(job.ignored_keys.is_empty());
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
    fn the_name_of_an_image_is_checked() {
        let wrap = |body: &str| format!("# /// ci\n# on = [\"push main\"]\n# {body}\n# ///\n");
        let job = parse_job("x.sh", &wrap("image = \"ubuntu24\""))
            .unwrap()
            .unwrap();
        assert_eq!(job.image.as_deref(), Some("ubuntu24"));
        for bad in ["Ubuntu", "ubuntu_24", "../x", "ubuntu.24", ""] {
            let err = parse_job("x.sh", &wrap(&format!("image = \"{bad}\""))).unwrap_err();
            assert!(err.contains(&format!("image name `{bad}`")), "{err}");
        }
    }

    #[test]
    fn context_and_cache_parse_and_are_ignored() {
        let wrap = |body: &str| format!("# /// ci\n# on = [\"push main\"]\n# {body}\n# ///\n");
        let job = parse_job("x.sh", &wrap("context = \"x\"\n# cache = [\".c\"]"))
            .unwrap()
            .unwrap();
        assert_eq!(job.ignored_keys, ["context", "cache"]);
        let plain = parse_job("x.sh", &wrap("")).unwrap().unwrap();
        assert!(plain.ignored_keys.is_empty());
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
