//! The scripts a CI job's terminal runs, rendered as text.
//!
//! `launch.sh` is the terminal's program on this machine for every kind of
//! job: it checks the commit out (on the host, or over ssh in the guest) and
//! starts the job where it runs. `inner.sh` / `inner.ps1` is the job itself
//! in its environment; on failure it records the exit code where
//! [`JobHost`](crate::jobs::JobHost)'s wait finds it and becomes a shell in
//! the same place, so a failed job is a terminal to debug in.

use std::fmt::Write as _;
use std::path::Path;

use zeughaus_link::enlist::Assignment;

use super::CI_DIR;
use super::stage::Engine;

/// Everything `launch.sh` needs that is not in the assignment.
pub struct Launch<'a> {
    pub assignment: &'a Assignment,
    /// `<state>/runs/<id>`.
    pub run_dir: &'a Path,
    /// Where the files holding secrets live; removed when the launcher ends.
    pub secret_dir: &'a Path,
    /// The bare mirror the commit is fetched from.
    pub mirror: &'a Path,
    pub target: Target<'a>,
}

/// Where this runner starts the job, with the settings of this runner that
/// the placement needs.
pub enum Target<'a> {
    /// Directly on this machine, in `workspace`.
    Host { workspace: &'a Path },
    /// In a rootless container of `engine` built for `image` and tagged
    /// `tag`; `workspace` is bind-mounted at /work.
    Container {
        engine: Engine,
        workspace: &'a Path,
        image: &'a str,
        tag: &'a str,
        cpus: u32,
        memory: &'a str,
    },
    /// In a Windows VM reached as `ssh_host`; the workspace lives in the
    /// guest.
    WindowsVm { ssh_host: &'a str },
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

/// `NAME=value` lines for `podman run --env-file` and `container run
/// --env-file`, which take the rest of the line verbatim. Values never
/// contain a newline: secrets holding one are refused when read, and the CI
/// variables are names and paths.
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
fn clean_excludes(cache: &[String], quote: fn(&str) -> String) -> String {
    cache
        .iter()
        .map(|path| format!(" -e {}", quote(&format!("/{}", path.trim_end_matches('/')))))
        .collect()
}

/// `launch.sh` for a job on this machine (host or container), or for a
/// Windows VM job, which has its own launcher.
pub fn launch_sh(launch: &Launch<'_>) -> String {
    let a = launch.assignment;
    let workspace = match &launch.target {
        Target::Host { workspace } | Target::Container { workspace, .. } => *workspace,
        Target::WindowsVm { .. } => return launch_sh_windows_vm(launch),
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
        n = a.number,
        excludes = clean_excludes(&a.cache, sh_quote),
    );
    let inner = launch.run_dir.join("inner.sh");
    match &launch.target {
        Target::Host { .. } => {
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
        Target::Container {
            engine,
            image,
            tag,
            cpus,
            memory,
            ..
        } => {
            let tag = sh_quote(tag);
            let file = sh_quote(&format!("{CI_DIR}/{image}.Containerfile"));
            let ctx = sh_quote(CI_DIR);
            let name = sh_quote(&format!("zci-{}", a.run));
            let memory = sh_quote(memory);
            let run = sh_path(launch.run_dir);
            let inputs = sh_path(&launch.run_dir.join("inputs"));
            match engine {
                Engine::Podman => {
                    let _ = write!(
                        out,
                        "podman image exists {tag} || podman build -t {tag} -f {file} {ctx} || exit 71\n\
                         podman run --rm -it --init --name {name} --cpus {cpus} --memory {memory} \
                         --env-file \"$secret_dir/env\" -v \"$ws:/work\" -v {run}:/ci/run \
                         -v {inputs}:/ci/inputs:ro -w /work {tag} /bin/sh /ci/run/inner.sh\n\
                         exit $?\n",
                    );
                }
                // `container` has no `:ro` on `-v`, so the inputs are writable.
                // The system is started on demand and left running; every
                // container is a VM of its own that `--rm` removes.
                Engine::Apple => {
                    let _ = write!(
                        out,
                        "container system status >/dev/null 2>&1 || container system start --enable-kernel-install || exit 71\n\
                         container image inspect {tag} >/dev/null 2>&1 || container build -t {tag} -f {file} --cpus {cpus} --memory {memory} {ctx} || exit 71\n\
                         container run --rm -it --init --name {name} --cpus {cpus} --memory {memory} \
                         --env-file \"$secret_dir/env\" -v \"$ws:/work\" -v {run}:/ci/run \
                         -v {inputs}:/ci/inputs -w /work {tag} /bin/sh /ci/run/inner.sh\n\
                         exit $?\n",
                    );
                }
            }
        }
        Target::WindowsVm { .. } => unreachable!("handled above"),
    }
    out
}

/// `inner.sh`: the job in its environment, then a shell if it failed.
///
/// A host job (`runner_exe` is this runner's path) is timed by the runner's
/// own `timeout` subcommand, because macOS has no coreutils one; a container
/// has coreutils, and kills a job that ignores `SIGTERM` a minute later.
pub fn inner_sh(a: &Assignment, runner_exe: Option<&Path>) -> String {
    let secs = u64::from(a.timeout_minutes) * 60;
    let file = sh_quote(&format!("{CI_DIR}/{}", a.file));
    let run = match runner_exe {
        Some(exe) => format!(
            "{} timeout {secs} -- {} {file}",
            sh_path(exe),
            a.interpreter
        ),
        None => format!("timeout -k 60 {secs} {} {file}", a.interpreter),
    };
    format!(
        "cd \"$CI_WORKSPACE\" || exit 70\n\
         mkdir -p \"$CI_OUTPUT\"\n\
         {run}; rc=$?\n\
         [ \"$rc\" -eq 0 ] && exit 0\n\
         printf '%s\\n' \"$rc\" > \"$ZEUGHAUS_RUN_DIR/code\"\n\
         printf '\\n[zeughaus-ci] %s exited %s; a shell follows, exit it to end the run\\n' \"$CI_JOB\" \"$rc\"\n\
         if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi\n",
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
pub fn inner_ps1(a: &Assignment) -> String {
    let run = guest_run_dir(&a.run);
    let excludes = clean_excludes(&a.cache, ps_quote);
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
        pipeline = a.number,
        file = ps_quote(&format!(r"{CI_DIR}\{}", a.file)),
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
/// The guest's copy of the run is removed when the launcher ends.
fn launch_sh_windows_vm(launch: &Launch<'_>) -> String {
    let Target::WindowsVm { ssh_host } = launch.target else {
        unreachable!("only Windows VM targets get here");
    };
    let run = launch.run_dir;
    let id = &launch.assignment.run;
    let guest = guest_run_dir(id);
    // scp addresses the guest's drives as /W:/...; ssh commands go to
    // PowerShell, the guest's default shell.
    let scp_dir = format!("/W:/ci/runs/{id}");
    let host = sh_quote(ssh_host);
    let debug_marker = sh_path(&run.join("debug-shell"));
    // The guest's copy of the run (inputs, secrets, bundle, output) goes
    // with the run however the launcher ends: after the copy-back, after the
    // shell that follows a failure, or when the terminal is closed. Failure
    // to remove is ignored, and a guest that does not answer is not waited
    // for.
    let remove_guest = sh_quote(&format!(
        "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue {guest}"
    ));
    let mut out = preamble(
        launch,
        &format!(
            "; rm -f {debug_marker}; ssh -o ConnectTimeout=15 {host} {remove_guest} </dev/null >/dev/null 2>&1 || true"
        ),
    );
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
        job = sh_quote(&launch.assignment.job),
    );
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zeughaus_link::enlist::{Place, WhenBusy};

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

    fn assignment(place: Place) -> Assignment {
        Assignment {
            run: "demo.7.build".to_owned(),
            repo: "demo".to_owned(),
            number: 7,
            job: "build".to_owned(),
            url: String::new(),
            fetch_user: String::new(),
            fetch_token: None,
            sha: "0".repeat(40),
            file: "build.sh".to_owned(),
            interpreter: "/bin/sh".to_owned(),
            place,
            cache: vec!["target/".to_owned()],
            env: Vec::new(),
            secrets: Vec::new(),
            timeout_minutes: 2,
            when_busy: WhenBusy::Wait,
            inputs: Vec::new(),
        }
    }

    /// A host job is timed by the runner's own subcommand, a container job by
    /// coreutils inside the image.
    #[test]
    fn the_timeout_is_the_runners_on_the_host_and_coreutils_in_a_container() {
        let a = assignment(Place::Host);
        let host = inner_sh(&a, Some(Path::new("/opt/zh runner")));
        assert!(host.contains("'/opt/zh runner' timeout 120 -- /bin/sh '.zeughaus-ci/build.sh'"));
        let container = inner_sh(&a, None);
        assert!(container.contains("timeout -k 60 120 /bin/sh '.zeughaus-ci/build.sh'"));
    }

    /// The rendered `inner.sh` of a host job runs the job under the given
    /// timeout program and reports a failure as the code file.
    #[test]
    fn a_failed_host_job_leaves_its_code_where_the_wait_finds_it() {
        let base = std::env::temp_dir().join(format!("zci-launch-inner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        let run = base.join("run");
        std::fs::create_dir_all(ws.join(CI_DIR)).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(ws.join(CI_DIR).join("build.sh"), "echo ran; exit 3\n").unwrap();
        // Stands in for `zeughaus-runner timeout <secs> -- <program> ..`.
        let stub = base.join("timeout-stub");
        std::fs::write(&stub, "#!/bin/sh\nshift 3\nexec \"$@\"\n").unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&stub)
            .status()
            .unwrap();
        let a = assignment(Place::Host);
        let script = inner_sh(&a, Some(&stub))
            // The shell that follows a failure would wait for input.
            .replace("if command -v bash", "exit 0; if command -v bash");
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("CI_WORKSPACE", &ws)
            .env("CI_OUTPUT", base.join("out"))
            .env("CI_JOB", "build")
            .env("ZEUGHAUS_RUN_DIR", &run)
            .output()
            .expect("run sh");
        let code = std::fs::read_to_string(run.join("code")).ok();
        let _ = std::fs::remove_dir_all(&base);
        assert!(String::from_utf8_lossy(&output.stdout).contains("ran"));
        assert_eq!(code.as_deref(), Some("3\n"));
    }

    fn container_script(engine: Engine) -> String {
        let a = assignment(Place::Container {
            image: "arch".to_owned(),
            tag: "localhost/zci-demo-arch:0123456789abcdef".to_owned(),
        });
        let workspace = PathBuf::from("/s/ci/work/demo/build");
        launch_sh(&Launch {
            assignment: &a,
            run_dir: Path::new("/s/runs/4"),
            secret_dir: Path::new("/run/zeughaus-ci/demo.7.build"),
            mirror: Path::new("/s/ci/mirrors/demo.git"),
            target: Target::Container {
                engine,
                workspace: &workspace,
                image: "arch",
                tag: "localhost/zci-demo-arch:0123456789abcdef",
                cpus: 12,
                memory: "24g",
            },
        })
    }

    #[test]
    fn a_container_launcher_builds_the_image_and_names_the_run() {
        let script = container_script(Engine::Podman);
        assert!(script.contains("-f '.zeughaus-ci/arch.Containerfile' '.zeughaus-ci'"));
        assert!(script.contains("--name 'zci-demo.7.build'"));
        assert!(script.contains("'+refs/ci/7:refs/ci/head'"));
        assert!(script.contains("-e '/target'"));
        assert!(script.contains("podman run --rm -it --init"));
        assert!(script.contains("/ci/inputs:ro"));
    }

    /// Apple's `container` starts its system on demand and has no read-only
    /// volume.
    #[test]
    fn an_apple_container_launcher_starts_the_system_and_mounts_inputs_writable() {
        let script = container_script(Engine::Apple);
        assert!(script.contains(
            "container system status >/dev/null 2>&1 || container system start --enable-kernel-install || exit 71"
        ));
        assert!(
            script.contains("container image inspect 'localhost/zci-demo-arch:0123456789abcdef'")
        );
        assert!(script.contains("container build -t 'localhost/zci-demo-arch:0123456789abcdef' -f '.zeughaus-ci/arch.Containerfile' --cpus 12 --memory '24g' '.zeughaus-ci'"));
        assert!(script.contains("container run --rm -it --init --name 'zci-demo.7.build'"));
        assert!(script.contains("-v '/s/runs/4/inputs':/ci/inputs -w /work"));
        assert!(!script.contains("podman"));
        assert!(!script.contains(":ro"));
    }
}
