//! The scripts a CI job's terminal runs, rendered as text.
//!
//! `launch.sh` is the terminal's program on a unix runner for every kind of
//! job: it checks the commit out (on the host, or over ssh in the guest) and
//! starts the job where it runs. `inner.sh` / `inner.ps1` is the job itself
//! in its environment; an Apple container reaches `inner.sh` through
//! `clock.sh`, which sets the VM's clock first. Every script ends with the
//! job's exit code, and the terminal closes with it: nothing stays behind a
//! failed job. A Windows runner runs host jobs only, and `launch.ps1` does
//! all of that in one script.

use std::fmt::Write as _;
use std::path::Path;

use zeughaus_link::enlist::{Assignment, Checkout, Upload};

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

/// The `cache` paths a checkout keeps: a wipe keeps none.
fn kept(a: &Assignment) -> &[String] {
    match a.checkout {
        Checkout::Wipe => &[],
        Checkout::Clean | Checkout::Continue => &a.cache,
    }
}

/// The part of `launch.sh` that enters the workspace and settles what is in
/// it: the pipeline's commit checked out and the pipeline written to
/// `.git/zci-context` (`Clean`, `Wipe`), or the check that an earlier job of
/// the pipeline's context left it (`Continue`: the job after it finds what it
/// built, and a workspace someone else took in between is refused).
fn enter_sh(a: &Assignment, workspace: &Path, mirror: &Path) -> String {
    let n = a.number;
    let mut out = format!(
        "ws={ws}\n\
         mkdir -p \"$ws\" && cd \"$ws\" || {{ printf '[zeughaus-ci] cannot enter %s\\n' \"$ws\"; exit 70; }}\n",
        ws = sh_path(workspace),
    );
    match a.checkout {
        Checkout::Continue => {
            let _ = writeln!(
                out,
                "[ \"$(cat .git/zci-context 2>/dev/null)\" = \"{n}\" ] || {{ printf '[zeughaus-ci] context workspace %s no longer holds pipeline #%s\\n' \"$ws\" {n}; exit 70; }}",
            );
        }
        Checkout::Clean | Checkout::Wipe => {
            let _ = write!(
                out,
                "[ -d .git ] || git init -q || exit 70\n\
                 if ! {{ git fetch -q --no-tags {mirror} '+refs/ci/{n}:refs/ci/head' \
                 && git checkout -q --force --detach refs/ci/head \
                 && git clean -ffdxq{excludes} \
                 && printf '%s\\n' {n} > .git/zci-context; }}; then\n\
                 \tprintf '[zeughaus-ci] checkout failed\\n'\n\
                 \texit 70\n\
                 fi\n",
                mirror = sh_path(mirror),
                excludes = clean_excludes(kept(a), sh_quote),
            );
        }
    }
    out.push_str("touch .git/zci-last-used\n");
    out
}

/// The file bundle `index` of a try run is downloaded to, in the run's own
/// directory, and the launcher checks out from.
pub(super) fn bundle_file(run_dir: &Path, index: u32) -> std::path::PathBuf {
    run_dir.join("bundles").join(format!("{index}.bundle"))
}

/// The directory name of a try run's main checkout, the first of its upload.
fn main_dir(upload: &Upload) -> &str {
    upload.dirs.first().map_or("", |dir| dir.dir.as_str())
}

/// The part of `launch.sh` of a try run that enters its working trees, in
/// place of [`enter_sh`]. Every checkout of the upload is a directory of its
/// own below `root`, the workspace all tries of the repository share: the
/// main checkout is the first, and the path dependencies it reaches through
/// `../` sit beside it. Each bundle holds one commit. `git clean` has no
/// `-x`, so what an earlier try built there (`target/`) stays, and the ref
/// an earlier bundle left is pruned. The bundles go as soon as they are in,
/// and the script ends up in the main checkout.
fn enter_try_sh(upload: &Upload, root: &Path, run_dir: &Path) -> String {
    let mut out = format!("root={}\n", sh_path(root));
    for (index, dir) in (0u32..).zip(&upload.dirs) {
        let name = sh_quote(&dir.dir);
        let _ = write!(
            out,
            "d=\"$root\"/{name}\n\
             mkdir -p \"$d\" && cd \"$d\" || {{ printf '[zeughaus-ci] cannot enter %s\\n' \"$d\"; exit 70; }}\n\
             [ -d .git ] || git init -q || exit 70\n\
             git fetch -q --no-tags --prune {bundle} '+refs/zeughaus-try/*:refs/zeughaus-try/*' \
             && git checkout -q --force --detach {sha} && git clean -ffdq \
             || {{ printf '[zeughaus-ci] checkout of %s failed\\n' {name}; exit 70; }}\n",
            bundle = sh_path(&bundle_file(run_dir, index)),
            sha = sh_quote(&dir.sha),
        );
    }
    let _ = write!(
        out,
        "rm -rf {bundles}\n\
         touch \"$root/.zci-last-used\"\n\
         ws=\"$root\"/{main}\n\
         cd \"$ws\"\n",
        bundles = sh_path(&run_dir.join("bundles")),
        main = sh_quote(main_dir(upload)),
    );
    out
}

