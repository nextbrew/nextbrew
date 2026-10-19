//! The checks of `brew doctor` (`diagnostic.rb`) that apply to nextbrew,
//! with Homebrew's wording. nextbrew pours bottles only and has no git
//! checkout or taps, so the git, tap, Xcode/CLT and stray-file checks, which
//! guard source builds and `brew update`, are left out. One check is
//! nextbrew's own: commands shadowed by an earlier `PATH` entry, such as
//! Homebrew's `bin` when both are in use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nb_core::{Error, formula_token};

use super::Installer;

/// One problem `doctor` found: the check that found it, as Homebrew names
/// it, and what to tell the user, remediation included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: &'static str,
    pub message: String,
}

impl Finding {
    fn new(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            check,
            message: message.into().trim_end().to_string(),
        }
    }
}

/// What the checks read from the environment, so tests can supply it.
#[derive(Debug, Clone, Default)]
pub struct SystemEnv {
    /// `PATH`, split, in order.
    pub path: Vec<PathBuf>,
    /// `TMPDIR`, if set.
    pub tmpdir: Option<PathBuf>,
    /// `SHELL`, for the line suggested to fix `PATH`.
    pub shell: Option<String>,
    pub user: String,
}

impl SystemEnv {
    pub fn current() -> Self {
        Self {
            path: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
            tmpdir: std::env::var_os("TMPDIR").map(PathBuf::from),
            shell: std::env::var("SHELL").ok(),
            user: std::env::var("USER").unwrap_or_else(|_| "$(whoami)".to_string()),
        }
    }
}

impl Installer {
    /// Run every check, in `brew doctor`'s order: the slow ones (missing
    /// dependencies) last, the cask ones only when a cask is installed.
    /// Broken symlinks are part of [`Installer::doctor`]'s report, which
    /// `--repair` can remove.
    pub async fn diagnose_system(&self, env: &SystemEnv) -> Result<Vec<Finding>, Error> {
        let installed = self.db.list_installed()?;
        let formulae: Vec<&str> = installed
            .iter()
            .map(|keg| keg.name.as_str())
            .filter(|name| !name.starts_with("cask:"))
            .collect();
        let casks: Vec<&str> = installed
            .iter()
            .filter_map(|keg| keg.name.strip_prefix("cask:"))
            .collect();
        let temp = self.prefix.join("tmp");

        let mut findings = Vec::new();
        #[cfg(target_os = "macos")]
        findings.extend(check_for_unsupported_macos(
            &macos_version()?,
            cfg!(target_arch = "x86_64"),
        ));
        findings.extend(check_exist_directories(
            &self.prefix,
            self.cellar.dir(),
            &env.user,
        ));
        findings.extend(check_access_directories(
            &self.writable_directories(),
            &env.user,
        ));
        findings.extend(check_tmpdir(env.tmpdir.as_deref()));
        findings.extend(check_tmpdir_sticky_bit(&temp));
        findings.extend(check_user_path(&self.prefix, env));
        findings.extend(check_shadowed_commands(&self.prefix, &env.path));
        findings.extend(self.check_for_unlinked_but_not_keg_only(&formulae).await?);
        let (deleted, lifecycle) = self.formula_lifecycle(&formulae).await?;
        findings.extend(check_deleted_formula(&deleted));
        findings.extend(check_deprecated_disabled(&lifecycle));
        #[cfg(target_os = "macos")]
        {
            let cellar = self.cellar.dir().to_path_buf();
            findings.extend(check_filesystem_case_sensitive(&[
                &self.prefix,
                &cellar,
                &temp,
            ]));
            findings.extend(check_for_multiple_volumes(
                &cellar,
                &temp,
                &self.store_dir(),
            ));
        }
        if !casks.is_empty() {
            findings.extend(self.check_cask_deprecated_disabled(&casks).await);
            #[cfg(target_os = "macos")]
            findings.extend(self.check_cask_quarantine_support(&temp));
        }
        findings.extend(self.check_missing_deps(&installed).await?);
        Ok(findings)
    }

    fn store_dir(&self) -> PathBuf {
        self.locks_dir
            .parent()
            .map(|root| root.join("store"))
            .unwrap_or_else(|| self.prefix.join("store"))
    }

