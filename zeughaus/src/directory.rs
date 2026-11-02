//! Runners by name: the addresses the CIs announce for each runner, and the
//! resolver that dials `<name>.runner.zeughaus` through them.
//!
//! A CI's overview lists every runner it knows with the `ip:port` pairs its
//! listener answers at. The editor publishes that table here and dials a
//! runner it only knows from a CI as
//! `weida://<fingerprint>@<name>.runner.zeughaus:<port>/`; weida asks
//! [`DirectoryResolver`] for the name and tries the candidates in order.
//! Every other name goes to the system resolver.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, RwLock};

use weida::{Resolved, Resolver, SystemResolver};
use weida_runtime::Exec;

/// The pseudo domain under which runners are dialled by name.
pub const RUNNER_DOMAIN: &str = "runner.zeughaus";

static DIRECTORY: LazyLock<RwLock<BTreeMap<String, Vec<SocketAddr>>>> =
    LazyLock::new(Default::default);

/// Replaces the table: runner name -> its candidate addresses, best first.
pub fn publish(entries: BTreeMap<String, Vec<SocketAddr>>) {
    *DIRECTORY.write().unwrap_or_else(|e| e.into_inner()) = entries;
}

/// The addresses of `<name>.runner.zeughaus`; `None` for another host or a
/// runner without addresses.
pub fn lookup(host: &str) -> Option<Vec<SocketAddr>> {
    let table = DIRECTORY.read().unwrap_or_else(|e| e.into_inner());
    lookup_in(&table, host)
}

fn lookup_in(table: &BTreeMap<String, Vec<SocketAddr>>, host: &str) -> Option<Vec<SocketAddr>> {
    let name = host.strip_suffix(RUNNER_DOMAIN)?.strip_suffix('.')?;
    table.get(name).filter(|a| !a.is_empty()).cloned()
}

/// Answers `*.runner.zeughaus` from the directory, everything else from the
/// system resolver.
#[derive(Debug)]
pub struct DirectoryResolver;

impl Resolver for DirectoryResolver {
    fn resolve<'a>(
        &'a self,
        exec: &'a Exec,
        name: &'a str,
        port: Option<u16>,
        max_addresses: usize,
    ) -> Resolved<'a> {
        if !name.ends_with(&format!(".{RUNNER_DOMAIN}")) {
            return SystemResolver.resolve(exec, name, port, max_addresses);
        }
        let found = lookup(name);
        Box::pin(async move {
            match found {
                Some(mut addresses) => {
                    addresses.truncate(max_addresses);
                    Ok(addresses)
                }
                None => Err(weida::Error::InvalidAddress(format!(
                    "{name}: no address announced by a CI"
                ))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_runner_names_with_addresses_are_found() {
        let a: SocketAddr = "10.8.0.7:7443".parse().unwrap();
        let table = BTreeMap::from([
            ("atik".to_owned(), vec![a]),
            ("bare".to_owned(), Vec::new()),
        ]);
        assert_eq!(lookup_in(&table, "atik.runner.zeughaus"), Some(vec![a]));
        assert_eq!(lookup_in(&table, "bare.runner.zeughaus"), None);
        assert_eq!(lookup_in(&table, "other.runner.zeughaus"), None);
        assert_eq!(lookup_in(&table, "atik"), None);
        assert_eq!(lookup_in(&table, "atikrunner.zeughaus"), None);
        assert_eq!(lookup_in(&table, "atik.example.org"), None);
    }
}