/// `launch.sh` for a job on this machine (host or container), or for a
/// Windows VM job, which has its own launcher.
pub fn launch_sh(launch: &Launch<'_>) -> String {
    let a = launch.assignment;
    let workspace = match &launch.target {
        Target::Host { workspace } | Target::Container { workspace, .. } => *workspace,
        Target::WindowsVm { .. } => return launch_sh_windows_vm(launch),
    };
    // A try run's bundles are deleted once they are checked out, and when
    // the launcher ends before.
    let drop_bundles = a.upload.as_ref().map_or_else(String::new, |_| {
        format!("; rm -rf {}", sh_path(&launch.run_dir.join("bundles")))
    });
    let mut out = preamble(launch, &drop_bundles);
    match &a.upload {
        Some(upload) => out.push_str(&enter_try_sh(upload, workspace, launch.run_dir)),
        None => out.push_str(&enter_sh(a, workspace, launch.mirror)),
    }
    let inner = launch.run_dir.join("inner.sh");
    match &launch.target {
        Target::Host { .. } => {
            // Read into this shell and deleted at once: the job inherits the
            // values, and no file holds them while it runs.
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
            // A try run mounts the root of its checkouts and starts in the
            // main one.
            let (mount, workdir) = match &a.upload {
                Some(upload) => (
                    "\"$root:/try\"",
                    sh_quote(&format!("/try/{}", main_dir(upload))),
                ),
                None => ("\"$ws:/work\"", "/work".to_owned()),
            };
            match engine {
                Engine::Podman => {
                    let _ = write!(
                        out,
                        "podman image exists {tag} || podman build -t {tag} -f {file} {ctx} || exit 71\n\
                         podman run --rm -it --init --name {name} --cpus {cpus} --memory {memory} \
                         --env-file \"$secret_dir/env\" -v {mount} -v {run}:/ci/run \
                         -v {inputs}:/ci/inputs:ro -w {workdir} {tag} /bin/sh /ci/run/inner.sh\n\
                         exit $?\n",
                    );
                }
                // `container` has no `:ro` on `-v`, so the inputs are writable.
                // The system is started on demand and left running; every
                // container is a VM of its own that `--rm` removes. That VM's
                // clock can run behind the host's, so the job starts through
                // `clock.sh` ([`clock_sh`]), which needs `CAP_SYS_TIME`.
                Engine::Apple => {
                    let _ = write!(
                        out,
                        "container system status >/dev/null 2>&1 || container system start --enable-kernel-install || exit 71\n\
                         container image inspect {tag} >/dev/null 2>&1 || container build -t {tag} -f {file} --cpus {cpus} --memory {memory} {ctx} || exit 71\n\
                         container run --rm -it --init --name {name} --cpus {cpus} --memory {memory} \
                         --cap-add CAP_SYS_TIME --env-file \"$secret_dir/env\" -v {mount} -v {run}:/ci/run \
                         -v {inputs}:/ci/inputs -w {workdir} {tag} /bin/sh /ci/run/clock.sh\n\
                         exit $?\n",
                    );
                }
            }
        }
        Target::WindowsVm { .. } => unreachable!("handled above"),
    }
    out
}

/// `inner.sh`: the job in its environment. The job replaces the script, so
/// the script's exit code is the job's.
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
         exec {run}\n",
    )
}

/// `clock.sh`, the command of an Apple container: sets the VM's clock from
/// the host's, then becomes `inner.sh`.
///
/// The VM keeps a clock of its own that can run behind the host's, while the
/// files a job writes through the mounts are stamped by the host: build tools
/// then see files from the future. A file created in `/ci/run` carries the
/// host's time, which goes to `date -s` with 5 ms added, so the guest runs a
/// little ahead of the host and a file written now never looks newer than the
/// clock. When any step fails the script says so and the job runs on with the
/// clock the VM has. Podman shares the host kernel's clock and has no such
/// script.
pub fn clock_sh() -> &'static str {
    r#": > /ci/run/.clock \
  && t=$(stat -c %.Y /ci/run/.clock) \
  && date -s "@$(awk -v t="$t" 'BEGIN { printf "%.6f", t + 0.005 }')" >/dev/null \
  || printf '[zeughaus-ci] warning: cannot set the container clock from the host; files written now may look newer than the clock\n'
rm -f /ci/run/.clock
exec /bin/sh /ci/run/inner.sh
"#
}

/// The guest directory of a run: `W:\ci\runs\<id>`.
pub fn guest_run_dir(run_id: &str) -> String {
    format!(r"W:\ci\runs\{run_id}")
}

/// The guest workspace of a job: `W:\work\<repo>\<workspace>`.
pub fn guest_workspace(repo: &str, workspace: &str) -> String {
    format!(r"W:\work\{repo}\{workspace}")
}

/// `inner.ps1`: the checkout from the run's bundle (or, for a job that
/// continues its context, the check that an earlier job of the pipeline left
/// the workspace), then the job.
///
/// Its exit code is also written to `<guest run>\rc`: Windows OpenSSH
/// reports 0 for a session with a terminal (`ssh -tt`) whatever the remote
/// command exited with, so the launcher reads the file instead.
pub fn inner_ps1(a: &Assignment) -> String {
    let run = guest_run_dir(&a.run);
    let n = a.number;
    let checkout = match a.checkout {
        Checkout::Continue => format!(
            "$held = \"$(Get-Content -Raw -ErrorAction SilentlyContinue .git\\zci-context)\".Trim()\r\n\
             if ($held -ne '{n}') {{ Write-Host \"[zeughaus-ci] context workspace $env:CI_WORKSPACE no longer holds pipeline #{n}\"; Finish 70 }}\r\n"
        ),
        Checkout::Clean | Checkout::Wipe => format!(
            "if (-not (Test-Path .git)) {{ git init -q; if ($LASTEXITCODE -ne 0) {{ Finish 70 }} }}\r\n\
             git fetch -q {bundle} '+refs/ci/{n}:refs/ci/head'\r\n\
             if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n\
             git checkout -q --force --detach refs/ci/head\r\n\
             if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n\
             git clean -ffdxq{excludes}\r\n\
             if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n\
             Set-Content -NoNewline -Path .git\\zci-context -Value '{n}'\r\n\
             if (-not $?) {{ Write-Host '[zeughaus-ci] checkout failed'; Finish 70 }}\r\n",
            bundle = ps_quote(&format!(r"{run}\src.bundle")),
            excludes = clean_excludes(kept(a), ps_quote),
        ),
    };
    format!(
        "$ErrorActionPreference = 'Stop'\r\n\
         function Finish([int]$code) {{ Set-Content -Path {rc} -Value $code -Encoding ascii; exit $code }}\r\n\
         . {env}\r\n\
         New-Item -ItemType Directory -Force $env:CI_WORKSPACE, $env:CI_OUTPUT | Out-Null\r\n\
         Set-Location $env:CI_WORKSPACE\r\n\
         $ErrorActionPreference = 'Continue'\r\n\
         {checkout}\
         New-Item .git\\zci-last-used -ItemType File -Force | Out-Null\r\n\
         & powershell -NoLogo -NoProfile -ExecutionPolicy Bypass -File {file}\r\n\
         Finish $LASTEXITCODE\r\n",
        rc = ps_quote(&format!(r"{run}\rc")),
        env = ps_quote(&format!(r"{run}\env.ps1")),
        file = ps_quote(&format!(r"{CI_DIR}\{}", a.file)),
    )
}