    /// `Keg.must_be_writable_directories`, with nextbrew's own directories
    /// for Homebrew's cache, locks and repository.
    fn writable_directories(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = PREFIX_WRITABLE_SUBDIRECTORIES
            .iter()
            .chain(MUST_EXIST_SUBDIRECTORIES)
            .map(|dir| self.prefix.join(dir))
            .collect();
        dirs.extend([
            self.cellar.dir().to_path_buf(),
            self.cache_dir.clone(),
            self.locks_dir.clone(),
            self.store_dir(),
        ]);
        if let Ok(entries) = std::fs::read_dir(self.prefix.join("lib")) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("python") {
                    dirs.push(entry.path().join("site-packages"));
                }
            }
        }
        dirs.sort();
        dirs.dedup();
        dirs
    }

    async fn check_for_unlinked_but_not_keg_only(
        &self,
        formulae: &[&str],
    ) -> Result<Option<Finding>, Error> {
        let mut unlinked = Vec::new();
        for name in formulae {
            if self.is_linked(name)? {
                continue;
            }
            // An unknown formula counts as not keg-only, as in Homebrew.
            if !self.is_keg_only(name).await.unwrap_or(false) {
                unlinked.push(name.to_string());
            }
        }
        if unlinked.is_empty() {
            return Ok(None);
        }
        Ok(Some(Finding::new(
            "check_for_unlinked_but_not_keg_only",
            format!(
                "You have unlinked kegs in your Cellar.\n\
                 Leaving kegs unlinked can lead to build-trouble and cause formulae that depend on\n\
                 those kegs to fail to run properly once built.\n{}",
                indented_list("Run `nb link` on these:", &unlinked)
            ),
        )))
    }

    /// Installed formulae the API no longer has, and the deprecated or
    /// disabled ones among the rest.
    async fn formula_lifecycle(
        &self,
        formulae: &[&str],
    ) -> Result<(Vec<String>, Vec<String>), Error> {
        let (mut deleted, mut lifecycle) = (Vec::new(), Vec::new());
        for name in formulae {
            match self.api_client.get_formula(name).await {
                Ok(formula) if formula.lifecycle.deprecated || formula.lifecycle.disabled => {
                    lifecycle.push(formula.name)
                }
                Ok(_) => {}
                Err(Error::MissingFormula { .. }) => deleted.push(name.to_string()),
                Err(e) => return Err(e),
            }
        }
        lifecycle.sort();
        lifecycle.dedup();
        Ok((deleted, lifecycle))
    }

    async fn check_cask_deprecated_disabled(&self, casks: &[&str]) -> Option<Finding> {
        let mut flagged = Vec::new();
        for token in casks {
            // A cask the API no longer serves is not this check's business.
            let Ok(cask) = self.cask(token).await else {
                continue;
            };
            if cask["deprecated"].as_bool() == Some(true)
                || cask["disabled"].as_bool() == Some(true)
            {
                flagged.push(token.to_string());
            }
        }
        if flagged.is_empty() {
            return None;
        }
        flagged.sort();
        Some(Finding::new(
            "check_cask_deprecated_disabled",
            format!(
                "Some installed casks are deprecated or disabled.\n{}",
                indented_list(
                    "You should find replacements for the following casks:\n",
                    &flagged
                )
            ),
        ))
    }

    #[cfg(target_os = "macos")]
    fn check_cask_quarantine_support(&self, temp: &Path) -> Option<Finding> {
        let dir = if temp.is_dir() {
            temp.to_path_buf()
        } else {
            std::env::temp_dir()
        };
        let error = super::quarantine::check_support(&dir).err()?;
        Some(Finding::new(
            "check_cask_quarantine_support",
            format!("No Cask quarantine support available: {error}"),
        ))
    }

    /// `Homebrew::Missing.deps`: each installed formula's runtime closure,
    /// and each cask's `depends_on formula:`, less what is installed.
    async fn check_missing_deps(
        &self,
        installed: &[crate::storage::db::InstalledKeg],
    ) -> Result<Option<Finding>, Error> {
        let have: BTreeSet<&str> = installed
            .iter()
            .map(|keg| formula_token(&keg.name))
            .collect();
        let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for keg in installed {
            let deps: Vec<String> = match keg.name.strip_prefix("cask:") {
                Some(_) => self.cask_formula_dependencies(&keg.name, &keg.version),
                None => match self.dependencies(&keg.name, false).await {
                    Ok(deps) => deps.into_iter().collect(),
                    // Reported by check_deleted_formula.
                    Err(Error::MissingFormula { .. }) => continue,
                    Err(e) => return Err(e),
                },
            };
            let absent: Vec<String> = deps
                .into_iter()
                .filter(|dep| !have.contains(formula_token(dep)))
                .collect();
            if !absent.is_empty() {
                let name = keg.name.strip_prefix("cask:").unwrap_or(&keg.name);
                missing.insert(name.to_string(), absent);
            }
        }
        if missing.is_empty() {
            return Ok(None);
        }
        let all: BTreeSet<&String> = missing.values().flatten().collect();
        let command = format!(
            "nb install {}",
            all.into_iter().cloned().collect::<Vec<_>>().join(" ")
        );
        let lines: Vec<String> = missing
            .iter()
            .map(|(name, deps)| format!("{name}: {}", deps.join(" ")))
            .collect();
        Ok(Some(Finding::new(
            "check_missing_deps",
            format!(
                "{}{}",
                indented_list(
                    "Some installed formulae or casks are missing dependencies:",
                    &lines
                ),
                indented_list(
                    "You should `nb install` the missing dependencies:",
                    &[command]
                )
            ),
        )))
    }
}

