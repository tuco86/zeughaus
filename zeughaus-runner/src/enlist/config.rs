//! `<state-dir>/enlist.toml`: the CI this runner enlists into and what it
//! offers there.
//!
//! The file is the runner's own business: the CI knows a runner by the
//! fingerprint it pins in its `ci.toml`, and what a job may use here (a
//! container engine, a VM, the busy measurement) is whatever this file says.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::names::valid_name;

pub const CONFIG_FILE: &str = "enlist.toml";

fn default_budget_gb() -> u64 {
    50
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnlistConfig {
    /// The CI's pinned URL: `weida://sha256:<fp>@host:port/`.
    pub ci: String,
    /// The name this runner enlists under; the CI refuses a name its config
    /// does not pair with this runner's fingerprint.
    pub name: String,
    /// `CI_CPUS` of host jobs; the machine's parallelism by default.
    pub cpus: Option<u32>,
    /// Size limit of the host workspaces under `ci/work`.
    #[serde(default = "default_budget_gb")]
    pub budget_gb: u64,
    /// Present: this runner runs jobs in rootless podman containers.
    pub containers: Option<ContainerConfig>,
    /// Present: this runner measures the GPU and holds jobs while it is busy.
    pub busy: Option<BusyConfig>,
    /// The machines behind this runner, by the name a job's `machine` key
    /// uses.
    #[serde(default)]
    pub machines: BTreeMap<String, MachineConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BusyConfig {
    /// `nvidia-smi` utilization.gpu threshold in percent.
    pub gpu_percent: u32,
    /// Mean over this window at or above the threshold means busy.
    pub busy_after_seconds: u64,
    /// Mean over this window below the threshold means free.
    pub free_after_seconds: u64,
}

impl Default for BusyConfig {
    fn default() -> Self {
        Self {
            gpu_percent: 50,
            busy_after_seconds: 60,
            free_after_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ContainerConfig {
    pub cpus: u32,
    pub memory: String,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            cpus: 12,
            memory: "24g".into(),
        }
    }
}

fn default_machine_cpus() -> u32 {
    8
}

fn default_machine_memory() -> String {
    "12G".into()
}

fn default_cache_disk() -> String {
    "100G".into()
}

fn default_idle_minutes() -> u64 {
    20
}

/// A machine a job's `machine = "<name>"` runs on; `kind` says which.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MachineConfig {
    WindowsVm(VmConfig),
}

/// A Windows VM this runner boots on demand.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    pub script: PathBuf,
    pub dir: PathBuf,
    pub ssh_host: String,
    #[serde(default = "default_machine_cpus")]
    pub cpus: u32,
    #[serde(default = "default_machine_memory")]
    pub memory: String,
    #[serde(default = "default_cache_disk")]
    pub cache_disk: String,
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u64,
}

impl EnlistConfig {
    pub fn parse(text: &str) -> Result<EnlistConfig, String> {
        let config: EnlistConfig =
            toml::from_str(text).map_err(|e| format!("{CONFIG_FILE}: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err(format!(
                "{CONFIG_FILE}: name `{}` must match [a-z0-9][a-z0-9-]*",
                self.name
            ));
        }
        let addr = weida::EndpointAddr::parse(&self.ci)
            .map_err(|e| format!("{CONFIG_FILE}: ci `{}`: {e}", self.ci))?;
        if addr.peer.is_none() {
            return Err(format!(
                "{CONFIG_FILE}: ci `{}` names no fingerprint \
                 (weida://sha256:<fp>@host:port/): a CI that is not pinned could be anyone",
                self.ci
            ));
        }
        if self.cpus == Some(0) {
            return Err(format!("{CONFIG_FILE}: `cpus` must be at least 1"));
        }
        if self.containers.as_ref().is_some_and(|c| c.cpus == 0) {
            return Err(format!(
                "{CONFIG_FILE}: [containers] `cpus` must be at least 1"
            ));
        }
        // A Windows runner runs its jobs on its own host: the container
        // engines and the VMs a runner drives are unix tools.
        if cfg!(not(unix)) && self.containers.is_some() {
            return Err(format!("{CONFIG_FILE}: [containers] needs a unix host"));
        }
        if cfg!(not(unix)) && !self.machines.is_empty() {
            return Err(format!("{CONFIG_FILE}: [machines] needs a unix host"));
        }
        for (name, machine) in &self.machines {
            if !valid_name(name) {
                return Err(format!(
                    "{CONFIG_FILE}: machine name `{name}` must match [a-z0-9][a-z0-9-]*"
                ));
            }
            let MachineConfig::WindowsVm(vm) = machine;
            if vm.ssh_host.is_empty() {
                return Err(format!(
                    "{CONFIG_FILE}: machine `{name}`: `ssh_host` must not be empty"
                ));
            }
            if vm.cpus == 0 {
                return Err(format!(
                    "{CONFIG_FILE}: machine `{name}`: `cpus` must be at least 1"
                ));
            }
        }
        Ok(())
    }

    /// The CI's URL for `path`.
    pub fn ci_url(&self, path: &str) -> Result<String, String> {
        let mut addr = weida::EndpointAddr::parse(&self.ci).map_err(|e| e.to_string())?;
        addr.path = path.to_owned();
        Ok(addr.to_string())
    }
}

