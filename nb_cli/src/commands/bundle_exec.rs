//! `nb bundle exec`, `sh` and `env`: a command run in the environment
//! Homebrew sets up for building against a Brewfile's formulae.
//!
//! Homebrew builds that environment with superenv, whose compiler shims
//! add the include and library directories. nextbrew builds nothing from
//! source and has no shims, so it sets the same variables (`PATH`,
//! `PKG_CONFIG_PATH`, `CMAKE_PREFIX_PATH`, `ACLOCAL_PATH` and the switches
//! for build tools) and hands the shims' directories to the compiler
//! through `CPATH` and `LIBRARY_PATH` instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::brewfile::{Entry, Kind};

/// A Brewfile formula, as the environment uses it.
#[derive(Debug, Clone)]
pub struct Dep {
    pub name: String,
    /// `<prefix>/opt/<name>`.
    pub opt: PathBuf,
    pub keg_only: bool,
    /// The keg a `HOMEBREW_BUNDLE_FORMULA_VERSION_<NAME>` pins, if any.
    pub pinned_keg: Option<PathBuf>,
    pub version: String,
}

/// The Brewfile's formulae: their names resolved, keg-only or not (all
/// keg-only with `$HOMEBREW_BUNDLE_EXEC_ALL_KEG_ONLY_DEPS`), and any
/// version the environment pins. The pins are taken out of `env`, as
/// Homebrew does.
pub async fn deps(
    installer: &nb_io::Installer,
    entries: &[Entry],
    env: &mut BTreeMap<String, String>,
) -> Result<Vec<Dep>, nb_core::Error> {
    let all_keg_only = env
        .remove("HOMEBREW_BUNDLE_EXEC_ALL_KEG_ONLY_DEPS")
        .is_some_and(|v| !v.is_empty());
    let pins: BTreeMap<String, String> = env
        .iter()
        .filter_map(|(key, value)| {
            let name = key.strip_prefix("HOMEBREW_BUNDLE_FORMULA_VERSION_")?;
            (!name.is_empty()).then(|| (name.to_string(), value.clone()))
        })
        .collect();
    env.retain(|key, _| !key.starts_with("HOMEBREW_BUNDLE_FORMULA_VERSION_"));

    let mut deps = Vec::new();
    for entry in entries.iter().filter(|e| e.kind == Kind::Brew) {
        let formula = installer.formula(&entry.name).await?;
        let keg_only = all_keg_only || installer.is_keg_only(&formula.name).await?;
        let pinned_keg = pins
            .get(&env_name(&formula.name))
            .map(|version| installer.keg_path(&formula.name, version));
        deps.push(Dep {
            opt: installer.prefix().join("opt").join(&formula.name),
            name: formula.name,
            keg_only,
            pinned_keg,
            version: formula.versions.stable.clone(),
        });
    }
    Ok(deps)
}

/// `formula_versions_from_env`'s spelling of a formula name.
fn env_name(name: &str) -> String {
    name.to_uppercase()
        .replace('@', "AT")
        .replace('+', "X")
        .replace('-', "_")
}