/// `Keg.must_exist_subdirectories`, less Homebrew's `var/homebrew/linked`,
/// which nextbrew does not keep.
pub(super) const MUST_EXIST_SUBDIRECTORIES: &[&str] =
    &["bin", "etc", "include", "lib", "opt", "sbin", "share"];

/// The prefix part of `Keg.must_be_writable_directories`.
const PREFIX_WRITABLE_SUBDIRECTORIES: &[&str] = &[
    "etc/bash_completion.d",
    "lib/cps",
    "lib/pkgconfig",
    "share/aclocal",
    "share/doc",
    "share/info",
    "share/locale",
    "share/man",
    "share/man/man1",
    "share/man/man2",
    "share/man/man3",
    "share/man/man4",
    "share/man/man5",
    "share/man/man6",
    "share/man/man7",
    "share/man/man8",
    "share/cps",
    "share/zsh",
    "share/zsh/site-functions",
    "share/fish",
    "share/fish/vendor_completions.d",
    "share/fish/vendor_functions.d",
    "share/pwsh",
    "share/pwsh/completions",
    "var/log",
];

/// `append_indented_list`: a header, then each item on its own line.
fn indented_list(header: &str, items: &[impl AsRef<str>]) -> String {
    let mut text = format!("{}\n", header.trim_end_matches('\n'));
    if header.ends_with('\n') {
        text.push('\n');
    }
    for item in items {
        text.push_str(&format!("  {}\n", item.as_ref()));
    }
    text
}

fn joined(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is NUL-terminated and outlives the call.
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

fn check_exist_directories(prefix: &Path, cellar: &Path, user: &str) -> Option<Finding> {
    if writable(prefix) {
        return None;
    }
    let mut dirs: Vec<PathBuf> = MUST_EXIST_SUBDIRECTORIES
        .iter()
        .map(|dir| prefix.join(dir))
        .chain([cellar.to_path_buf()])
        .filter(|dir| !dir.exists())
        .collect();
    if dirs.is_empty() {
        return None;
    }
    dirs.sort();
    let list = joined(&dirs);
    let display: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    Some(Finding::new(
        "check_exist_directories",
        format!(
            "{}{}",
            indented_list("The following directories do not exist:", &display),
            indented_list(
                "You should create these directories and change their ownership to your user.",
                &[
                    format!("sudo mkdir -p {list}"),
                    format!("sudo chown -R {user} {list}"),
                ]
            )
        ),
    ))
}

fn check_access_directories(dirs: &[PathBuf], user: &str) -> Option<Finding> {
    let not_writable: Vec<PathBuf> = dirs
        .iter()
        .filter(|dir| dir.exists() && !writable(dir))
        .cloned()
        .collect();
    if not_writable.is_empty() {
        return None;
    }
    let list = joined(&not_writable);
    let display: Vec<String> = not_writable
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    Some(Finding::new(
        "check_access_directories",
        format!(
            "{}{}",
            indented_list(
                "The following directories are not writable by your user:",
                &display
            ),
            indented_list(
                "You should change the ownership of these directories to your user,\n\
                 and make sure that you have write permission.",
                &[
                    format!("sudo chown -R {user} {list}"),
                    format!("chmod u+w {list}"),
                ]
            )
        ),
    ))
}

fn check_tmpdir(tmpdir: Option<&Path>) -> Option<Finding> {
    let tmpdir = tmpdir?;
    if tmpdir.is_dir() {
        return None;
    }
    Some(Finding::new(
        "check_tmpdir",
        format!("TMPDIR {:?} doesn't exist.", tmpdir.display().to_string()),
    ))
}

fn check_tmpdir_sticky_bit(temp: &Path) -> Option<Finding> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(temp).ok()?.permissions().mode();
    if mode & 0o777 != 0o777 || mode & 0o1000 != 0 {
        return None;
    }
    Some(Finding::new(
        "check_tmpdir_sticky_bit",
        format!(
            "{} is world-writable but does not have the sticky bit set.\n{}",
            temp.display(),
            indented_list(
                "To set it, run the following command:",
                &[format!("sudo chmod +t {}", temp.display())]
            )
        ),
    ))
}

