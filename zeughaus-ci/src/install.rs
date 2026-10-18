//! `install ci`: puts an unpacked release on this host as the CI's user
//! service and leaves `install.toml` and `update.toml` so the CI updates
//! itself from then on.
//!
//! ```text
//! zeughaus-ci [--state-dir D] install ci [--channel C] [--from DIR] [--no-service]
//! ```
//!
//! The binary goes to `~/bin`, the unit from the release's `share/` to the
//! user's systemd directory, and the unit is started or restarted. Every
//! build the updater installs writes the unit again ([`refresh`]).

use std::path::{Path, PathBuf};

use zeughaus_update::Product;
use zeughaus_update::host::{self, FileStep, InstallConfig};

const USAGE: &str =
    "usage: zeughaus-ci [--state-dir D] install ci [--channel C] [--from DIR] [--no-service]";

const UNIT: &str = "zeughaus-ci.service";

/// The one profile: its name in `install.toml`.
const PROFILE: &str = "ci";

fn unit_path(home: &Path) -> PathBuf {
    host::config_home(home).join("systemd/user").join(UNIT)
}

fn files(from: &Path, home: &Path, bin: &Path) -> Result<Vec<host::Rendered>, String> {
    let steps = [FileStep {
        share: "deploy/ci/zeughaus-ci.service",
        to: unit_path(home),
        mode: 0o644,
    }];
    let bin = bin.display().to_string();
    host::render_files(from, &steps, &[("@BINDIR@", &bin)])
}

/// `install`, from the command line: answers the line to print.
pub fn run(args: &[String], state_dir: &Path) -> Result<String, String> {
    let (profile, rest) = args.split_first().ok_or_else(|| USAGE.to_owned())?;
    if profile != PROFILE {
        return Err(USAGE.to_owned());
    }
    let mut channel = "dev".to_owned();
    let mut from = None;
    let mut no_service = false;
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        match flag.as_str() {
            "--no-service" => no_service = true,
            "--channel" | "--from" => {
                let value = rest
                    .next()
                    .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
                if flag == "--channel" {
                    channel = value.clone();
                } else {
                    from = Some(PathBuf::from(value));
                }
            }
            _ => return Err(format!("unknown flag {flag}\n{USAGE}")),
        }
    }
    let from = match from {
        Some(from) => from,
        None => std::env::current_exe()
            .map_err(|e| format!("this executable cannot be located: {e}"))?
            .parent()
            .map(Path::to_path_buf)
            .ok_or("this executable has no directory")?,
    };
    let home = host::home_dir()?;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("cannot create {}: {e}", bin.display()))?;

    let files = files(&from, &home, &bin)?;
    let build = host::release_build(&from, Product::Ci)?;
    let binary = Product::Ci.file_name();
    host::install_binary(&from.join(&binary), &bin.join(&binary))?;
    host::write_rendered(&files)?;
    let config = InstallConfig {
        profile: PROFILE.to_owned(),
        bin_dir: bin,
        feed_addr: None,
    };
    host::record(state_dir, &config, &channel)?;
    if !no_service {
        let done = host::start_or(UNIT, "restart")?;
        eprintln!("[install] {done} {UNIT}");
    }
    Ok(format!("[install] {PROFILE} from build {build}"))
}

/// Writes the unit from the release at `from` again: what a new build does
/// once, through the updater's refresh hook. A changed unit applies at the
/// next start of the service.
pub fn refresh(state_dir: &Path, from: &Path) {
    let config = match host::load(state_dir) {
        Ok(Some(config)) if config.profile == PROFILE => config,
        Ok(Some(config)) => {
            eprintln!(
                "[install] {}: {:?} is not the CI's profile",
                host::INSTALL_FILE,
                config.profile
            );
            return;
        }
        Ok(None) => return,
        Err(e) => {
            eprintln!("[install] {e}");
            return;
        }
    };
    let changed = host::home_dir()
        .and_then(|home| files(from, &home, &config.bin_dir))
        .and_then(|files| host::write_rendered(&files));
    match changed {
        Ok(changed) if changed.is_empty() => {}
        Ok(_) => {
            if let Err(e) = host::systemctl(&["daemon-reload"]) {
                eprintln!("[install] {e}");
            }
            eprintln!("[install] unit changed; it applies at the next start of the service");
        }
        Err(e) => eprintln!("[install] {e}"),
    }
}
