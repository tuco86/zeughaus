//! The scripts a CI job's terminal runs, rendered as text.
//!
//! `launch.sh` is the terminal's program on this machine for every kind of
//! job: it checks the commit out (on the host, or over ssh in the guest or on
//! a unix host) and starts the job where it runs. `inner.sh` / `inner.ps1` is
//! the job itself in its environment; on failure it records the exit code
//! where [`JobHost`](crate::jobs::JobHost)'s wait finds it and becomes a shell
//! in the same place, so a failed job is a terminal to debug in. A unix host
//! runs the job from `inner.sh` staged in its own run directory, whose
//! launcher reads the exit status off the ssh session instead.

use std::fmt::Write as _;
use std::path::Path;

use super::CI_DIR;
use super::header::JobDef;

/// Everything `launch.sh` needs that is not in the job header.
pub struct Launch<'a> {
    pub job: &'a JobDef,
    /// `<state>/runs/<id>`.
    pub run_dir: &'a Path,
    /// The run id, the run directory's basename.
    pub run_id: &'a str,
    /// Where the files holding secrets live; removed when the launcher ends.
    pub secret_dir: &'a Path,
    /// The bare mirror the commit is fetched from.
    pub mirror: &'a Path,
    /// The pipeline number; the commit is `refs/ci/<n>` in the mirror.
    pub pipeline: u64,
    pub place: Place<'a>,
}