/// `check_user_path_1`, `_2` and `_3` together, as they share one walk of
/// `PATH`.
fn check_user_path(prefix: &Path, env: &SystemEnv) -> Vec<Finding> {
    let bin = prefix.join("bin");
    let sbin = prefix.join("sbin");
    let mut paths: Vec<&PathBuf> = Vec::new();
    for p in &env.path {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }

    let mut findings = Vec::new();
    let (mut seen_bin, mut seen_sbin) = (false, false);
    for p in paths {
        if p == Path::new("/usr/bin") && !seen_bin {
            let mut conflicts: Vec<String> = std::fs::read_dir(&bin)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| Path::new("/usr/bin").join(name).exists())
                .collect();
            conflicts.sort();
            if !conflicts.is_empty() {
                findings.push(Finding::new(
                    "check_user_path_1",
                    format!(
                        "{}{}",
                        indented_list(
                            &format!(
                                "/usr/bin occurs before {} in your PATH.\n\
                                 This means that system-provided programs will be used instead of those\n\
                                 provided by nextbrew.\n\n\
                                 The following tools exist at both paths:",
                                bin.display()
                            ),
                            &conflicts
                        ),
                        path_remediation(&bin, env.shell.as_deref())
                    ),
                ));
            }
        } else if *p == bin {
            seen_bin = true;
        } else if *p == sbin {
            seen_sbin = true;
        }
    }

    if !seen_bin {
        findings.push(Finding::new(
            "check_user_path_2",
            format!(
                "nextbrew's \"bin\" was not found in your PATH.\n{}",
                path_remediation(&bin, env.shell.as_deref())
            ),
        ));
    }
    let sbin_used = std::fs::read_dir(&sbin)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name() != ".keepme");
    if !seen_sbin && sbin_used {
        findings.push(Finding::new(
            "check_user_path_3",
            format!(
                "nextbrew's \"sbin\" was not found in your PATH but you have installed\n\
                 formulae that put executables in {}.\n{}",
                sbin.display(),
                path_remediation(&sbin, env.shell.as_deref())
            ),
        ));
    }
    findings
}

/// `path_remediation`, with `Utils::Shell.prepend_path_in_profile` for the
/// common shells.
fn path_remediation(dir: &Path, shell: Option<&str>) -> String {
    format!(
        "Consider setting your PATH for example like so:\n  {}",
        prepend_path_in_profile(dir, shell)
    )
}