/// Directories, without those missing or repeated (Homebrew's
/// `PATH#existing`).
fn existing(paths: impl IntoIterator<Item = PathBuf>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        let path = path.to_string_lossy().into_owned();
        if Path::new(&path).is_dir() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn join(paths: &[String]) -> String {
    paths.join(":")
}

/// Homebrew's `SANITIZED_VARS`, cleared before the environment is set up.
const SANITIZED: &[&str] = &[
    "CDPATH",
    "CLICOLOR_FORCE",
    "CPATH",
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "OBJC_INCLUDE_PATH",
    "CC",
    "CXX",
    "OBJC",
    "OBJCXX",
    "CPP",
    "MAKE",
    "LD",
    "LDSHARED",
    "CFLAGS",
    "CXXFLAGS",
    "OBJCFLAGS",
    "OBJCXXFLAGS",
    "LDFLAGS",
    "CPPFLAGS",
    "MACOSX_DEPLOYMENT_TARGET",
    "SDKROOT",
    "DEVELOPER_DIR",
    "CMAKE_PREFIX_PATH",
    "CMAKE_INCLUDE_PATH",
    "CMAKE_FRAMEWORK_PATH",
    "GOBIN",
    "GOPATH",
    "GOROOT",
    "PERL_MB_OPT",
    "PERL_MM_OPT",
    "LIBRARY_PATH",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "LD_RUN_PATH",
    "RUSTFLAGS",
];

/// `ENV.sensitive?`.
fn sensitive(key: &str) -> bool {
    let key = key.to_lowercase();
    ["cookie", "key", "token", "password", "passphrase", "auth"]
        .iter()
        .any(|word| key.contains(word))
}

/// The environment `command` runs in: `env` (the user's) with secrets
/// removed if asked, then the build environment for `deps`.
pub fn compose(
    mut env: BTreeMap<String, String>,
    deps: &[Dep],
    prefix: &Path,
    command: Option<&str>,
    no_secrets: bool,
) -> Result<BTreeMap<String, String>, nb_core::Error> {
    let user_path = env.get("PATH").cloned().unwrap_or_default();
    let home = env.get("HOME").cloned().unwrap_or_default();
    if no_secrets {
        env.retain(|key, _| !sensitive(key));
    }
    env.retain(|key, _| !SANITIZED.contains(&key.as_str()));
    let keg_only: Vec<&Dep> = deps.iter().filter(|d| d.keg_only).collect();
    let mut set = |key: &str, value: String| {
        env.insert(key.to_string(), value);
    };

    for (key, value) in [
        ("CC", "clang"),
        ("OBJC", "clang"),
        ("CXX", "clang++"),
        ("OBJCXX", "clang++"),
    ] {
        set(key, value.to_string());
    }
    // Python formulae's unversioned `python` and `pip`, newest first.
    let mut pythons: Vec<&Dep> = deps
        .iter()
        .filter(|d| d.name == "python" || d.name.starts_with("python@"))
        .collect();
    pythons.sort_by_key(|d| std::cmp::Reverse(version_key(&d.version)));
    let mut path = existing(
        deps.iter()
            .map(|d| d.opt.join("bin"))
            .chain(pythons.iter().map(|d| d.opt.join("libexec/bin")))
            .chain(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from)),
    );
    set(
        "PKG_CONFIG_PATH",
        join(&existing(deps.iter().flat_map(|d| {
            [d.opt.join("lib/pkgconfig"), d.opt.join("share/pkgconfig")]
        }))),
    );
    set(
        "PKG_CONFIG_LIBDIR",
        join(&existing([PathBuf::from("/usr/lib/pkgconfig")])),
    );
    set(
        "CMAKE_PREFIX_PATH",
        join(&existing(
            keg_only
                .iter()
                .map(|d| d.opt.clone())
                .chain([prefix.to_path_buf()]),
        )),
    );
    set(
        "ACLOCAL_PATH",
        join(&existing(
            keg_only
                .iter()
                .map(|d| d.opt.join("share/aclocal"))
                .chain([prefix.join("share/aclocal")]),
        )),
    );
    if deps.iter().any(|d| d.name == "libtool") {
        set(
            "M4",
            prefix.join("opt/m4/bin/m4").to_string_lossy().into_owned(),
        );
    }
    // What the compiler shims would add: `-isystem <prefix>/include`, the
    // keg-only formulae's `include`, and their `lib` and `<prefix>/lib`.
    set(
        "CPATH",
        join(&existing(
            [prefix.join("include")]
                .into_iter()
                .chain(keg_only.iter().map(|d| d.opt.join("include"))),
        )),
    );
    set(
        "LIBRARY_PATH",
        join(&existing(
            keg_only
                .iter()
                .filter(|d| {
                    !(d.name == "llvm"
                        || d.name
                            .strip_prefix("llvm@")
                            .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit())))
                })
                .map(|d| d.opt.join("lib"))
                .chain([prefix.join("lib")]),
        )),
    );
    let jobs = std::thread::available_parallelism().map_or(1, usize::from);
    env.entry("MAKEFLAGS".to_string())
        .or_insert_with(|| format!("-j{jobs}"));
    let mut set = |key: &str, value: &str| {
        env.insert(key.to_string(), value.to_string());
    };
    for (key, value) in [
        ("OPENSSL_NO_VENDOR", "1"),
        ("GOTOOLCHAIN", "local"),
        ("MATURIN_NO_INSTALL_RUST", "1"),
        ("HIDAPI_SYSTEM_HIDAPI", "1"),
        ("PYZMQ_NO_BUNDLE", "1"),
        ("SODIUM_INSTALL", "system"),
        ("BUNDLE_FORCE_RUBY_PLATFORM", "true"),
        ("BUNDLE_VERSION", "system"),
        ("BUNDLE_WITHOUT", "development:test"),
        ("OPAMNODEPEXTS", "1"),
        ("OPAMYES", "1"),
        ("ZERO_AR_DATE", "1"),
        ("HOMEBREW_INSIDE_BUNDLE", "1"),
    ] {
        set(key, value);
    }
    if cfg!(target_os = "macos") {
        set("am_cv_func_iconv_works", "yes");
    }

    // Version managers' shims go first.
    for dep in deps {
        if ["nodenv", "pyenv", "rbenv"].contains(&dep.name.as_str()) {
            let root = env
                .get(&format!("HOMEBREW_{}_ROOT", dep.name.to_uppercase()))
                .cloned()
                .unwrap_or_else(|| format!("{home}/.{}", dep.name));
            path.insert(0, format!("{root}/shims"));
        }
    }
    let found = |path: &[String], command: &str| {
        path.iter()
            .map(|dir| Path::new(dir).join(command))
            .find(|candidate| candidate.is_file())
    };
    if let Some(command) = command.filter(|c| !c.contains('/'))
        && let Some(dir) = found(&path, command).and_then(|f| f.parent().map(Path::to_path_buf))
    {
        path.insert(0, dir.to_string_lossy().into_owned());
    }
    env.insert("PATH".to_string(), path.join(":"));

    // A pinned version's keg instead of `opt/`: Homebrew's `/opt/<name>`
    // followed by `/`, `:` or `$`. In a `PATH`-like variable the rewritten
    // directories move to the front.
    for dep in deps {
        let Some(keg) = &dep.pinned_keg else {
            continue;
        };
        let opt = regex::Regex::new(&format!(
            "{}([/:$])",
            regex::escape(&dep.opt.to_string_lossy())
        ))
        .unwrap();
        let keg = keg.to_string_lossy().into_owned();
        let rewrite = |value: &str| {
            opt.replace_all(value, |found: &regex::Captures| {
                format!("{keg}{}", &found[1])
            })
            .into_owned()
        };
        for (key, value) in env.iter_mut() {
            if !opt.is_match(value) {
                continue;
            }
            *value = if key.contains("PATH") && path_like(value) {
                let mut dirs: Vec<String> = Vec::new();
                let mut moved: Vec<String> = Vec::new();
                for dir in value.split(':').filter(|dir| !dir.is_empty()) {
                    if opt.is_match(dir) {
                        moved.push(rewrite(dir));
                    } else {
                        dirs.push(dir.to_string());
                    }
                }
                for dir in moved {
                    dirs.insert(0, dir);
                }
                dirs.join(":")
            } else {
                rewrite(value)
            };
        }
    }

    // The user's own `PATH` after it.
    if !user_path.is_empty() {
        let path = env.entry("PATH".to_string()).or_default();
        for dir in user_path.split(':') {
            if !path.split(':').any(|known| known == dir) {
                path.push(':');
                path.push_str(dir);
            }
        }
    }
    if let Some(command) = command.filter(|c| !c.contains('/')) {
        let path: Vec<String> = env["PATH"].split(':').map(str::to_string).collect();
        if found(&path, command).is_none() {
            return Err(nb_core::Error::ExecutionError {
                message: format!("command was not found in your PATH: {command}"),
            });
        }
    }
    for var in ["HOMEBREW_TEMP", "TMPDIR", "HOMEBREW_TMPDIR"] {
        if env
            .get(var)
            .is_some_and(|dir| !dir.is_empty() && !writable(Path::new(dir)))
        {
            env.remove(var);
        }
    }
    Ok(env)
}

fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string for the call.
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

/// Homebrew's `PATH_LIKE_ENV_REGEX` (`.+:`).
fn path_like(value: &str) -> bool {
    value.find(':').is_some_and(|at| at > 0)
}

/// Numbers in a version, for ordering (`3.14.1` > `3.9`).
fn version_key(version: &str) -> Vec<u64> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// `brew bundle env`: `export` lines for what changed, Homebrew's own
/// variables left out; new directories of a `PATH`-like variable go in
/// front of its current value.
pub fn exports(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in new {
        if key.starts_with("HOMEBREW_") || key.starts_with("PORTABLE_RUBY_") || value.is_empty() {
            continue;
        }
        let old_value = old.get(key);
        if old_value == Some(value) {
            continue;
        }
        if key.contains("PATH") && path_like(value) {
            let old_dirs: Vec<&str> = old_value.map_or(Vec::new(), |v| v.split(':').collect());
            let mut added: Vec<&str> = Vec::new();
            for dir in value.split(':') {
                if !dir.is_empty() && !old_dirs.contains(&dir) && !added.contains(&dir) {
                    added.push(dir);
                }
            }
            if added.is_empty() {
                continue;
            }
            out += &format!(
                "export {key}=\"{}:${{{key}:-}}\"\n",
                sh_quote(&added.join(":"))
            );
        } else {
            out += &format!("export {key}=\"{}\"\n", sh_quote(value));
        }
    }
    out
}