pub enum Place<'a> {
    /// Directly on this machine, in `workspace`.
    Host { workspace: &'a Path },
    /// In a rootless podman container; `workspace` is bind-mounted at /work.
    Container {
        workspace: &'a Path,
        tag: &'a str,
        cpus: u32,
        memory: &'a str,
    },
    /// In a Windows VM reached as `ssh_host`; the workspace lives in the
    /// guest.
    WindowsVm { ssh_host: &'a str },
    /// On a unix host (macOS included) reached as `ssh_host`; `dir` is its
    /// absolute CI directory, below which `runs/` and `work/` live.
    UnixHost { ssh_host: &'a str, dir: &'a str },
}

/// A word for `sh`, quoted so nothing in it is interpreted.
pub fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn sh_path(path: &Path) -> String {
    sh_quote(&path.to_string_lossy())
}

/// A string for PowerShell in single quotes, which interpret nothing.
pub fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `export NAME='value'` lines, sourced by a host job's launcher.
pub fn env_sh(vars: &[(String, String)]) -> String {
    vars.iter().fold(String::new(), |mut out, (key, value)| {
        let _ = writeln!(out, "export {key}={}", sh_quote(value));
        out
    })
}

/// `NAME=value` lines for `podman run --env-file`, which takes the rest of
/// the line verbatim. Values never contain a newline: secrets holding one
/// are refused when read, and the CI variables are names and paths.
pub fn env_file(vars: &[(String, String)]) -> String {
    vars.iter().fold(String::new(), |mut out, (key, value)| {
        let _ = writeln!(out, "{key}={value}");
        out
    })
}

/// `$env:NAME = '...'` lines, dot-sourced in the guest.
pub fn env_ps1(vars: &[(String, String)]) -> String {
    vars.iter().fold(String::new(), |mut out, (key, value)| {
        let _ = writeln!(out, "$env:{key} = {}", ps_quote(value));
        out
    })
}

/// The traps every launcher starts with: the secret directory goes however
/// the launcher ends, and a hangup (the terminal closed) or a stop ends it
/// with the conventional code instead of carrying on.
fn preamble(launch: &Launch<'_>, extra_cleanup: &str) -> String {
    format!(
        "set -u\n\
         secret_dir={secret}\n\
         cleanup() {{ rm -rf \"$secret_dir\"{extra_cleanup}; }}\n\
         trap cleanup EXIT\n\
         trap 'exit 129' HUP\n\
         trap 'exit 143' TERM\n\
         trap 'exit 130' INT\n",
        secret = sh_path(launch.secret_dir),
    )
}

/// `-e /<path>` for every cache path, so `git clean` keeps it.
fn clean_excludes(job: &JobDef, quote: fn(&str) -> String) -> String {
    job.cache
        .iter()
        .map(|path| format!(" -e {}", quote(&format!("/{}", path.trim_end_matches('/')))))
        .collect()
}

/// `launch.sh` for a job on this machine (host or container), or for the
/// remote kinds, which have their own launchers.
pub fn launch_sh(launch: &Launch<'_>) -> String {
    let job = launch.job;
    let workspace = match &launch.place {
        Place::Host { workspace } | Place::Container { workspace, .. } => *workspace,
        Place::WindowsVm { .. } => return launch_sh_windows_vm(launch),
        Place::UnixHost { .. } => return launch_sh_unix_host(launch),
    };
    let mut out = preamble(launch, "");
    let _ = write!(
        out,
        "ws={ws}\n\
         mkdir -p \"$ws\" && cd \"$ws\" || {{ printf '[zeughaus-ci] cannot enter %s\\n' \"$ws\"; exit 70; }}\n\
         [ -d .git ] || git init -q || exit 70\n\
         if ! {{ git fetch -q --no-tags {mirror} '+refs/ci/{n}:refs/ci/head' \
         && git checkout -q --force --detach refs/ci/head \
         && git clean -ffdxq{excludes}; }}; then\n\
         \tprintf '[zeughaus-ci] checkout failed\\n'\n\
         \texit 70\n\
         fi\n\
         touch .git/zci-last-used\n",
        ws = sh_path(workspace),
        mirror = sh_path(launch.mirror),
        n = launch.pipeline,
        excludes = clean_excludes(job, sh_quote),
    );
    let inner = launch.run_dir.join("inner.sh");
    match &launch.place {
        Place::Host { .. } => {
            // Read into this shell and deleted at once: the job and its
            // debug shell inherit the values, and no file holds them while
            // a failed run's shell stays open for days.
            let _ = write!(
                out,
                "set -a\n\
                 . \"$secret_dir/env.sh\" || exit 70\n\
                 set +a\n\
                 rm -f \"$secret_dir/env.sh\"\n\
                 /bin/sh {inner}\n\
                 exit $?\n",
                inner = sh_path(&inner),
            );
        }
        Place::Container {
            tag, cpus, memory, ..
        } => {
            let image = job.image.as_deref().unwrap_or_default();
            let _ = write!(
                out,
                "podman image exists {tag} || podman build -t {tag} -f {file} {ctx} || exit 71\n\
                 podman run --rm -it --init --name {name} --cpus {cpus} --memory {memory} \
                 --env-file \"$secret_dir/env\" -v \"$ws:/work\" -v {run}:/ci/run \
                 -v {inputs}:/ci/inputs:ro -w /work {tag} /bin/sh /ci/run/inner.sh\n\
                 exit $?\n",
                tag = sh_quote(tag),
                file = sh_quote(&format!("{CI_DIR}/{image}.Containerfile")),
                ctx = sh_quote(CI_DIR),
                name = sh_quote(&format!("zci-{}", launch.run_id)),
                memory = sh_quote(memory),
                run = sh_path(launch.run_dir),
                inputs = sh_path(&launch.run_dir.join("inputs")),
            );
        }
        Place::WindowsVm { .. } | Place::UnixHost { .. } => unreachable!("handled above"),
    }
    out
}

/// `inner.sh`: the job in its environment, then a shell if it failed.
pub fn inner_sh(job: &JobDef) -> String {
    format!(
        "cd \"$CI_WORKSPACE\" || exit 70\n\
         mkdir -p \"$CI_OUTPUT\"\n\
         timeout -k 60 {secs} {interpreter} {file}; rc=$?\n\
         [ \"$rc\" -eq 0 ] && exit 0\n\
         printf '%s\\n' \"$rc\" > \"$ZEUGHAUS_RUN_DIR/code\"\n\
         printf '\\n[zeughaus-ci] %s exited %s; a shell follows, exit it to end the run\\n' \"$CI_JOB\" \"$rc\"\n\
         if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi\n",
        secs = u64::from(job.timeout_minutes) * 60,
        interpreter = job.interpreter,
        file = sh_quote(&format!("{CI_DIR}/{}", job.file)),
    )
}

/// The guest directory of a run: `W:\ci\runs\<id>`.
pub fn guest_run_dir(run_id: &str) -> String {
    format!(r"W:\ci\runs\{run_id}")
}

/// The guest workspace of a job: `W:\work\<repo>\<job>`.
pub fn guest_workspace(repo: &str, job: &str) -> String {
    format!(r"W:\work\{repo}\{job}")
}

/// `inner.ps1`: the checkout from the run's bundle, then the job.
///
/// Its exit code is also written to `<guest run>\rc`: Windows OpenSSH
/// reports 0 for a session with a terminal (`ssh -tt`) whatever the remote
/// command exited with, so the launcher reads the file instead.
pub fn inner_ps1(job: &JobDef, run_id: &str, pipeline: u64) -> String {
    let run = guest_run_dir(run_id);
    let excludes = clean_excludes(job, ps_quote);
    format!(
        "$ErrorActionPreference = 'Stop'\r\n\
         function Finish([int]$code) {{ Set-Content -Path {rc} -Value $code -Encoding ascii; exit $code }}\r\n\
         . {env}\r\n\
         New-Item -ItemType Directory -Force $env:CI_WORKSPACE, $env:CI_OUTPUT | Out-Null\r\n\
         Set-Location $env:CI_WORKSPACE\r\n\
         $ErrorActionPreference = 'Continue'\r\n\
         if (-not (Test-Path .git)) {{ git init -q; if ($LASTEXITCODE -ne 0) {{ Finish 70 }} }}\r\n\
         git fetch -q {bundle} '+refs/ci/{pipeline}:refs/ci/head'\r\n\
         if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n\
         git checkout -q --force --detach refs/ci/head\r\n\
         if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n\
         git clean -ffdxq{excludes}\r\n\
         if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n\
         New-Item .git\\zci-last-used -ItemType File -Force | Out-Null\r\n\
         & powershell -NoLogo -NoProfile -ExecutionPolicy Bypass -File {file}\r\n\
         Finish $LASTEXITCODE\r\n",
        rc = ps_quote(&format!(r"{run}\rc")),
        env = ps_quote(&format!(r"{run}\env.ps1")),
        bundle = ps_quote(&format!(r"{run}\src.bundle")),
        file = ps_quote(&format!(r"{CI_DIR}\{}", job.file)),
    )
}

/// `debug.ps1`: the job's environment and workspace, for the shell that
/// follows a failure.
pub fn debug_ps1(run_id: &str) -> String {
    format!(
        ". {env}\r\nSet-Location $env:CI_WORKSPACE\r\n",
        env = ps_quote(&format!(r"{}\env.ps1", guest_run_dir(run_id))),
    )
}

/// `launch.sh` for a Windows VM job: stage the run in the guest over ssh, run
/// it there, copy its output back, and on failure open a shell in the guest.
fn launch_sh_windows_vm(launch: &Launch<'_>) -> String {
    let Place::WindowsVm { ssh_host } = launch.place else {
        unreachable!("only Windows VM places get here");
    };
    let run = launch.run_dir;
    let id = launch.run_id;
    let guest = guest_run_dir(id);
    // scp addresses the guest's drives as /W:/...; ssh commands go to
    // PowerShell, the guest's default shell.
    let scp_dir = format!("/W:/ci/runs/{id}");
    let host = sh_quote(ssh_host);
    let debug_marker = sh_path(&run.join("debug-shell"));
    let mut out = preamble(launch, &format!("; rm -f {debug_marker}"));
    let _ = write!(
        out,
        "run={run}\n\
         ssh {host} {mkdir} </dev/null || exit 75\n\
         scp -q \"$run/vm/inner.ps1\" \"$run/vm/debug.ps1\" \"$run/src.bundle\" \"$secret_dir/env.ps1\" {host}:{dest} || exit 75\n\
         scp -q -r \"$run/inputs\" {host}:{dest_in} || exit 75\n\
         exec 3<&0\n\
         ssh -tt {host} {inner} <&3 &\n\
         echo $! > \"$run/ssh.pid\"\n\
         wait $!\n\
         rc=$?\n\
         rm -f \"$run/ssh.pid\"\n\
         guest_rc=$(ssh {host} {read_rc} </dev/null 2>/dev/null | tr -dc 0-9)\n\
         if [ -n \"$guest_rc\" ]; then rc=$guest_rc; elif [ \"$rc\" -eq 0 ]; then rc=75; fi\n\
         [ -e \"$run/timeout\" ] && rc=124\n\
         if [ \"$rc\" -eq 0 ]; then\n\
         \tscp -q -r {host}:{dest_out} \"$run/artifacts\" || rc=74\n\
         \t[ \"$rc\" -eq 0 ] && exit 0\n\
         fi\n\
         printf '%s\\n' \"$rc\" > \"$run/code\"\n\
         printf '\\n[zeughaus-ci] %s exited %s; a shell in the guest follows\\n' {job} \"$rc\"\n\
         touch {debug_marker}\n\
         ssh -tt {host} {debug} <&3\n\
         exit $rc\n",
        run = sh_path(run),
        mkdir = sh_quote(&format!(
            "New-Item -ItemType Directory -Force {guest} | Out-Null"
        )),
        read_rc = sh_quote(&format!(
            r"Get-Content -Raw {guest}\rc -ErrorAction SilentlyContinue"
        )),
        dest = sh_quote(&format!("{scp_dir}/")),
        dest_in = sh_quote(&format!("{scp_dir}/in")),
        dest_out = sh_quote(&format!("{scp_dir}/out")),
        inner = sh_quote(&format!(
            r"powershell -NoLogo -NoProfile -ExecutionPolicy Bypass -File {guest}\inner.ps1"
        )),
        debug = sh_quote(&format!(
            r"powershell -NoLogo -NoExit -ExecutionPolicy Bypass -File {guest}\debug.ps1"
        )),
        job = sh_quote(&launch.job.name),
    );
    out
}

/// The run directory on a unix host: `<dir>/runs/<id>`.
pub fn remote_run_dir(dir: &str, run_id: &str) -> String {
    format!("{dir}/runs/{run_id}")
}

/// The workspace of a job on a unix host: `<dir>/work/<repo>/<job>`.
pub fn remote_workspace(dir: &str, repo: &str, job: &str) -> String {
    format!("{dir}/work/{repo}/{job}")
}

/// The command that finds out whether a unix host answers, and removes the
/// run directories a killed launcher left behind (with their `env.sh`). The
/// age is above the debug shell's hold, so no live run loses its directory.
pub fn host_probe(dir: &str) -> String {
    let runs = sh_quote(&format!("{dir}/runs"));
    format!(
        "mkdir -p {runs} && find {runs} -mindepth 1 -maxdepth 1 -mmin +1500 -exec rm -rf {{}} +"
    )
}

/// `inner.sh` on a unix host: the checkout from the run's bundle, then the
/// job. Its exit status is the ssh session's. There is no `timeout`
/// (macOS has none); the scheduler's clock ends the local ssh instead.
pub fn inner_unix(job: &JobDef, dir: &str, run_id: &str, pipeline: u64) -> String {
    format!(
        "set -u\n\
         run={run}\n\
         set -a\n\
         . \"$run/env.sh\" || exit 70\n\
         set +a\n\
         mkdir -p \"$CI_WORKSPACE\" \"$CI_OUTPUT\" && cd \"$CI_WORKSPACE\" || {{ printf '[zeughaus-ci] cannot enter %s\\n' \"$CI_WORKSPACE\"; exit 70; }}\n\
         [ -d .git ] || git init -q || exit 70\n\
         if ! {{ git fetch -q --no-tags \"$run/src.bundle\" '+refs/ci/{pipeline}:refs/ci/head' \
         && git checkout -q --force --detach refs/ci/head \
         && git clean -ffdxq{excludes}; }}; then\n\
         \tprintf '[zeughaus-ci] checkout failed\\n'\n\
         \texit 70\n\
         fi\n\
         touch .git/zci-last-used\n\
         exec {interpreter} {file}\n",
        run = sh_quote(&remote_run_dir(dir, run_id)),
        excludes = clean_excludes(job, sh_quote),
        interpreter = job.interpreter,
        file = sh_quote(&format!("{CI_DIR}/{}", job.file)),
    )
}

/// `debug.sh`: the job's environment and workspace, for the shell that
/// follows a failure.
pub fn debug_unix(dir: &str, run_id: &str) -> String {
    format!(
        "run={run}\n\
         set -a\n\
         . \"$run/env.sh\"\n\
         set +a\n\
         cd \"$CI_WORKSPACE\" 2>/dev/null\n\
         exec \"${{SHELL:-/bin/sh}}\" -i\n",
        run = sh_quote(&remote_run_dir(dir, run_id)),
    )
}

/// `launch.sh` for a unix host job: stage the run on the host over ssh, run
/// it there, copy its output back, and on failure open a shell on the host.
///
/// Remote command strings are quoted for the remote shell and then once more
/// as the local word. scp paths are one local word and no more: OpenSSH's
/// scp speaks sftp by default, which takes the path as it is, so the remote
/// directory (absolute, free of shell metacharacters) is not quoted for the
/// remote side.
fn launch_sh_unix_host(launch: &Launch<'_>) -> String {
    let Place::UnixHost { ssh_host, dir } = launch.place else {
        unreachable!("only unix host places get here");
    };
    let run = launch.run_dir;
    let remote = remote_run_dir(dir, launch.run_id);
    let remote_word = sh_quote(&remote);
    let host = sh_quote(ssh_host);
    let debug_marker = sh_path(&run.join("debug-shell"));
    let remove = sh_quote(&format!("rm -rf {remote_word}"));
    let mut out = preamble(
        launch,
        &format!("; rm -f {debug_marker}; ssh {host} {remove} </dev/null >/dev/null 2>&1"),
    );
    let _ = write!(
        out,
        "run={run}\n\
         ssh {host} {mkdir} </dev/null || exit 75\n\
         scp -q \"$run/remote/inner.sh\" \"$run/remote/debug.sh\" \"$run/src.bundle\" \"$secret_dir/env.sh\" {dest} || exit 75\n\
         scp -q -r \"$run/inputs\" {dest_in} || exit 75\n\
         exec 3<&0\n\
         ssh -tt {host} {inner} <&3 &\n\
         echo $! > \"$run/ssh.pid\"\n\
         wait $!\n\
         rc=$?\n\
         rm -f \"$run/ssh.pid\"\n\
         [ -e \"$run/timeout\" ] && rc=124\n\
         if [ \"$rc\" -eq 0 ]; then\n\
         \tscp -q -r {dest_out} \"$run/artifacts\" || rc=74\n\
         \t[ \"$rc\" -eq 0 ] && exit 0\n\
         fi\n\
         printf '%s\\n' \"$rc\" > \"$run/code\"\n\
         printf '\\n[zeughaus-ci] %s exited %s; a shell on %s follows\\n' {job} \"$rc\" {host}\n\
         touch {debug_marker}\n\
         ssh -tt {host} {debug} <&3\n\
         exit $rc\n",
        run = sh_path(run),
        mkdir = sh_quote(&format!("mkdir -p {remote_word}")),
        dest = sh_quote(&format!("{ssh_host}:{remote}/")),
        dest_in = sh_quote(&format!("{ssh_host}:{remote}/in")),
        dest_out = sh_quote(&format!("{ssh_host}:{remote}/out")),
        inner = sh_quote(&format!("sh {remote_word}/inner.sh")),
        debug = sh_quote(&format!("sh {remote_word}/debug.sh")),
        job = sh_quote(&launch.job.name),
    );
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::header::parse_job;
    use super::*;

    #[test]
    fn shell_quoting_survives_quotes() {
        let quoted = sh_quote("it's $HOME");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {quoted}"))
            .output()
            .expect("run sh");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "it's $HOME");
    }

    #[test]
    fn env_sh_round_trips_through_a_shell() {
        let vars = vec![
            ("A".to_owned(), "x 'y' $z".to_owned()),
            ("B".to_owned(), "plain".to_owned()),
        ];
        let script = format!("{}printf '%s|%s' \"$A\" \"$B\"", env_sh(&vars));
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run sh");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "x 'y' $z|plain");
    }

    #[test]
    fn powershell_quotes_double_single_quotes() {
        assert_eq!(ps_quote("it's"), "'it''s'");
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=ci", "-c", "user.email=ci@example.invalid"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "ci")
            .env("GIT_AUTHOR_EMAIL", "ci@example.invalid")
            .env("GIT_COMMITTER_NAME", "ci")
            .env("GIT_COMMITTER_EMAIL", "ci@example.invalid")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Runs `inner_unix` for a job whose file holds `body`, against a
    /// directory that stands in for the remote run directory. Returns the
    /// script's exit code, the committed sha and the scratch directory.
    fn run_inner_unix(test: &str, body: &str) -> (i32, String, PathBuf) {
        let base = std::env::temp_dir().join(format!("zci-launch-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let remote = base.join("remote");
        let run = remote.join("runs/r1");
        std::fs::create_dir_all(repo.join(CI_DIR)).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        let text = format!("#!/bin/sh\n# /// ci\n# on = [\"push main\"]\n# ///\n{body}");
        std::fs::write(repo.join(CI_DIR).join("t.sh"), &text).unwrap();
        let job = parse_job("t.sh", &text).unwrap().unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "t"]);
        git(&repo, &["update-ref", "refs/ci/1", "HEAD"]);
        let sha = git(&repo, &["rev-parse", "HEAD"]);
        let bundle = run.join("src.bundle");
        git(
            &repo,
            &[
                "bundle",
                "create",
                "-q",
                bundle.to_str().unwrap(),
                "refs/ci/1",
            ],
        );
        let vars = vec![
            (
                "CI_WORKSPACE".to_owned(),
                base.join("ws").to_string_lossy().into_owned(),
            ),
            (
                "CI_OUTPUT".to_owned(),
                run.join("out").to_string_lossy().into_owned(),
            ),
            ("CI_SHA".to_owned(), sha.clone()),
        ];
        std::fs::write(run.join("env.sh"), env_sh(&vars)).unwrap();
        let script = inner_unix(&job, remote.to_str().unwrap(), "r1", 1);
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::null())
            .status()
            .expect("run sh");
        (status.code().expect("exit code"), sha, base)
    }

    #[test]
    fn unix_inner_checks_out_the_bundle_and_runs_the_job() {
        let (code, sha, base) =
            run_inner_unix("ok", "printf %s \"$CI_SHA\" > \"$CI_OUTPUT/sha\"\n");
        let written = std::fs::read_to_string(base.join("remote/runs/r1/out/sha")).ok();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(code, 0);
        assert_eq!(written.as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn unix_inner_passes_the_job_exit_code_on() {
        let (code, _, base) = run_inner_unix("fail", "exit 3\n");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(code, 3);
    }
}
