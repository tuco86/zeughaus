//! The locale every terminal of this host starts with.
//!
//! Shells and jobs inherit the runner's environment, and a runner started
//! over ssh, by launchd or under a stripped environment has no locale at all,
//! which leaves a shell in the 7-bit `C` locale. [`resolve`] determines the
//! user's locale on the runner's own host -- the process environment when it
//! names one, else `locale.conf` (Linux) or `AppleLocale` (macOS) -- drops
//! every value that is not installed, and guarantees that the character type
//! is UTF-8. [`LocaleEnv::apply`] writes the result into the runner's own
//! environment, which every child inherits.
//!
//! A locale set explicitly in the environment wins; only values that are not
//! installed and a non-UTF-8 `LC_ALL` or character type are overridden.

use std::process::Command;

/// Every locale variable, in the order they are reported.
const KEYS: [&str; 15] = [
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_MESSAGES",
    "LC_PAPER",
    "LC_NAME",
    "LC_ADDRESS",
    "LC_TELEPHONE",
    "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
];

const LANG: usize = 0;
const LANGUAGE: usize = 1;
const LC_ALL: usize = 2;
const LC_CTYPE: usize = 3;

/// The UTF-8 locale assumed when `locale -a` gives no evidence. macOS has no
/// `C.UTF-8` before 13; glibc and musl always have it.
const DEFAULT_UTF8: &str = if cfg!(target_os = "macos") {
    "en_US.UTF-8"
} else {
    "C.UTF-8"
};

/// What [`resolve`] decided, and the changes that make the process
/// environment say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleEnv {
    /// "environment", the locale.conf path read, "AppleLocale <id>", or "fallback".
    pub source: String,
    pub set: Vec<(String, String)>,
    pub remove: Vec<String>,
    /// (key, value, reason) of every value that was not used.
    pub refused: Vec<(String, String, &'static str)>,
    /// Every locale variable a terminal will see, in KEYS order.
    pub effective: Vec<(String, String)>,
}

impl LocaleEnv {
    /// Writes the resolved locale into this process's environment.
    ///
    /// # Safety
    /// No other thread may exist: this calls `std::env::set_var`/`remove_var`.
    pub unsafe fn apply(&self) {
        for key in &self.remove {
            // SAFETY: the caller guarantees this process has one thread.
            unsafe { std::env::remove_var(key) };
        }
        for (key, value) in &self.set {
            // SAFETY: as above.
            unsafe { std::env::set_var(key, value) };
        }
    }
}

/// Determines the locale for this host's terminals from the process
/// environment, the configured locale and the installed locales.
///
/// Runs `locale -a` (and `defaults` on macOS); neither starts a thread in this
/// process, so the result can still be applied afterwards.
pub fn resolve() -> LocaleEnv {
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            KEYS.contains(&key.as_str())
                .then(|| (key, value.to_string_lossy().into_owned()))
        })
        .collect();
    let installed = installed_locales();
    let configured = configured(&installed);
    plan(&env, configured, &installed)
}