/// `Utils::Shell.sh_quote`: Ruby's `Shellwords.escape`, keeping `~`.
fn sh_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || "_-.,:+/@".contains(c) {
            out.push(c);
        } else if c == '\n' {
            out.push_str("'\n'");
        } else if c == '~' {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    out
}

/// `Utils::Shell.shell_with_prompt`: the user's shell with an `nb bundle`
/// prompt, the build environment's `PATH` in front of whatever the shell's
/// own startup files set.
pub fn shell(env: &BTreeMap<String, String>) -> Result<Command, nb_core::Error> {
    let shell = env
        .get("SHELL")
        .cloned()
        .unwrap_or_else(|| "/bin/bash".to_string());
    let kind = Path::new(&shell)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let kind = kind.split('-').next().unwrap_or_default().to_string();
    let path = env.get("PATH").cloned().unwrap_or_default();
    let home = env.get("HOME").cloned().unwrap_or_default();
    let io = |e: std::io::Error| nb_core::Error::FileError {
        message: format!("failed to set up the shell's prompt: {e}"),
    };
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let mut cmd = Command::new(&shell);
    cmd.env_clear().envs(env);
    match kind.as_str() {
        "zsh" => {
            let dir = std::env::temp_dir().join(format!("nb-zsh-prompt-{euid}"));
            std::fs::create_dir_all(&dir).map_err(io)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
            std::fs::write(dir.join(".zshrc"), ZSHRC).map_err(io)?;
            for file in [".zshenv", ".zcompdump", ".zsh_history", ".zsh_sessions"] {
                let link = dir.join(file);
                let _ = std::fs::remove_file(&link);
                let _ = std::os::unix::fs::symlink(format!("{home}/{file}"), &link);
            }
            cmd.env("BREW_PROMPT_PATH", &path)
                .env("BREW_PROMPT_TYPE", "nb bundle")
                .env("ZDOTDIR", &dir);
        }
        "bash" => {
            let rc = std::env::temp_dir().join(format!("nb-bash-prompt-{euid}.bash"));
            std::fs::write(&rc, BASHRC).map_err(io)?;
            cmd.env("BREW_PROMPT_PATH", &path)
                .env("BREW_PROMPT_TYPE", "nb bundle")
                .arg("--rcfile")
                .arg(&rc);
        }
        _ => {
            cmd.env(
                "PS1",
                "\\[\\033[1;32m\\]nb bundle \\[\\033[1;31m\\]\\w \\[\\033[1;34m\\]$\\[\\033[0m\\] ",
            );
        }
    }
    if env
        .get("HOMEBREW_NO_ENV_HINTS")
        .is_none_or(String::is_empty)
    {
        println!(
            "Your shell has been configured to use a build environment from your `Brewfile`.\n\
             This should help you build stuff.\n\
             Hide these hints with `HOMEBREW_NO_ENV_HINTS=1`.\n\
             When done, type `exit`."
        );
    }
    Ok(cmd)
}

/// Homebrew's `brew-sh-prompt-zshrc.zsh`.
const ZSHRC: &str = r#"# Read the user's ~/.zshrc first
if [[ -f "${HOME}/.zshrc" ]]
then
  source "${HOME}/.zshrc"
fi

# Override the user's ZSH prompt with our custom prompt
export PROMPT="%B%F{green}${BREW_PROMPT_TYPE}%f %F{blue}$%f%b "
export RPROMPT="[%B%F{red}%~%f%b]"