/// The PowerShell of a Windows host, by path: the runner may run with a
/// `PATH` that does not name it.
pub fn windows_powershell() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe")
}

/// The checkout of a try run in `launch.ps1`, as [`enter_try_sh`] does it in
/// `launch.sh`: `$ws` is the root of the run's working trees on entry and the
/// main checkout on exit.
fn enter_try_ps1(upload: &Upload) -> String {
    let mut out = String::from(
        "$root = $ws\r\n\
         $bundles = Join-Path $env:ZEUGHAUS_RUN_DIR 'bundles'\r\n",
    );
    for (index, dir) in upload.dirs.iter().enumerate() {
        let check = format!(
            "if ($LASTEXITCODE -ne 0) {{ Write-Host {}; exit 70 }}",
            ps_quote(&format!("[zeughaus-ci] checkout of {} failed", dir.dir))
        );
        let _ = write!(
            out,
            "$d = Join-Path $root {name}\r\n\
             New-Item -ItemType Directory -Force -Path $d | Out-Null\r\n\
             Set-Location -LiteralPath $d\r\n\
             if (-not (Test-Path -LiteralPath .git)) {{ git init -q; if ($LASTEXITCODE -ne 0) {{ Write-Host \"[zeughaus-ci] cannot enter $d\"; exit 70 }} }}\r\n\
             git fetch -q --no-tags --prune (Join-Path $bundles '{index}.bundle') '+refs/zeughaus-try/*:refs/zeughaus-try/*'\r\n\
             {check}\r\n\
             git checkout -q --force --detach {sha}\r\n\
             {check}\r\n\
             git clean -ffdq\r\n\
             {check}\r\n",
            name = ps_quote(&dir.dir),
            sha = ps_quote(&dir.sha),
        );
    }
    let _ = write!(
        out,
        "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $bundles\r\n\
         New-Item -ItemType File -Force -Path (Join-Path $root '.zci-last-used') | Out-Null\r\n\
         $ws = Join-Path $root {main}\r\n\
         Set-Location -LiteralPath $ws\r\n",
        main = ps_quote(main_dir(upload)),
    );
    out
}

/// `launch.ps1`: a host job on a Windows runner. The checkout into
/// `workspace` (or, for a job that continues its context, the check that an
/// earlier job of the pipeline left it; for a try run, the checkouts of its
/// upload below it), the secrets read into the session
/// from `env.ps1` and deleted at once, and the job under this runner's
/// `timeout`, whose exit code is the script's. The secret directory goes
/// however the script ends.
pub fn launch_ps1(
    a: &Assignment,
    secret_dir: &Path,
    mirror: &Path,
    workspace: &Path,
    runner_exe: &Path,
) -> String {
    let secs = u64::from(a.timeout_minutes) * 60;
    let path = |p: &Path| ps_quote(&p.to_string_lossy());
    let check = "if ($LASTEXITCODE -ne 0) { Write-Host '[zeughaus-ci] checkout failed'; exit 70 }";
    let n = a.number;
    let (checkout, touch, drop_bundles) = match &a.upload {
        // The bundles go when the launcher ends before they were checked out.
        Some(upload) => (
            enter_try_ps1(upload),
            String::new(),
            "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath (Join-Path $env:ZEUGHAUS_RUN_DIR 'bundles')\r\n"
                .to_owned(),
        ),
        None => {
            let checkout = match a.checkout {
                Checkout::Continue => format!(
                    "$held = \"$(Get-Content -Raw -LiteralPath .git\\zci-context -ErrorAction SilentlyContinue)\".Trim()\r\n\
                     if ($held -ne '{n}') {{ Write-Host \"[zeughaus-ci] context workspace $ws no longer holds pipeline #{n}\"; exit 70 }}\r\n"
                ),
                Checkout::Clean | Checkout::Wipe => format!(
                    "if (-not (Test-Path -LiteralPath .git)) {{ git init -q; if ($LASTEXITCODE -ne 0) {{ Write-Host \"[zeughaus-ci] cannot enter $ws\"; exit 70 }} }}\r\n\
                     git fetch -q --no-tags {mirror} '+refs/ci/{n}:refs/ci/head'\r\n\
                     {check}\r\n\
                     git checkout -q --force --detach refs/ci/head\r\n\
                     {check}\r\n\
                     git clean -ffdxq{excludes}\r\n\
                     {check}\r\n\
                     Set-Content -NoNewline -Path .git\\zci-context -Value '{n}'\r\n\
                     if (-not $?) {{ Write-Host '[zeughaus-ci] checkout failed'; exit 70 }}\r\n",
                    mirror = path(mirror),
                    excludes = clean_excludes(kept(a), ps_quote),
                ),
            };
            let touch = "New-Item -ItemType File -Force -Path .git\\zci-last-used | Out-Null\r\n";
            (checkout, touch.to_owned(), String::new())
        }
    };
    format!(
        "$secretDir = {secret}\r\n\
         try {{\r\n\
         $ws = {ws}\r\n\
         New-Item -ItemType Directory -Force -Path $ws | Out-Null\r\n\
         Set-Location -LiteralPath $ws\r\n\
         {checkout}\
         {touch}\
         $secrets = Join-Path $secretDir 'env.ps1'\r\n\
         . $secrets\r\n\
         Remove-Item -Force -LiteralPath $secrets\r\n\
         New-Item -ItemType Directory -Force -Path $env:CI_OUTPUT | Out-Null\r\n\
         & {exe} timeout {secs} -- {interpreter} -NoLogo -NoProfile -ExecutionPolicy Bypass -File {file}\r\n\
         exit $LASTEXITCODE\r\n\
         }} finally {{\r\n\
         Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $secretDir\r\n\
         {drop_bundles}\
         }}\r\n",
        secret = path(secret_dir),
        ws = path(workspace),
        exe = path(runner_exe),
        interpreter = ps_quote(&a.interpreter),
        file = ps_quote(&format!(r"{CI_DIR}\{}", a.file)),
    )
}