/// `Ok(None)` when there is no `enlist.toml`: this runner does not enlist.
pub fn load(state_dir: &Path) -> Result<Option<EnlistConfig>, String> {
    let path = state_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    EnlistConfig::parse(&text).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

    fn url() -> String {
        format!("weida://{FP}@10.8.0.1:7444/")
    }

    #[test]
    fn a_minimal_file_enlists_without_offering_anything() {
        let config =
            EnlistConfig::parse(&format!("ci = \"{}\"\nname = \"sadala\"\n", url())).unwrap();
        assert_eq!(config.name, "sadala");
        assert_eq!(config.budget_gb, 50);
        assert!(config.containers.is_none() && config.busy.is_none());
        assert!(config.machines.is_empty());
        assert_eq!(
            config.ci_url("/enlist").unwrap(),
            format!("weida://{FP}@10.8.0.1:7444/enlist")
        );
    }

    #[test]
    fn a_full_file_reads_every_section() {
        let text = format!(
            "ci = \"{}\"\nname = \"tuco-pc\"\ncpus = 16\n\
             [containers]\ncpus = 12\nmemory = \"24g\"\n\
             [busy]\ngpu_percent = 70\n\
             [machines.win11]\nkind = \"windows-vm\"\nscript = \"/s/vm.sh\"\n\
             dir = \"/d\"\nssh_host = \"win11-ci\"\ncpus = 8\n",
            url()
        );
        let config = EnlistConfig::parse(&text).unwrap();
        assert_eq!(config.cpus, Some(16));
        assert_eq!(config.containers.unwrap().cpus, 12);
        let busy = config.busy.unwrap();
        assert_eq!((busy.gpu_percent, busy.busy_after_seconds), (70, 60));
        let MachineConfig::WindowsVm(vm) = &config.machines["win11"];
        assert_eq!((vm.ssh_host.as_str(), vm.idle_minutes), ("win11-ci", 20));
    }

    #[test]
    fn an_unpinned_ci_is_refused() {
        let text = "ci = \"weida://10.8.0.1:7444/\"\nname = \"x\"\n";
        assert!(
            EnlistConfig::parse(text)
                .unwrap_err()
                .contains("fingerprint")
        );
    }

    #[test]
    fn unknown_keys_and_bad_names_are_refused() {
        let base = format!("ci = \"{}\"\n", url());
        assert!(EnlistConfig::parse(&format!("{base}name = \"x\"\nbogus = 1\n")).is_err());
        assert!(EnlistConfig::parse(&format!("{base}name = \"Tuco PC\"\n")).is_err());
        assert!(
            EnlistConfig::parse(&format!(
                "{base}name = \"x\"\n[machines.\"A b\"]\nkind = \"windows-vm\"\n\
                 script = \"/s\"\ndir = \"/d\"\nssh_host = \"h\"\n"
            ))
            .is_err()
        );
    }
}