# Add the build environment's PATH in front of the user's PATH
export PATH="${BREW_PROMPT_PATH}:${PATH}"
unset BREW_PROMPT_TYPE BREW_PROMPT_PATH
"#;

/// Homebrew's `brew-sh-prompt-bashrc.bash`.
const BASHRC: &str = r#"# Read the user's ~/.bashrc first
if [[ -f "${HOME}/.bashrc" ]]
then
  source "${HOME}/.bashrc"
fi

# Override the user's Bash prompt with our custom prompt
export PS1="\\[\\033[1;32m\\]${BREW_PROMPT_TYPE} \\[\\033[1;31m\\]\\w \\[\\033[1;34m\\]$\\[\\033[0m\\] "

# Add the build environment's PATH in front of the user's PATH
export PATH="${BREW_PROMPT_PATH}:${PATH}"
unset BREW_PROMPT_TYPE BREW_PROMPT_PATH
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(root: &Path, name: &str, keg_only: bool, dirs: &[&str]) -> Dep {
        let opt = root.join("opt").join(name);
        for dir in dirs {
            std::fs::create_dir_all(opt.join(dir)).unwrap();
        }
        Dep {
            name: name.to_string(),
            opt,
            keg_only,
            pinned_keg: None,
            version: "1.0".to_string(),
        }
    }

    #[test]
    fn sets_up_the_build_environment() {
        let root = tempfile::tempdir().unwrap();
        let prefix = root.path();
        for dir in ["include", "lib", "share/aclocal"] {
            std::fs::create_dir_all(prefix.join(dir)).unwrap();
        }
        let deps = [
            dep(prefix, "jq", false, &["bin", "lib/pkgconfig"]),
            dep(
                prefix,
                "openssl@3",
                true,
                &["bin", "include", "lib/pkgconfig"],
            ),
        ];
        let user: BTreeMap<String, String> = [
            ("PATH", "/usr/bin:/home/me/bin"),
            ("HOME", "/home/me"),
            ("CFLAGS", "-O3"),
            ("GITHUB_TOKEN", "secret"),
            ("MAKEFLAGS", "-j2"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();
        let env = compose(user.clone(), &deps, prefix, None, true).unwrap();
        let p = |rel: &str| prefix.join(rel).to_string_lossy().into_owned();
        assert!(
            env["PATH"].starts_with(&format!("{}:{}:", p("opt/jq/bin"), p("opt/openssl@3/bin"))),
            "{}",
            env["PATH"]
        );
        assert!(env["PATH"].ends_with(":/home/me/bin"));
        assert_eq!(
            env["PKG_CONFIG_PATH"],
            format!(
                "{}:{}",
                p("opt/jq/lib/pkgconfig"),
                p("opt/openssl@3/lib/pkgconfig")
            )
        );
        // Only keg-only formulae need their own directories named.
        assert_eq!(
            env["CMAKE_PREFIX_PATH"],
            format!("{}:{}", p("opt/openssl@3"), prefix.display())
        );
        assert_eq!(
            env["CPATH"],
            format!("{}:{}", p("include"), p("opt/openssl@3/include"))
        );
        assert_eq!(
            env["LIBRARY_PATH"],
            format!("{}:{}", p("opt/openssl@3/lib"), p("lib"))
        );
        assert_eq!(env["CC"], "clang");
        assert_eq!(env["MAKEFLAGS"], "-j2");
        assert!(!env.contains_key("CFLAGS"));
        assert!(!env.contains_key("GITHUB_TOKEN"));

        let exports = exports(&user, &env);
        assert!(exports.contains(&format!(
            "export CPATH=\"{}:${{CPATH:-}}\"\n",
            sh_quote(&format!("{}:{}", p("include"), p("opt/openssl@3/include")))
        )));
        assert!(exports.contains("export CC=\"clang\"\n"));
        assert!(!exports.contains("HOMEBREW_INSIDE_BUNDLE"));
        assert!(!exports.contains("MAKEFLAGS"));

        assert!(compose(user, &deps, prefix, Some("no-such-command-xyz"), false).is_err());
    }

    #[test]
    fn quotes_like_shellwords() {
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "a\\ b");
        assert_eq!(sh_quote("~/x:y"), "~/x:y");
        assert_eq!(sh_quote("-j8"), "-j8");
        assert_eq!(env_name("python@3.14"), "PYTHONAT3.14");
        assert_eq!(env_name("gtk+3"), "GTKX3");
    }
}