/// `Utils::Shell.prepend_path_in_profile` for the common shells: the line
/// that puts `dir` first in `PATH` from the profile of `shell` (`$SHELL`).
pub fn prepend_path_in_profile(dir: &Path, shell: Option<&str>) -> String {
    let shell = shell
        .and_then(|s| Path::new(s).file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match shell.as_str() {
        "fish" => format!("fish_add_path {}", sh_quote(&dir.display().to_string())),
        _ => {
            let profile = match shell.as_str() {
                "zsh" => "~/.zshrc",
                "bash"
                    if std::env::var_os("HOME")
                        .is_some_and(|home| Path::new(&home).join(".bash_profile").exists()) =>
                {
                    "~/.bash_profile"
                }
                "ksh" | "mksh" => "~/.kshrc",
                _ => "~/.profile",
            };
            format!(
                "printf '%s\\n' {} >> {profile}",
                sh_quote(&format!(
                    "export PATH={}:$PATH",
                    sh_quote(&dir.display().to_string())
                ))
            )
        }
    }
}

/// `Utils::Shell.sh_quote`: unchanged when safe, else backslash-escaped.
fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-.,:/@\n".contains(c))
    {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if !(c.is_ascii_alphanumeric() || "_-.,:/@\n".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// nextbrew's own check: commands in `$PREFIX/bin` that an earlier `PATH`
/// entry also has, so the shell runs the other one. `/usr/bin` is left to
/// `check_user_path_1`.
fn check_shadowed_commands(prefix: &Path, path: &[PathBuf]) -> Option<Finding> {
    let bin = prefix.join("bin");
    let earlier: Vec<&PathBuf> = path
        .iter()
        .take_while(|p| **p != bin)
        .filter(|p| p.as_path() != Path::new("/usr/bin"))
        .collect();
    if earlier.len() == path.len() {
        return None; // not in PATH: check_user_path_2 says so
    }
    let mut names: Vec<String> = std::fs::read_dir(&bin)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let shadowed: Vec<String> = names
        .iter()
        .filter_map(|name| {
            let other = earlier
                .iter()
                .map(|dir| dir.join(name))
                .find(|p| is_executable(p))?;
            Some(format!("{name}: {}", other.display()))
        })
        .collect();
    if shadowed.is_empty() {
        return None;
    }
    Some(Finding::new(
        "check_shadowed_commands",
        format!(
            "{}Put {} before those directories in your PATH to use nextbrew's.",
            indented_list(
                &format!(
                    "Some commands in {} are shadowed by earlier entries in your PATH:",
                    bin.display()
                ),
                &shadowed
            ),
            bin.display()
        ),
    ))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn check_deleted_formula(deleted: &[String]) -> Option<Finding> {
    if deleted.is_empty() {
        return None;
    }
    Some(Finding::new(
        "check_deleted_formula",
        format!(
            "Some installed kegs have no formulae!\n\
             This means they were either deleted or installed manually.\n\n{}",
            indented_list(
                "You should find replacements for the following formulae:\n",
                deleted
            )
        ),
    ))
}

fn check_deprecated_disabled(names: &[String]) -> Option<Finding> {
    if names.is_empty() {
        return None;
    }
    Some(Finding::new(
        "check_deprecated_disabled",
        format!(
            "Some installed formulae are deprecated or disabled.\n{}",
            indented_list(
                "You should find replacements for the following formulae:\n",
                names
            )
        ),
    ))
}

/// Homebrew's support window (`brew.sh`): keep in step with
/// `HOMEBREW_MACOS_OLDEST_SUPPORTED` and `HOMEBREW_MACOS_NEWEST_UNSUPPORTED`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_OLDEST_SUPPORTED: u32 = 15;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_NEWEST_UNSUPPORTED: u32 = 28;
/// The last macOS Homebrew builds Intel bottles for (`version > :tahoe`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const MACOS_LAST_INTEL: u32 = 26;

#[cfg(target_os = "macos")]
fn macos_version() -> Result<String, Error> {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .map_err(|e| Error::ExecutionError {
            message: format!("failed to run sw_vers: {e}"),
        })?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The major version, with 10.x as 10 (and Big Sur's 10.16 as 11).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_major(version: &str) -> Option<u32> {
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    if major == 10 && parts.next() == Some("16") {
        return Some(11);
    }
    Some(major)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn check_for_unsupported_macos(version: &str, intel: bool) -> Option<Finding> {
    let major = macos_major(version)?;
    let outdated = major < MACOS_OLDEST_SUPPORTED;
    let (who, what, shown) = if outdated {
        (
            "We (and Apple)",
            "old version.".to_string(),
            version.to_string(),
        )
    } else if intel {
        (
            "We",
            "platform (as-of September 2026, announced August 2025).\n\n\
             Apple have dropped Intel x86_64 support in macOS Golden Gate (27).\n\
             GitHub Actions are dropping macOS Intel x86_64 runners in 2027.\n\
             Homebrew is a non-profit project run entirely by volunteers, not employees.\n\
             If the biggest companies in the world cannot support macOS Intel x86_64\n\
             any longer, sadly neither can we."
                .to_string(),
            "on Intel x86_64".to_string(),
        )
    } else if major >= MACOS_NEWEST_UNSUPPORTED {
        (
            "We",
            "pre-release version.".to_string(),
            version.to_string(),
        )
    } else {
        return None;
    };
    let mut message =
        format!("You are using macOS {shown}.\n{who} do not provide support for this {what}");
    if (intel || outdated) && major <= MACOS_LAST_INTEL {
        message.push_str(
            "\nHomebrew no longer builds bottles for this configuration.\n\
             nextbrew installs bottles only, so formulae without one for it cannot be installed.",
        );
    }
    Some(Finding::new("check_for_unsupported_macos", message))
}

/// A directory is on a case-sensitive volume when its upper- or lower-case
/// spelling is missing.
#[cfg(target_os = "macos")]
fn check_filesystem_case_sensitive(dirs: &[&Path]) -> Option<Finding> {
    let mut volumes: Vec<String> = dirs
        .iter()
        .filter(|dir| dir.exists())
        .filter(|dir| {
            let s = dir.to_string_lossy();
            !(Path::new(&s.to_uppercase()).exists() && Path::new(&s.to_lowercase()).exists())
        })
        .filter_map(|dir| mount_point(dir))
        .collect();
    volumes.sort();
    volumes.dedup();
    if volumes.is_empty() {
        return None;
    }
    Some(Finding::new(
        "check_filesystem_case_sensitive",
        format!(
            "The filesystem on {} appears to be case-sensitive.\n\
             The default macOS filesystem is case-insensitive. Please report any apparent problems.",
            volumes.join(",")
        ),
    ))
}

#[cfg(target_os = "macos")]
fn mount_point(path: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated; `fs` is a valid out-pointer.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut fs) } != 0 {
        return None;
    }
    // SAFETY: the kernel NUL-terminates `f_mntonname`.
    let name = unsafe { std::ffi::CStr::from_ptr(fs.f_mntonname.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

/// Homebrew's check compares the Cellar with its temporary directory.
/// nextbrew also clones kegs out of its store, which only works within one
/// volume, so it compares the store too.
#[cfg(target_os = "macos")]
fn check_for_multiple_volumes(cellar: &Path, temp: &Path, store: &Path) -> Vec<Finding> {
    use std::os::unix::fs::MetadataExt;
    let device = |p: &Path| std::fs::metadata(p).ok().map(|m| m.dev());
    let Some(cellar_dev) = device(cellar) else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    if device(temp).is_some_and(|dev| dev != cellar_dev) {
        findings.push(Finding::new(
            "check_for_multiple_volumes",
            "Your Cellar and TEMP directories are on different volumes.\n\
             macOS won't move relative symlinks across volumes unless the target file already\n\
             exists. Formulae known to be affected by this are Git and Narwhal.",
        ));
    }
    if device(store).is_some_and(|dev| dev != cellar_dev) {
        findings.push(Finding::new(
            "check_for_multiple_volumes",
            format!(
                "Your Cellar and store ({}) are on different volumes.\n\
                 nextbrew copies kegs out of the store instead of cloning them, which is slower\n\
                 and takes the space twice. Keep NEXTBREW_ROOT on the same volume as the prefix.",
                store.display()
            ),
        ));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn env(path: &[&Path]) -> SystemEnv {
        SystemEnv {
            path: path.iter().map(|p| p.to_path_buf()).collect(),
            tmpdir: None,
            shell: Some("/bin/zsh".to_string()),
            user: "me".to_string(),
        }
    }

    fn prefix_with_bin(names: &[&str]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("bin")).unwrap();
        for name in names {
            let file = tmp.path().join("bin").join(name);
            std::fs::write(&file, "").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        tmp
    }

    #[test]
    fn usr_bin_first_reports_only_tools_in_both() {
        let prefix = prefix_with_bin(&["env", "nb-only-tool"]);
        let bin = prefix.path().join("bin");
        let findings = check_user_path(prefix.path(), &env(&[Path::new("/usr/bin"), &bin]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "check_user_path_1");
        assert!(findings[0].message.contains("  env\n"));
        assert!(!findings[0].message.contains("nb-only-tool"));
        assert!(findings[0].message.ends_with(&format!(
            "printf '%s\\n' export\\ PATH\\={}:\\$PATH >> ~/.zshrc",
            bin.display()
        )));
    }

    #[test]
    fn bin_first_is_fine_and_missing_bin_is_reported() {
        let prefix = prefix_with_bin(&["ls"]);
        let bin = prefix.path().join("bin");
        assert!(check_user_path(prefix.path(), &env(&[&bin, Path::new("/usr/bin")])).is_empty());

        let findings = check_user_path(prefix.path(), &env(&[Path::new("/bin")]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "check_user_path_2");
    }

    #[test]
    fn sbin_is_only_wanted_when_something_is_in_it() {
        let prefix = prefix_with_bin(&[]);
        let bin = prefix.path().join("bin");
        std::fs::create_dir(prefix.path().join("sbin")).unwrap();
        assert!(check_user_path(prefix.path(), &env(&[&bin])).is_empty());

        std::fs::write(prefix.path().join("sbin/tool"), "").unwrap();
        let findings = check_user_path(prefix.path(), &env(&[&bin]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check, "check_user_path_3");
    }

    #[test]
    fn a_command_in_an_earlier_path_entry_is_shadowed() {
        let prefix = prefix_with_bin(&["jq", "xz"]);
        let other = prefix_with_bin(&["jq"]);
        let bin = prefix.path().join("bin");
        let other_bin = other.path().join("bin");

        let finding =
            check_shadowed_commands(prefix.path(), &[other_bin.clone(), bin.clone()]).unwrap();
        assert!(
            finding
                .message
                .contains(&format!("  jq: {}\n", other_bin.join("jq").display()))
        );
        assert!(!finding.message.contains("xz"));

        assert!(check_shadowed_commands(prefix.path(), &[bin.clone(), other_bin]).is_none());
        assert!(check_shadowed_commands(prefix.path(), &[]).is_none());
    }

    #[test]
    fn tmpdir_must_exist_when_set() {
        let tmp = TempDir::new().unwrap();
        assert!(check_tmpdir(None).is_none());
        assert!(check_tmpdir(Some(tmp.path())).is_none());
        let finding = check_tmpdir(Some(&tmp.path().join("gone"))).unwrap();
        assert!(finding.message.starts_with("TMPDIR \""));
        assert!(finding.message.ends_with("gone\" doesn't exist."));
    }

    #[test]
    fn a_world_writable_temp_needs_the_sticky_bit() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_tmpdir_sticky_bit(dir).is_some());
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(check_tmpdir_sticky_bit(dir).is_none());
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_tmpdir_sticky_bit(dir).is_none());
    }

    #[test]
    fn unwritable_directories_are_listed_with_the_fix() {
        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let dirs = [
            locked.clone(),
            tmp.path().join("absent"),
            tmp.path().to_path_buf(),
        ];
        let finding = check_access_directories(&dirs, "me");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root can write anywhere
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let message = finding.unwrap().message;
        assert!(message.contains(&format!("sudo chown -R me {}", locked.display())));
        assert!(!message.contains("absent"));
    }

    #[test]
    fn deprecated_and_deleted_formulae_name_the_kegs() {
        assert!(check_deleted_formula(&[]).is_none());
        assert_eq!(
            check_deprecated_disabled(&["a".into(), "b".into()])
                .unwrap()
                .message,
            "Some installed formulae are deprecated or disabled.\n\
             You should find replacements for the following formulae:\n\n  a\n  b"
        );
    }

    #[test]
    fn macos_versions_outside_the_support_window_are_reported() {
        assert!(check_for_unsupported_macos("26.6.2", false).is_none());
        assert!(check_for_unsupported_macos("15.7", false).is_none());

        let old = check_for_unsupported_macos("14.7", false).unwrap().message;
        assert!(old.starts_with(
            "You are using macOS 14.7.\nWe (and Apple) do not provide support for this old version."
        ));
        assert!(old.contains("no longer builds bottles"));

        let pre = check_for_unsupported_macos("28.0", false).unwrap().message;
        assert_eq!(
            pre,
            "You are using macOS 28.0.\nWe do not provide support for this pre-release version."
        );

        let intel = check_for_unsupported_macos("26.1", true).unwrap().message;
        assert!(intel.starts_with("You are using macOS on Intel x86_64.\n"));
        assert!(intel.contains("no longer builds bottles"));
        assert!(
            !check_for_unsupported_macos("27.0", true)
                .unwrap()
                .message
                .contains("bottles")
        );
        assert_eq!(macos_major("10.16"), Some(11));
    }

    #[test]
    fn sh_quote_escapes_like_homebrew() {
        assert_eq!(sh_quote("/opt/nextbrew/bin"), "/opt/nextbrew/bin");
        assert_eq!(
            sh_quote("export PATH=/x:$PATH"),
            "export\\ PATH\\=/x:\\$PATH"
        );
    }
}