/// `locale -a`, or nothing when it cannot be run.
fn installed_locales() -> Vec<String> {
    match Command::new("locale").arg("-a").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// The first readable `locale.conf`, per user before system-wide.
#[cfg(not(target_os = "macos"))]
fn configured(_installed: &[String]) -> Option<(String, Vec<(String, String)>)> {
    use std::path::PathBuf;

    let mut paths = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(dir).join("locale.conf"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        paths.push(PathBuf::from(home).join(".config/locale.conf"));
    }
    paths.push(PathBuf::from("/etc/locale.conf"));
    paths.push(PathBuf::from("/etc/default/locale"));
    paths.into_iter().find_map(|path| {
        let text = std::fs::read_to_string(&path).ok()?;
        Some((path.display().to_string(), parse_locale_conf(&text)))
    })
}

/// The user's region setting, mapped onto the installed locales.
#[cfg(target_os = "macos")]
fn configured(installed: &[String]) -> Option<(String, Vec<(String, String)>)> {
    let out = Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if id.is_empty() {
        return None;
    }
    let vars = from_apple_locale(&id, installed);
    Some((format!("AppleLocale {id}"), vars))
}

fn key_index(key: &str) -> Option<usize> {
    KEYS.iter().position(|k| *k == key)
}

/// A locale name with its codeset spelled the way `locale -a` compares:
/// `de_DE.UTF-8` and `de_DE.utf8` are the same locale.
fn normalize(name: &str) -> String {
    let (body, modifier) = match name.split_once('@') {
        Some((body, modifier)) => (body, Some(modifier)),
        None => (name, None),
    };
    let mut out = match body.split_once('.') {
        Some((lang_territory, codeset)) => {
            let codeset: String = codeset
                .chars()
                .filter(|c| *c != '-')
                .map(|c| c.to_ascii_lowercase())
                .collect();
            format!("{lang_territory}.{codeset}")
        }
        None => body.to_owned(),
    };
    if let Some(modifier) = modifier {
        out.push('@');
        out.push_str(modifier);
    }
    out
}

/// The part before the codeset and modifier: `de_DE` of `de_DE.UTF-8@euro`.
fn language_territory(name: &str) -> &str {
    name.split(['.', '@']).next().unwrap_or(name)
}

fn is_utf8(name: &str) -> bool {
    if name.eq_ignore_ascii_case("UTF-8") || name.eq_ignore_ascii_case("utf8") {
        return true;
    }
    let normalized = normalize(name);
    let body = normalized.split('@').next().unwrap_or_default();
    body.split_once('.')
        .is_some_and(|(_, codeset)| codeset == "utf8")
}

/// `want` as `locale -a` spells it, when it is installed.
fn installed_spelling(want: &str, installed: &[String]) -> Option<String> {
    let want = normalize(want);
    installed
        .iter()
        .find(|name| normalize(name) == want)
        .cloned()
}

/// Whether libc will accept `value`. An empty list means `locale -a` failed,
/// which is no evidence to refuse on.
fn is_valid(value: &str, installed: &[String]) -> bool {
    installed.is_empty()
        || value == "C"
        || value == "POSIX"
        || installed_spelling(value, installed).is_some()
}

/// The locale variables of a `locale.conf` / `/etc/default/locale`.
#[cfg(any(not(target_os = "macos"), test))]
fn parse_locale_conf(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            let value = ['"', '\'']
                .iter()
                .find_map(|q| {
                    value
                        .strip_prefix(*q)
                        .and_then(|rest| rest.strip_suffix(*q))
                })
                .unwrap_or(value);
            (KEYS.contains(&key) && !value.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// The locale variables for a macOS `AppleLocale` id such as
/// `en_001@rg=dezzzz` (English, formats of the region Germany): `LANG` for the
/// language, and the categories macOS libc implements for the region when it
/// needs a different locale.
#[cfg(any(target_os = "macos", test))]
fn from_apple_locale(id: &str, installed: &[String]) -> Vec<(String, String)> {
    let (base, keywords) = match id.split_once('@') {
        Some((base, keywords)) => (base, Some(keywords)),
        None => (id, None),
    };
    let (lang, territory) = match base.split_once('_') {
        Some((lang, territory)) => (lang, Some(territory)),
        None => (base, None),
    };
    let lang = lang.to_ascii_lowercase();
    let two_letters = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic());
    let territory = territory
        .filter(|t| two_letters(t))
        .map(str::to_ascii_uppercase);
    let region = keywords
        .and_then(|k| k.split(';').find_map(|kv| kv.strip_prefix("rg=")))
        .and_then(|rg| rg.get(..2))
        .filter(|rg| two_letters(rg))
        .map(str::to_ascii_uppercase);

    let mut candidates = Vec::new();
    if let Some(territory) = &territory {
        candidates.push(format!("{lang}_{territory}.UTF-8"));
    }
    if let Some(region) = &region {
        candidates.push(format!("{lang}_{region}.UTF-8"));
    }
    candidates.push(format!("{lang}_{}.UTF-8", lang.to_ascii_uppercase()));
    if lang == "en" {
        candidates.push("en_US.UTF-8".to_owned());
    }
    let lang_locale = candidates
        .iter()
        .find_map(|c| installed_spelling(c, installed));

    let format_locale = region.or(territory).and_then(|r| {
        installed_spelling(&format!("{lang}_{r}.UTF-8"), installed)
            .or_else(|| {
                installed_spelling(&format!("{}_{r}.UTF-8", r.to_ascii_lowercase()), installed)
            })
            .or_else(|| {
                let suffix = format!("_{r}");
                installed
                    .iter()
                    .find(|name| language_territory(name).ends_with(&suffix) && is_utf8(name))
                    .cloned()
            })
    });

    let mut vars = Vec::new();
    if let Some(lang_locale) = &lang_locale {
        vars.push(("LANG".to_owned(), lang_locale.clone()));
    }
    if let Some(format_locale) = format_locale
        && lang_locale.as_ref() != Some(&format_locale)
    {
        for key in ["LC_NUMERIC", "LC_TIME", "LC_MONETARY"] {
            vars.push((key.to_owned(), format_locale.clone()));
        }
    }
    vars
}

/// The decision [`resolve`] makes, on inputs instead of the process.
fn plan(
    env: &[(String, String)],
    configured: Option<(String, Vec<(String, String)>)>,
    installed: &[String],
) -> LocaleEnv {
    let mut from_env: [Option<String>; KEYS.len()] = Default::default();
    for (key, value) in env {
        if let Some(i) = key_index(key)
            && !value.is_empty()
        {
            from_env[i] = Some(value.clone());
        }
    }

    // Source: the environment when it names any locale, else the configured one.
    let env_names_locale = from_env
        .iter()
        .enumerate()
        .any(|(i, v)| i != LANGUAGE && v.is_some());
    let mut map: [Option<String>; KEYS.len()] = Default::default();
    let source = if env_names_locale {
        map.clone_from(&from_env);
        "environment".to_owned()
    } else if let Some((name, vars)) = configured {
        for (key, value) in vars {
            if let Some(i) = key_index(&key)
                && !value.is_empty()
            {
                map[i] = Some(value);
            }
        }
        name
    } else {
        "fallback".to_owned()
    };
    if from_env[LANGUAGE].is_some() {
        map[LANGUAGE].clone_from(&from_env[LANGUAGE]);
    }

    let mut refused = Vec::new();

    // Validation: a locale libc does not have silently becomes C.
    for (i, slot) in map.iter_mut().enumerate() {
        if i == LANGUAGE {
            continue;
        }
        if let Some(value) = slot.take_if(|v| !is_valid(v, installed)) {
            refused.push((KEYS[i].to_owned(), value, "not installed"));
        }
    }

    // LC_ALL overrides every category, so a non-UTF-8 one cannot stay; its
    // choice survives as the default for the categories not set otherwise.
    if let Some(all) = map[LC_ALL].take_if(|v| !is_utf8(v)) {
        if map[LANG].is_none() {
            map[LANG] = Some(all.clone());
        }
        refused.push(("LC_ALL".to_owned(), all, "not UTF-8, moved to LANG"));
    }

    let fallback = installed_spelling("C.UTF-8", installed)
        .or_else(|| installed_spelling("en_US.UTF-8", installed))
        .or_else(|| installed.iter().find(|name| is_utf8(name)).cloned())
        .or_else(|| installed.is_empty().then(|| DEFAULT_UTF8.to_owned()));

    if map[LANG].is_none() {
        map[LANG].clone_from(&fallback);
    }

    // The character type decides how the terminal's bytes are read; it must
    // be UTF-8 whatever the other categories say.
    let ctype = map[LC_ALL]
        .as_ref()
        .or(map[LC_CTYPE].as_ref())
        .or(map[LANG].as_ref())
        .cloned();
    if !ctype.as_deref().is_some_and(is_utf8) {
        let utf8 = ctype
            .as_deref()
            .and_then(|c| {
                installed_spelling(&format!("{}.UTF-8", language_territory(c)), installed)
            })
            .or_else(|| fallback.clone());
        match utf8 {
            Some(utf8) => map[LC_CTYPE] = Some(utf8),
            None => refused.push((
                "LC_CTYPE".to_owned(),
                ctype.unwrap_or_default(),
                "no UTF-8 locale installed",
            )),
        }
    }

    let mut set = Vec::new();
    let mut remove = Vec::new();
    let mut effective = Vec::new();
    for (i, key) in KEYS.iter().enumerate() {
        match &map[i] {
            Some(value) => {
                if from_env[i].as_ref() != Some(value) {
                    set.push(((*key).to_owned(), value.clone()));
                }
                effective.push(((*key).to_owned(), value.clone()));
            }
            None if from_env[i].is_some() => remove.push((*key).to_owned()),
            None => {}
        }
    }

    LocaleEnv {
        source,
        set,
        remove,
        refused,
        effective,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: [&str; 5] = ["C", "C.utf8", "de_DE.utf8", "en_US.utf8", "POSIX"];
    const MAC: [&str; 7] = [
        "C",
        "C.UTF-8",
        "POSIX",
        "de_DE",
        "de_DE.UTF-8",
        "en_US",
        "en_US.UTF-8",
    ];

    fn owned(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn a_valid_environment_is_left_alone() {
        let env = pairs(&[("LANG", "en_US.UTF-8"), ("LC_TIME", "de_DE.UTF-8")]);
        let out = plan(&env, None, &owned(&LINUX));
        assert_eq!(out.source, "environment");
        assert!(out.set.is_empty(), "{out:?}");
        assert!(out.remove.is_empty(), "{out:?}");
        assert_eq!(out.effective, env);
    }

    #[test]
    fn an_empty_environment_takes_the_configured_locale() {
        let conf = parse_locale_conf("# c\nexport LANG=\"en_US.UTF-8\"\nLC_TIME='de_DE.UTF-8'\n");
        let out = plan(
            &[],
            Some(("/etc/locale.conf".to_owned(), conf)),
            &owned(&LINUX),
        );
        assert_eq!(out.source, "/etc/locale.conf");
        assert_eq!(
            out.set,
            pairs(&[("LANG", "en_US.UTF-8"), ("LC_TIME", "de_DE.UTF-8")])
        );
        assert!(out.remove.is_empty());
    }

    #[test]
    fn an_uninstalled_value_is_removed() {
        let env = pairs(&[("LANG", "de_DE.UTF-8"), ("LC_CTYPE", "UTF-8")]);
        let out = plan(&env, None, &owned(&LINUX));
        assert_eq!(out.remove, ["LC_CTYPE"]);
        assert!(out.set.is_empty(), "{out:?}");
        assert_eq!(
            out.refused,
            [("LC_CTYPE".to_owned(), "UTF-8".to_owned(), "not installed")]
        );
    }

    #[test]
    fn a_non_utf8_lc_all_moves_to_lang_and_ctype_becomes_utf8() {
        let env = pairs(&[("LC_ALL", "C")]);
        let out = plan(&env, None, &owned(&LINUX));
        assert_eq!(out.remove, ["LC_ALL"]);
        assert_eq!(out.set, pairs(&[("LANG", "C"), ("LC_CTYPE", "C.utf8")]));
        assert_eq!(out.refused[0].2, "not UTF-8, moved to LANG");
    }

    #[test]
    fn nothing_configured_falls_back_to_an_installed_utf8_locale() {
        let out = plan(&[], None, &owned(&LINUX));
        assert_eq!(out.source, "fallback");
        assert_eq!(out.set, pairs(&[("LANG", "C.utf8")]));
    }

    #[test]
    fn without_locale_list_nothing_is_refused() {
        let out = plan(&[], None, &[]);
        assert_eq!(out.set, pairs(&[("LANG", DEFAULT_UTF8)]));

        let env = pairs(&[("LANG", "xx_YY.UTF-8")]);
        let out = plan(&env, None, &[]);
        assert!(out.refused.is_empty(), "{out:?}");
        assert_eq!(out.effective, env);
    }

    #[test]
    fn apple_locale_splits_language_and_region() {
        assert_eq!(
            from_apple_locale("en_001@rg=dezzzz", &owned(&MAC)),
            pairs(&[
                ("LANG", "en_US.UTF-8"),
                ("LC_NUMERIC", "de_DE.UTF-8"),
                ("LC_TIME", "de_DE.UTF-8"),
                ("LC_MONETARY", "de_DE.UTF-8"),
            ])
        );
        assert_eq!(
            from_apple_locale("de_DE", &owned(&MAC)),
            pairs(&[("LANG", "de_DE.UTF-8")])
        );
    }
}