/// `launch.sh` for a Windows VM job: stage the run in the guest over ssh, run
/// it there and copy its output back. The script's exit code is the job's.
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
    // The guest's copy of the run (inputs, secrets, bundle, output) goes
    // with the run however the launcher ends: after the copy-back, on a
    // failure, or when the terminal is closed. Failure to remove is ignored,
    // and a guest that does not answer is not waited for.
    let remove_guest = sh_quote(&format!(
        "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue {guest}"
    ));
    let mut out = preamble(
        launch,
        &format!(
            "; ssh -o ConnectTimeout=15 {host} {remove_guest} </dev/null >/dev/null 2>&1 || true"
        ),
    );
    let _ = write!(
        out,
        "run={run}\n\
         ssh {host} {mkdir} </dev/null || exit 75\n\
         scp -q \"$run/vm/inner.ps1\" \"$run/src.bundle\" \"$secret_dir/env.ps1\" {host}:{dest} || exit 75\n\
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
         fi\n\
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
    );
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zeughaus_link::enlist::{Place, Upload, UploadDir, WhenBusy};

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
            workspace: "build".to_owned(),
            checkout: Checkout::Clean,
            upload: None,
        }
    }

    /// A program the shell side of these tests can run in place of `ssh`,
    /// `date` or the runner's `timeout`: `body` under `#!/bin/sh`.
    fn stand_in(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(path)
            .status()
            .unwrap();
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
    /// timeout program and ends with the job's exit code: no shell follows a
    /// failure, and nothing is written for the runner to look for.
    #[test]
    fn a_failed_host_job_exits_with_its_code() {
        let base = std::env::temp_dir().join(format!("zci-launch-inner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        let run = base.join("run");
        std::fs::create_dir_all(ws.join(CI_DIR)).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(ws.join(CI_DIR).join("build.sh"), "echo ran; exit 3\n").unwrap();
        // Stands in for `zeughaus-runner timeout <secs> -- <program> ..`.
        let stub = base.join("timeout-stub");
        stand_in(&stub, "shift 3\nexec \"$@\"\n");
        let a = assignment(Place::Host);
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(inner_sh(&a, Some(&stub)))
            .env("CI_WORKSPACE", &ws)
            .env("CI_OUTPUT", base.join("out"))
            .env("CI_JOB", "build")
            .env("ZEUGHAUS_RUN_DIR", &run)
            .output()
            .expect("run sh");
        let wrote_code = run.join("code").exists();
        let _ = std::fs::remove_dir_all(&base);
        assert!(String::from_utf8_lossy(&output.stdout).contains("ran"));
        assert_eq!(output.status.code(), Some(3));
        assert!(!wrote_code);
    }

    /// The shell side of a Windows VM job, run for real against stand-ins for
    /// `ssh` and `scp`: the launcher ends with the guest's exit code, copies
    /// the output back only after a success, and opens no second session
    /// after a failure.
    #[test]
    fn a_vm_job_ends_with_the_guests_exit_code_and_opens_no_shell() {
        let base = std::env::temp_dir().join(format!("zci-launch-vm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (bin, run, secret) = (base.join("bin"), base.join("run"), base.join("secret"));
        for dir in [&bin, &run, &secret] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // `ssh` logs its arguments and answers the read of the guest's exit
        // code with `$STUB_GUEST_RC`; `scp` only logs.
        let log = base.join("log");
        for (name, body) in [
            (
                "ssh",
                "printf '%s\\n' \"ssh $*\" >> \"$STUB_LOG\"\n\
                 case \"$*\" in *Get-Content*) printf '%s\\n' \"$STUB_GUEST_RC\" ;; esac\n",
            ),
            ("scp", "printf '%s\\n' \"scp $*\" >> \"$STUB_LOG\"\n"),
        ] {
            stand_in(&bin.join(name), body);
        }
        let a = assignment(Place::Machine {
            name: "win11".to_owned(),
        });
        let script = launch_sh(&Launch {
            assignment: &a,
            run_dir: &run,
            secret_dir: &secret,
            mirror: Path::new("/s/ci/mirrors/demo.git"),
            target: Target::WindowsVm { ssh_host: "win11" },
        });
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let launch = |guest_rc: &str| {
            let _ = std::fs::remove_file(&log);
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .env("PATH", &path)
                .env("STUB_LOG", &log)
                .env("STUB_GUEST_RC", guest_rc)
                .output()
                .expect("run sh");
            (
                output.status.code(),
                std::fs::read_to_string(&log).unwrap_or_default(),
            )
        };

        let (code, calls) = launch("3");
        assert_eq!(code, Some(3), "{calls}");
        assert_eq!(calls.matches("ssh -tt").count(), 1, "{calls}");
        assert!(
            !calls.contains("debug.ps1") && !calls.contains("-NoExit"),
            "{calls}"
        );
        assert!(
            !calls.contains("demo.7.build/out"),
            "a failed run copies nothing back: {calls}"
        );
        assert!(!run.join("code").exists());

        let (code, calls) = launch("0");
        assert_eq!(code, Some(0), "{calls}");
        assert!(
            calls.contains("scp -q -r win11:/W:/ci/runs/demo.7.build/out"),
            "{calls}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A Windows host job ends with its exit code, and nothing follows it.
    #[test]
    fn a_windows_host_launcher_ends_with_the_jobs_exit_code() {
        let ps1 = launch_ps1(
            &assignment(Place::Host),
            Path::new(r"C:\s\secret"),
            Path::new(r"C:\s\mirror"),
            Path::new(r"C:\s\work\demo\linux"),
            Path::new(r"C:\s\runner.exe"),
        );
        assert!(
            ps1.contains("-File '.zeughaus-ci\\build.sh'\r\nexit $LASTEXITCODE\r\n} finally {\r\n"),
            "{ps1}"
        );
        assert!(!ps1.contains("shell"), "{ps1}");
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

    /// Apple's container VM keeps a clock of its own, so its job starts
    /// through `clock.sh` and the capability to set the clock; podman shares
    /// the host kernel's clock and starts `inner.sh` directly.
    #[test]
    fn only_an_apple_container_sets_its_clock_from_the_host() {
        let apple = container_script(Engine::Apple);
        assert!(apple.contains("--cap-add CAP_SYS_TIME"), "{apple}");
        assert!(
            apple.contains(
                "-w /work 'localhost/zci-demo-arch:0123456789abcdef' /bin/sh /ci/run/clock.sh\nexit $?\n"
            ),
            "{apple}"
        );
        assert!(!apple.contains("inner.sh"), "{apple}");

        let podman = container_script(Engine::Podman);
        assert!(
            podman.contains(" /bin/sh /ci/run/inner.sh\nexit $?\n"),
            "{podman}"
        );
        assert!(
            !podman.contains("CAP_SYS_TIME") && !podman.contains("clock.sh"),
            "{podman}"
        );
    }

    /// The clock script run for real against stand-ins for `stat` and `date`:
    /// the clock is set to the host's stamp plus 5 ms and the job starts; when
    /// setting it fails the script says so and the job starts all the same.
    #[test]
    fn the_clock_script_sets_the_clock_a_little_ahead_and_always_starts_the_job() {
        let base = std::env::temp_dir().join(format!("zci-launch-clock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (bin, run) = (base.join("bin"), base.join("run"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        // The host's stamp on the file, and the one call that sets the clock.
        stand_in(&bin.join("stat"), "echo 1700000000.250000000\n");
        stand_in(
            &bin.join("date"),
            "printf '%s\\n' \"$*\" > \"$STUB_DATE_LOG\"\nexit \"$STUB_DATE_EXIT\"\n",
        );
        let started = run.join("started");
        std::fs::write(
            run.join("inner.sh"),
            format!("echo ran > {}\n", sh_path(&started)),
        )
        .unwrap();
        // `/ci/run` is the run directory inside the container.
        let script = run.join("clock.sh");
        std::fs::write(
            &script,
            clock_sh().replace("/ci/run", &run.to_string_lossy()),
        )
        .unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let log = base.join("date.log");
        let clock = |date_exit: &str| {
            let _ = std::fs::remove_file(&started);
            let _ = std::fs::remove_file(&log);
            let output = std::process::Command::new("/bin/sh")
                .arg(&script)
                .env("PATH", &path)
                .env("STUB_DATE_LOG", &log)
                .env("STUB_DATE_EXIT", date_exit)
                .output()
                .expect("run sh");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )
        };

        let (code, out) = clock("0");
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "-s @1700000000.255000\n"
        );
        assert!(!out.contains("warning"), "{out}");
        assert!(started.exists(), "the job runs");
        assert!(!run.join(".clock").exists());

        let (code, out) = clock("1");
        assert_eq!(code, Some(0), "{out}");
        assert!(
            out.contains("warning: cannot set the container clock from the host"),
            "{out}"
        );
        assert!(started.exists(), "the job runs on with the clock it had");
        assert!(!run.join(".clock").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    fn host_launcher(a: &Assignment) -> String {
        let workspace = PathBuf::from("/s/ci/work/demo/linux");
        launch_sh(&Launch {
            assignment: a,
            run_dir: Path::new("/s/runs/4"),
            secret_dir: Path::new("/run/zeughaus-ci/demo.7.build"),
            mirror: Path::new("/s/ci/mirrors/demo.git"),
            target: Target::Host {
                workspace: &workspace,
            },
        })
    }

    /// The three launchers that check a commit out: a host job on a unix
    /// runner, a host job on a Windows runner and a job in a Windows VM.
    fn launchers(a: &Assignment) -> [(&'static str, String); 3] {
        let windows_host = launch_ps1(
            a,
            Path::new(r"C:\s\secret"),
            Path::new(r"C:\s\mirror"),
            Path::new(r"C:\s\work\demo\linux"),
            Path::new(r"C:\s\runner.exe"),
        );
        [
            ("launch.sh", host_launcher(a)),
            ("launch.ps1", windows_host),
            ("inner.ps1", inner_ps1(a)),
        ]
    }

    #[test]
    fn a_job_continuing_its_context_checks_the_workspace_and_fetches_nothing() {
        let mut a = assignment(Place::Host);
        a.checkout = Checkout::Continue;
        for (name, script) in launchers(&a) {
            for forbidden in ["git fetch", "git checkout", "git clean", "git init"] {
                assert!(!script.contains(forbidden), "{name}: {forbidden}");
            }
            assert!(script.contains("zci-context"), "{name}");
            assert!(script.contains("no longer holds pipeline #"), "{name}");
            assert!(script.contains("70"), "{name}");
            assert!(script.contains("zci-last-used"), "{name}");
        }
        let sh = host_launcher(&a);
        assert!(sh.contains(r#"[ "$(cat .git/zci-context 2>/dev/null)" = "7" ]"#));
        assert!(sh.contains("no longer holds pipeline #%s\\n' \"$ws\" 7; exit 70;"));
    }

    #[test]
    fn a_checkout_leaves_the_pipeline_in_the_context_file() {
        for checkout in [Checkout::Clean, Checkout::Wipe] {
            let mut a = assignment(Place::Host);
            a.checkout = checkout;
            let [(_, sh), (_, windows_host), (_, guest)] = launchers(&a);
            assert!(sh.contains("&& printf '%s\\n' 7 > .git/zci-context; }; then"));
            for script in [windows_host, guest] {
                assert!(
                    script.contains(r"Set-Content -NoNewline -Path .git\zci-context -Value '7'")
                );
            }
            for (name, script) in launchers(&a) {
                assert!(script.contains("git fetch"), "{name}");
            }
        }
    }

    #[test]
    fn a_wipe_keeps_no_cache_path_and_a_clean_keeps_them() {
        let mut a = assignment(Place::Host);
        for (name, script) in launchers(&a) {
            assert!(script.contains("git clean -ffdxq -e '/target'"), "{name}");
        }
        a.checkout = Checkout::Wipe;
        for (name, script) in launchers(&a) {
            assert!(script.contains("git clean -ffdxq"), "{name}");
            assert!(!script.contains(" -e "), "{name}");
        }
    }

    /// The shell side of a context, run for real: each job of a pipeline
    /// finds the workspace as the one before left it, a job of another
    /// pipeline is refused, and a clean checkout keeps only the cache paths.
    #[test]
    fn a_context_workspace_passes_from_one_job_to_the_next() {
        let base = std::env::temp_dir().join(format!("zci-launch-context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (mirror, ws) = (base.join("mirror"), base.join("ws"));
        std::fs::create_dir_all(&mirror).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&mirror)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(mirror.join("tracked"), "x").unwrap();
        git(&["add", "tracked"]);
        git(&["commit", "-q", "-m", "c"]);
        git(&["update-ref", "refs/ci/7", "HEAD"]);
        git(&["update-ref", "refs/ci/8", "HEAD"]);

        let enter = |checkout: Checkout, number: u64| {
            let a = Assignment {
                number,
                checkout,
                cache: vec![".ci-cache/".to_owned()],
                ..assignment(Place::Host)
            };
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(enter_sh(&a, &ws, &mirror))
                .output()
                .expect("run sh");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )
        };
        let context = || std::fs::read_to_string(ws.join(".git/zci-context")).ok();

        // The first job checks out and takes the workspace for pipeline 7.
        assert_eq!(enter(Checkout::Clean, 7).0, Some(0));
        assert!(ws.join("tracked").exists());
        assert_eq!(context().as_deref(), Some("7\n"));
        std::fs::create_dir_all(ws.join(".ci-cache")).unwrap();
        std::fs::write(ws.join(".ci-cache/kept"), "").unwrap();
        std::fs::write(ws.join("built"), "").unwrap();

        // The next job of the context finds all of it.
        assert_eq!(enter(Checkout::Continue, 7).0, Some(0));
        assert!(ws.join("built").exists());

        // A job of another pipeline is refused and leaves it alone.
        let (code, out) = enter(Checkout::Continue, 8);
        assert_eq!(code, Some(70));
        assert!(
            out.contains("context workspace") && out.contains("no longer holds pipeline #8"),
            "{out}"
        );
        assert!(ws.join("built").exists());

        // A clean checkout keeps the cache path and nothing else; a wipe not
        // even that.
        assert_eq!(enter(Checkout::Clean, 8).0, Some(0));
        assert!(ws.join(".ci-cache/kept").exists());
        assert!(!ws.join("built").exists());
        assert_eq!(context().as_deref(), Some("8\n"));
        assert_eq!(enter(Checkout::Wipe, 7).0, Some(0));
        assert!(!ws.join(".ci-cache").exists());
        assert_eq!(context().as_deref(), Some("7\n"));

        // A workspace that was removed in between holds no context.
        std::fs::remove_dir_all(&ws).unwrap();
        assert_eq!(enter(Checkout::Continue, 7).0, Some(70));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn try_upload() -> Upload {
        Upload {
            dirs: vec![
                UploadDir {
                    dir: "zeughaus".to_owned(),
                    sha: "a".repeat(40),
                },
                UploadDir {
                    dir: "weida".to_owned(),
                    sha: "b".repeat(40),
                },
            ],
        }
    }

    fn try_assignment(place: Place) -> Assignment {
        Assignment {
            run: "demo.9.try".to_owned(),
            number: 9,
            job: "try".to_owned(),
            file: "zeughaus-try.sh".to_owned(),
            workspace: "zeughaus-try".to_owned(),
            cache: Vec::new(),
            upload: Some(try_upload()),
            ..assignment(place)
        }
    }

    fn try_launcher(a: &Assignment, target: Target<'_>) -> String {
        launch_sh(&Launch {
            assignment: a,
            run_dir: Path::new("/s/runs/4"),
            secret_dir: Path::new("/run/zeughaus-ci/demo.9.try"),
            mirror: Path::new("/s/ci/mirrors/demo.git"),
            target,
        })
    }

    /// A try run has no commit to fetch from the mirror: every checkout of
    /// its upload is fetched from its bundle and checked out in a directory of
    /// its own below the workspace, and the job starts in the first.
    #[test]
    fn a_try_launcher_fetches_every_bundle_and_checks_out_every_commit() {
        let workspace = PathBuf::from("/s/ci/work/demo/zeughaus-try");
        let a = try_assignment(Place::Host);
        let sh = try_launcher(
            &a,
            Target::Host {
                workspace: &workspace,
            },
        );
        assert!(sh.contains("root='/s/ci/work/demo/zeughaus-try'\n"), "{sh}");
        for (index, dir, sha) in [(0, "zeughaus", "a"), (1, "weida", "b")] {
            let sha = sha.repeat(40);
            assert!(sh.contains(&format!("d=\"$root\"/'{dir}'\n")), "{dir}");
            assert!(
                sh.contains(&format!(
                    "git fetch -q --no-tags --prune '/s/runs/4/bundles/{index}.bundle' \
                     '+refs/zeughaus-try/*:refs/zeughaus-try/*' \
                     && git checkout -q --force --detach '{sha}' && git clean -ffdq \
                     || {{ printf '[zeughaus-ci] checkout of %s failed\\n' '{dir}'; exit 70; }}\n"
                )),
                "{dir}"
            );
        }
        assert!(
            sh.find("'zeughaus'").unwrap() < sh.find("'weida'").unwrap(),
            "the main checkout first"
        );
        // The bundles go once they are in, and when the launcher ends before.
        assert!(sh.contains("rm -rf '/s/runs/4/bundles'\ntouch \"$root/.zci-last-used\"\n"));
        assert!(sh.contains("ws=\"$root\"/'zeughaus'\ncd \"$ws\"\n"));
        assert!(sh.contains("cleanup() { rm -rf \"$secret_dir\"; rm -rf '/s/runs/4/bundles'; }"));
        // No commit of the forge, no `-x`: what a build left stays.
        for forbidden in [
            "refs/ci/",
            "git clean -ffdxq",
            "zci-context",
            ".git/zci-last-used",
        ] {
            assert!(!sh.contains(forbidden), "{forbidden}");
        }
        assert!(sh.contains("/bin/sh '/s/runs/4/inner.sh'"));

        // In a container the root is mounted and the job starts in the main
        // checkout, whatever the engine.
        for engine in [Engine::Podman, Engine::Apple] {
            let container = try_assignment(Place::Container {
                image: "trixie".to_owned(),
                tag: "localhost/zci-demo-trixie:0123456789abcdef".to_owned(),
            });
            let sh = try_launcher(
                &container,
                Target::Container {
                    engine,
                    workspace: &workspace,
                    image: "trixie",
                    tag: "localhost/zci-demo-trixie:0123456789abcdef",
                    cpus: 4,
                    memory: "8g",
                },
            );
            assert!(
                sh.contains("-v \"$root:/try\" -v '/s/runs/4':/ci/run"),
                "{sh}"
            );
            assert!(sh.contains("-w '/try/zeughaus' "), "{sh}");
            assert!(!sh.contains(":/work") && !sh.contains("-w /work"), "{sh}");
            assert!(!sh.contains("$ws:"), "{sh}");
            // The image is built from the main checkout's own folder.
            let enters = sh.find("cd \"$ws\"\n").expect("enters the main checkout");
            let builds = sh.find("-f '.zeughaus-ci/trixie.Containerfile' ");
            assert!(builds.is_some_and(|builds| enters < builds), "{sh}");
        }
    }

    #[test]
    fn a_try_launcher_for_windows_checks_out_every_commit_too() {
        let a = Assignment {
            interpreter: "powershell".to_owned(),
            file: "zeughaus-try.ps1".to_owned(),
            ..try_assignment(Place::Host)
        };
        let ps1 = launch_ps1(
            &a,
            Path::new(r"C:\s\secret"),
            Path::new(r"C:\s\mirror"),
            Path::new(r"C:\s\work\demo\zeughaus-try"),
            Path::new(r"C:\s\runner.exe"),
        );
        assert!(
            ps1.contains(r"$ws = 'C:\s\work\demo\zeughaus-try'"),
            "{ps1}"
        );
        assert!(ps1.contains("$root = $ws\r\n"));
        for (index, dir, sha) in [(0, "zeughaus", "a"), (1, "weida", "b")] {
            assert!(
                ps1.contains(&format!("$d = Join-Path $root '{dir}'\r\n")),
                "{dir}"
            );
            assert!(
                ps1.contains(&format!(
                    "git fetch -q --no-tags --prune (Join-Path $bundles '{index}.bundle') \
                     '+refs/zeughaus-try/*:refs/zeughaus-try/*'\r\n"
                )),
                "{dir}"
            );
            assert!(
                ps1.contains(&format!(
                    "git checkout -q --force --detach '{}'\r\n",
                    sha.repeat(40)
                )),
                "{dir}"
            );
            assert!(ps1.contains(&format!(
                "if ($LASTEXITCODE -ne 0) {{ Write-Host '[zeughaus-ci] checkout of {dir} failed'; exit 70 }}"
            )));
        }
        assert!(ps1.contains("git clean -ffdq\r\n"));
        assert!(
            ps1.contains("$ws = Join-Path $root 'zeughaus'\r\nSet-Location -LiteralPath $ws\r\n")
        );
        assert!(
            ps1.contains("New-Item -ItemType File -Force -Path (Join-Path $root '.zci-last-used')")
        );
        // The bundles go once they are in, and when the launcher ends before.
        assert!(ps1.contains(
            "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $bundles\r\n"
        ));
        assert!(ps1.contains(
            "} finally {\r\n\
             Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $secretDir\r\n\
             Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath (Join-Path $env:ZEUGHAUS_RUN_DIR 'bundles')\r\n\
             }\r\n"
        ));
        for forbidden in [
            "refs/ci/",
            "git clean -ffdxq",
            "zci-context",
            r".git\zci-last-used",
        ] {
            assert!(!ps1.contains(forbidden), "{forbidden}");
        }
    }

    /// The shell side of a try run, run for real against bundles as the
    /// agent makes them: each checkout lands in a directory of its own below
    /// the root, a later try finds what an earlier one built there, and a
    /// bundle that does not hold its commit ends the launcher with 70.
    #[test]
    fn a_try_enters_every_working_tree_from_its_bundle() {
        use std::process::Command;

        let base = std::env::temp_dir().join(format!("zci-launch-try-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, run_dir) = (base.join("root"), base.join("run"));
        std::fs::create_dir_all(run_dir.join("bundles")).unwrap();

        let git = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        // One snapshot of `name`: `files` written (None removes one), a
        // commit without a parent of the whole tree, and a bundle of it
        // under `refname` as `bundles/<index>.bundle`.
        let snapshot = |name: &str, files: &[(&str, Option<&str>)], refname: &str, index: u32| {
            let src = base.join("src").join(name);
            std::fs::create_dir_all(&src).unwrap();
            if !src.join(".git").exists() {
                git(&src, &["init", "-q"]);
            }
            for (file, text) in files {
                match text {
                    Some(text) => std::fs::write(src.join(file), text).unwrap(),
                    None => std::fs::remove_file(src.join(file)).unwrap(),
                }
            }
            git(&src, &["add", "-A"]);
            let tree = git(&src, &["write-tree"]);
            let commit = git(&src, &["commit-tree", &tree, "-m", "zeughaus try"]);
            git(&src, &["update-ref", refname, &commit]);
            let bundle = bundle_file(&run_dir, index);
            git(
                &src,
                &["bundle", "create", bundle.to_str().unwrap(), refname],
            );
            git(&src, &["update-ref", "-d", refname]);
            commit
        };
        let upload = |app: &str, lib: &str| Upload {
            dirs: vec![
                UploadDir {
                    dir: "app".to_owned(),
                    sha: app.to_owned(),
                },
                UploadDir {
                    dir: "lib".to_owned(),
                    sha: lib.to_owned(),
                },
            ],
        };
        let enter = |upload: &Upload| {
            let script = format!("{}pwd -P\n", enter_try_sh(upload, &root, &run_dir));
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .output()
                .expect("run sh");
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
            )
        };
        let read = |path: &str| std::fs::read_to_string(root.join(path)).ok();

        // The first try: both trees come out of their bundles.
        let app1 = snapshot(
            "app",
            &[
                ("main.rs", Some("v1")),
                ("old", Some("")),
                (".gitignore", Some("target/\n")),
            ],
            "refs/zeughaus-try/1-1",
            0,
        );
        let lib1 = snapshot("lib", &[("answer", Some("41"))], "refs/zeughaus-try/1-1", 1);
        let (code, out) = enter(&upload(&app1, &lib1));
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(read("app/main.rs").as_deref(), Some("v1"));
        assert_eq!(read("lib/answer").as_deref(), Some("41"));
        assert_eq!(
            out.trim_end(),
            std::fs::canonicalize(root.join("app"))
                .unwrap()
                .to_string_lossy(),
            "the script ends in the main checkout"
        );
        assert!(!run_dir.join("bundles").exists(), "the bundles are gone");
        assert!(root.join(".zci-last-used").exists());

        // A build leaves its output in an ignored directory, and a stray file.
        std::fs::create_dir_all(root.join("app/target")).unwrap();
        std::fs::write(root.join("app/target/out"), "built").unwrap();
        std::fs::write(root.join("app/stray"), "").unwrap();

        // The next try, from other bundles: the snapshot replaces the trees
        // (a deleted file goes, a stray one goes), the build output stays,
        // and the ref of the first bundle is pruned.
        std::fs::create_dir_all(run_dir.join("bundles")).unwrap();
        let app2 = snapshot(
            "app",
            &[("main.rs", Some("v2")), ("old", None)],
            "refs/zeughaus-try/2-1",
            0,
        );
        let lib2 = snapshot("lib", &[("answer", Some("42"))], "refs/zeughaus-try/2-1", 1);
        let (code, out) = enter(&upload(&app2, &lib2));
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(read("app/main.rs").as_deref(), Some("v2"));
        assert_eq!(read("lib/answer").as_deref(), Some("42"));
        assert_eq!(read("app/old"), None);
        assert_eq!(read("app/stray"), None);
        assert_eq!(read("app/target/out").as_deref(), Some("built"));
        let refs = git(&root.join("app"), &["for-each-ref", "--format=%(refname)"]);
        assert_eq!(refs, "refs/zeughaus-try/2-1");

        // A bundle that does not hold the commit ends it with 70, and says
        // which checkout.
        std::fs::create_dir_all(run_dir.join("bundles")).unwrap();
        let lib3 = snapshot("lib", &[("answer", Some("43"))], "refs/zeughaus-try/3-1", 1);
        std::fs::write(bundle_file(&run_dir, 0), "not a bundle").unwrap();
        let (code, out) = enter(&upload(&app2, &lib3));
        assert_eq!(code, Some(70), "{out}");
        assert!(
            out.contains("[zeughaus-ci] checkout of app failed"),
            "{out}"
        );
        assert_eq!(
            read("lib/answer").as_deref(),
            Some("42"),
            "nothing past it ran"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
