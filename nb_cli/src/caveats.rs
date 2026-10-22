//! What installed packages ask to be told: a formula's `Caveats`, a
//! cask's caveats, and `Messages#display_caveats`, which shows those of
//! the packages a command installed once it is done.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use console::style;
use serde_json::Value;

/// What the API's placeholders stand for here.
pub(crate) struct Places {
    pub prefix: String,
    pub appdir: String,
    pub home: String,
}

impl Places {
    /// The places for `installer`, with casks' apps in `appdir`.
    pub fn new(installer: &nb_io::Installer, appdir: Option<&Path>) -> Self {
        Places {
            prefix: installer.prefix().display().to_string(),
            appdir: appdir
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "/Applications".to_string()),
            home: std::env::var("HOME").unwrap_or_default(),
        }
    }

    /// `CaskStruct#deep_remove_placeholders`, for a cask's strings. A bare
    /// `$HOME` stays as it is.
    pub fn expand(&self, text: &str) -> String {
        text.replace("/$HOME", &self.home)
            .replace("$HOMEBREW_PREFIX", &self.prefix)
            .replace("$HOMEBREW_CELLAR", &self.cellar())
            .replace("$APPDIR", &self.appdir)
    }

    /// `Formulary.replace_placeholders`, for a formula's.
    pub fn expand_formula(&self, text: &str) -> String {
        text.replace("$HOMEBREW_PREFIX", &self.prefix)
            .replace("$HOMEBREW_CELLAR", &self.cellar())
            .replace("/$HOME", &self.home)
    }

    fn cellar(&self) -> String {
        format!("{}/Cellar", self.prefix)
    }
}

/// `Messages`: the caveats of the packages a command installed, and the
/// formulae's completions, shown together at its end.
#[derive(Default)]
pub(crate) struct Messages {
    caveats: Vec<(String, String)>,
    completions_and_elisp: Vec<String>,
}

impl Messages {
    /// `FormulaInstaller#caveats` for an installed formula: only one
    /// installed on request has any.
    pub async fn record_formula(&mut self, installer: &nb_io::Installer, name: &str) {
        let Some(keg) = installer.get_installed(name) else {
            return;
        };
        if !keg.on_request {
            return;
        }
        let full_name = installer.installed_full_name(name);
        let Ok(json) = installer.formula_json(&full_name).await else {
            return;
        };
        let json = nb_io::installer::for_current_platform(&json);
        let keg_only = serde_json::from_value::<nb_core::Formula>(json.clone())
            .is_ok_and(|formula| formula.is_keg_only());
        let places = Places::new(installer, None);
        for text in completions_and_elisp(installer, name, keg_only) {
            if !self.completions_and_elisp.contains(&text) {
                self.completions_and_elisp.push(text);
            }
        }
        let caveats =
            formula_caveats(installer, &json, name, &full_name, keg_only, &places, true).await;
        if !caveats.is_empty() {
            self.caveats.push((name.to_string(), caveats));
        }
    }

    /// `Cask::Installer.record_caveats` for an installed cask (`token`
    /// without `cask:`).
    pub async fn record_cask(&mut self, installer: &nb_io::Installer, token: &str) {
        let key = format!("cask:{token}");
        let Some(keg) = installer.get_installed(&key) else {
            return;
        };
        let full_token = match &keg.tap {
            Some(tap) => format!("{tap}/{token}"),
            None => token.to_string(),
        };
        let Ok(json) = installer.cask(&full_token).await else {
            return;
        };
        let json = nb_io::installer::for_current_platform(&json);
        let config = nb_io::CaskConfig::load(&installer.keg_path(&key, &keg.version));
        let places = Places::new(installer, config.appdir());
        if let Some(caveats) = cask_caveats(&json, token, &places) {
            self.caveats.push((token.to_string(), caveats));
        }
    }

    /// `Messages#display_caveats` as `Install.finish_installation` calls
    /// it, forced: every package's, under its name.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.caveats.is_empty() && self.completions_and_elisp.is_empty() {
            return out;
        }
        out += &format!("{}\n", oh1("Caveats"));
        for text in &self.completions_and_elisp {
            out += &puts(text);
        }
        for (package, caveats) in &self.caveats {
            out += &format!("{}\n{}", ohai(package), puts(caveats));
        }
        out
    }
}

/// Ruby's `puts`: the text, ending in one newline.
fn puts(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

fn ohai(title: &str) -> String {
    format!("{} {}", style("==>").blue().bold(), style(title).bold())
}

fn oh1(title: &str) -> String {
    format!("{} {}", style("==>").green().bold(), style(title).bold())
}

/// A cask's caveats (`Cask::DSL::Caveats#to_s`): its own, as the API
/// has them, then Rosetta's, which the API leaves out as it depends on
/// the Mac.
pub(crate) fn cask_caveats(json: &Value, token: &str, places: &Places) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(caveats) = json["caveats"].as_str().filter(|c| !c.trim().is_empty()) {
        parts.push(format!("{}\n", places.expand(caveats).trim_end()));
    }
    if json["caveats_rosetta"].as_bool() == Some(true)
        && cfg!(target_arch = "aarch64")
        && !Path::new("/Library/Apple/usr/libexec/oahd").exists()
    {
        parts.push(format!(
            "{token} is built for Intel macOS and so requires Rosetta 2 to be installed.\n\
             You can install Rosetta 2 with:\n  \
             softwareupdate --install-rosetta --agree-to-license\n\
             Note that it is very difficult to remove Rosetta 2 once it is installed.\n"
        ));
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// `Caveats#caveats`: the formula's own, then why it is keg-only (and how
/// to use it so), then which of its commands others on `PATH` shadow
/// (with `shadowed_path`), then how to run its service.
pub(crate) async fn formula_caveats(
    installer: &nb_io::Installer,
    json: &Value,
    name: &str,
    full_name: &str,
    keg_only: bool,
    places: &Places,
    shadowed_path: bool,
) -> String {
    let mut parts = Vec::new();
    if let Some(caveats) = json["caveats"].as_str().filter(|c| !c.is_empty()) {
        let caveats = places.expand_formula(caveats);
        parts.push(format!(
            "{}\n",
            caveats.strip_suffix('\n').unwrap_or(&caveats)
        ));
    }
    let keg = installer.get_installed(name);
    let linked = keg.is_some() && installer.is_linked(name).unwrap_or(false);
    if keg_only && !linked {
        let keg_path = keg
            .as_ref()
            .map(|keg| installer.keg_path(name, &keg.version));
        parts.push(keg_only_text(
            name,
            &json["keg_only_reason"],
            Path::new(&places.prefix),
            keg_path.as_deref(),
        ));
    }
    if shadowed_path
        && let Some(keg) = &keg
        && let Some(text) = shadowed_path_text(
            name,
            Path::new(&places.prefix),
            &installer.keg_path(name, &keg.version),
            keg_only && !linked,
        )
    {
        parts.push(text);
    }
    if let Some(service) = json.get("service").filter(|s| !s.is_null()) {
        let home = std::path::PathBuf::from(&places.home);
        let service_places = nb_io::services::Places {
            prefix: installer.prefix().to_path_buf(),
            home: home.clone(),
        };
        if let Ok(Some(mut spec)) = nb_io::services::ServiceSpec::from_api(service, &service_places)
        {
            spec.set_env_file(nb_io::services::user_env_file(&home, name));
            let running = nb_io::services::Launchd::current(home)
                .find(&nb_io::services::label(name))
                .running();
            let sudo = if spec.requires_root() { "sudo " } else { "" };
            let mut lines = if running {
                vec![
                    format!("To restart {full_name} after an upgrade:"),
                    format!("  {sudo}nb services restart {full_name}"),
                ]
            } else if spec.requires_root() {
                vec![
                    format!("To start {full_name} now and restart at startup:"),
                    format!("  sudo nb services start {full_name}"),
                ]
            } else {
                vec![
                    format!("To start {full_name} now and restart at login:"),
                    format!("  nb services start {full_name}"),
                ]
            };
            lines.push("Or, if you don't want/need a background service you can just run:".into());
            lines.push(format!("  {}", spec.manual_command()));
            parts.push(format!("{}\n", lines.join("\n")));
        }
    }
    parts.join("\n")
}

/// `Caveats#keg_only_text`: why it was not linked, and for an installed
/// keg the lines that put it on `PATH` and before compilers.
pub(crate) fn keg_only_text(
    name: &str,
    reason: &Value,
    prefix: &Path,
    keg: Option<&Path>,
) -> String {
    let mut text = format!(
        "{name} is keg-only, which means it was not symlinked into {},\nbecause {}.\n",
        prefix.display(),
        keg_only_reason(reason)
    );
    let Some(keg) = keg else {
        return text;
    };
    let opt = prefix.join("opt").join(name);
    let shell = std::env::var("SHELL").ok();
    let dir = |sub: &str| keg.join(sub).is_dir();
    if dir("bin") || dir("sbin") {
        text += &format!("\nIf you need to have {name} first in your PATH, run:\n");
        for sub in ["bin", "sbin"] {
            if dir(sub) {
                text += &format!(
                    "  {}\n",
                    nb_io::installer::install::diagnostic::prepend_path_in_profile(
                        &opt.join(sub),
                        shell.as_deref()
                    )
                );
            }
        }
    }
    if dir("lib") || dir("include") {
        text += &format!("\nFor compilers to find {name} you may need to set:\n");
        if dir("lib") {
            let value = format!("-L{}", opt.join("lib").display());
            text += &format!("  {}\n", export_value("LDFLAGS", &value, shell.as_deref()));
        }
        if dir("include") {
            let value = format!("-I{}", opt.join("include").display());
            text += &format!("  {}\n", export_value("CPPFLAGS", &value, shell.as_deref()));
        }
        if which("pkgconf").is_some() && (dir("lib/pkgconfig") || dir("share/pkgconfig")) {
            text += &format!("\nFor pkgconf to find {name} you may need to set:\n");
            for sub in ["lib/pkgconfig", "share/pkgconfig"] {
                if dir(sub) {
                    let value = opt.join(sub).display().to_string();
                    text += &format!(
                        "  {}\n",
                        export_value("PKG_CONFIG_PATH", &value, shell.as_deref())
                    );
                }
            }
        }
        if which("cmake").is_some() && (dir("lib/cmake") || dir("share/cmake")) {
            text += &format!(
                "\nFor cmake to find {name} you may need to set:\n  {}\n",
                export_value(
                    "CMAKE_PREFIX_PATH",
                    &opt.display().to_string(),
                    shell.as_deref()
                )
            );
        }
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// `KegOnlyReason#to_s`.
pub(crate) fn keg_only_reason(reason: &Value) -> String {
    if let Some(explanation) = reason["explanation"].as_str().filter(|e| !e.is_empty()) {
        return explanation.trim_end().to_string();
    }
    match reason["reason"].as_str().unwrap_or_default() {
        ":versioned_formula" => "this is an alternate version of another formula".to_string(),
        ":provided_by_macos" => "macOS already provides this software and installing another version in\nparallel can cause all kinds of trouble".to_string(),
        ":shadowed_by_macos" => "macOS provides similar software and installing this software in\nparallel can cause all kinds of trouble".to_string(),
        other => other.trim_start_matches(':').trim_end().to_string(),
    }
}

/// `Caveats#shadowed_path_text`: the formula's commands that others
/// earlier on `PATH` run in place of, said apart from those of a linked
/// version of it. A keg-only formula not linked has only the latter.
fn shadowed_path_text(name: &str, prefix: &Path, keg: &Path, unlinked: bool) -> Option<String> {
    if env_set("HOMEBREW_NO_PATH_SHADOW_CHECK") {
        return None;
    }
    let cellar = keg.parent()?.parent()?;
    let opt = prefix.join("opt").join(name);
    let mut shadowed: Vec<Shadowed> = Vec::new();
    for dir in [opt.join("bin"), opt.join("sbin")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if !is_executable(&child) {
                continue;
            }
            let command = entry.file_name().to_string_lossy().into_owned();
            let Some(found) = which(&command) else {
                continue;
            };
            if found.canonicalize().ok() == child.canonicalize().ok() {
                continue;
            }
            let sibling = sibling_keg_name(name, cellar, &found);
            if unlinked && sibling.is_none() {
                continue;
            }
            shadowed.push((command, found, sibling));
        }
    }
    if shadowed.is_empty() {
        return None;
    }
    shadowed.sort_by(|a, b| a.0.cmp(&b.0));
    let (sibling, external): (Vec<_>, Vec<_>) = shadowed.into_iter().partition(|s| s.2.is_some());

    let mut blocks = Vec::new();
    if !external.is_empty() {
        blocks.push(format!(
            "{}\nRunning these by name will not invoke the version provided by nextbrew\n\
             because the shadowing commands come earlier in your PATH.\n",
            shadowed_columns(name, &external)
        ));
    }
    if !sibling.is_empty() {
        blocks.push(format!(
            "{}\nRunning these by name will not invoke the version provided by this formula.\n\
             Run `nb link {name}` to switch the active version to this keg.\n",
            shadowed_columns(name, &sibling)
        ));
    }
    let mut text = blocks.join("\n");
    if !env_set("HOMEBREW_NO_ENV_HINTS") {
        text += "Disable this behaviour by setting `HOMEBREW_NO_PATH_SHADOW_CHECK=1`.\n";
        text += "Hide these hints with `HOMEBREW_NO_ENV_HINTS=1`.\n";
    }
    Some(text)
}

/// A command, the one that shadows it, and the other version of the
/// formula that one is in.
type Shadowed = (String, PathBuf, Option<String>);

/// `Caveats#shadowed_columns`: the commands, by where the shadowing ones
/// are.
fn shadowed_columns(name: &str, shadowed: &[Shadowed]) -> String {
    let mut groups: Vec<(PathBuf, Option<String>, Vec<String>)> = Vec::new();
    for (command, found, sibling) in shadowed {
        let dir = found.parent().map(Path::to_path_buf).unwrap_or_default();
        match groups
            .iter_mut()
            .find(|(d, s, _)| *d == dir && s == sibling)
        {
            Some((_, _, names)) => names.push(command.clone()),
            None => groups.push((dir, sibling.clone(), vec![command.clone()])),
        }
    }
    let mut out = String::new();
    for (dir, sibling, names) in groups {
        let linked = sibling.map(|s| format!("linked {s} ")).unwrap_or_default();
        out += &format!(
            "The following {name} executables are shadowed by {linked}commands in {}:\n{}",
            dir.display(),
            columns(&names)
        );
    }
    out.strip_suffix('\n').unwrap_or(&out).to_string()
}

/// `Caveats#sibling_keg_name`: the other version of the formula (`foo`
/// or `foo@1`) whose keg `shadower` is in.
fn sibling_keg_name(name: &str, cellar: &Path, shadower: &Path) -> Option<String> {
    let target = shadower.canonicalize().ok()?;
    let cellar = cellar.canonicalize().ok()?;
    let other = target
        .strip_prefix(&cellar)
        .ok()?
        .components()
        .next()?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    let base = name.split('@').next().unwrap_or(name);
    let family = other == base || other.starts_with(&format!("{base}@"));
    (other != name && family).then_some(other)
}

/// `Formatter.columns`: one a line off a terminal, else in columns as
/// wide as the longest.
fn columns(names: &[String]) -> String {
    let fallback = || format!("{}\n", names.join("\n"));
    let term = console::Term::stdout();
    if names.is_empty() || !term.is_term() {
        return fallback();
    }
    let gap = 2;
    let width = term.size().1 as usize;
    let lengths: Vec<usize> = names.iter().map(|n| n.chars().count()).collect();
    let longest = lengths.iter().copied().max().unwrap_or(0);
    let cols = (width + gap) / (longest + gap);
    if cols < 2 {
        return fallback();
    }
    let rows = names.len().div_ceil(cols);
    let col_width = (width + gap) / cols - gap;
    let mut out = String::new();
    for row in 0..rows {
        let indices: Vec<usize> = (row..names.len()).step_by(rows).collect();
        let (last, rest) = indices.split_last().expect("a row has a name");
        let mut cells: Vec<String> = rest
            .iter()
            .map(|&i| format!("{}{}", names[i], " ".repeat(col_width - lengths[i])))
            .collect();
        cells.push(names[*last].clone());
        out += &cells.join(&" ".repeat(gap));
        out.push('\n');
    }
    out
}

/// `Caveats#completions_and_elisp`: where the formula's completions,
/// shell functions and Emacs Lisp went, for the user's shell (all four
/// when it is none of them).
fn completions_and_elisp(installer: &nb_io::Installer, name: &str, keg_only: bool) -> Vec<String> {
    let Some(installed) = installer.get_installed(name) else {
        return Vec::new();
    };
    let keg = installer.keg_path(name, &installed.version);
    let prefix = installer.prefix();
    let valid = ["bash", "zsh", "fish", "pwsh"];
    let shell = std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            Path::new(&s)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .map(|s| s.split('-').next().unwrap_or_default().to_string())
        .filter(|s| valid.contains(&s.as_str()));
    let shells: Vec<&str> = match &shell {
        Some(shell) => vec![shell.as_str()],
        None => valid.to_vec(),
    };
    let root = if keg_only {
        prefix.join("opt").join(name)
    } else {
        prefix.to_path_buf()
    };
    let root = root.display();
    let mut out = Vec::new();
    for shell in shells {
        if which(shell).is_none() {
            continue;
        }
        let (completions, functions) = installed_for(&keg, shell);
        if !completions && !functions {
            continue;
        }
        let what = match (completions, functions) {
            (true, true) => "completions and functions",
            (true, false) => "completions",
            _ => "functions",
        };
        out.push(match shell {
            "bash" => {
                format!("Bash completion has been installed to:\n  {root}/etc/bash_completion.d\n")
            }
            "fish" => {
                let mut text = format!("fish {what} have been installed to:");
                if completions {
                    text += &format!("\n  {root}/share/fish/vendor_completions.d");
                }
                if functions {
                    text += &format!("\n  {root}/share/fish/vendor_functions.d");
                }
                text
            }
            "zsh" => {
                format!("zsh {what} have been installed to:\n  {root}/share/zsh/site-functions\n")
            }
            _ => format!(
                "PowerShell completion has been installed to:\n  {root}/share/pwsh/completions\n"
            ),
        });
    }
    let elisp = keg.join("share/emacs/site-lisp").join(name);
    let has_elisp = std::fs::read_dir(&elisp).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            matches!(
                entry.path().extension().and_then(|e| e.to_str()),
                Some("el" | "elc")
            )
        })
    });
    if !keg_only && has_elisp {
        out.push(format!(
            "Emacs Lisp files have been installed to:\n  {}/share/emacs/site-lisp/{name}\n",
            prefix.display()
        ));
    }
    out
}

/// `Keg#completion_installed?` and `Keg#functions_installed?` for `shell`.
fn installed_for(keg: &Path, shell: &str) -> (bool, bool) {
    let names = |dir: &str| -> Vec<String> {
        std::fs::read_dir(keg.join(dir))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    match shell {
        "bash" => (!names("etc/bash_completion.d").is_empty(), false),
        "fish" => (
            !names("share/fish/vendor_completions.d").is_empty(),
            !names("share/fish/vendor_functions.d").is_empty(),
        ),
        "zsh" => {
            let files = names("share/zsh/site-functions");
            (
                files.iter().any(|f| f.starts_with('_')),
                files.iter().any(|f| !f.starts_with('_')),
            )
        }
        _ => (!names("share/pwsh/completions").is_empty(), false),
    }
}

/// `Utils::Shell.export_value` for the shell of `$SHELL`.
pub(crate) fn export_value(key: &str, value: &str, shell: Option<&str>) -> String {
    let shell = shell
        .and_then(|s| Path::new(s).file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let value = sh_quote(value);
    match shell.as_str() {
        "fish" => format!("set -gx {key} {value}"),
        "csh" | "tcsh" => format!("setenv {key} {value};"),
        _ => format!("export {key}={value}"),
    }
}

/// `Utils::Shell.sh_quote`: unchanged when safe, else backslash-escaped.
fn sh_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if !(c.is_ascii_alphanumeric() || "_-.,:/@~+\n".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `which`: the first executable `command` on `PATH`.
fn which(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn env_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn shows_every_packages_caveats_under_its_name() {
        let mut messages = Messages::default();
        assert_eq!(messages.render(), "");
        messages.completions_and_elisp.push(
            "zsh completions have been installed to:\n  /p/share/zsh/site-functions\n".into(),
        );
        messages.caveats.push(("a".into(), "Read this.\n".into()));
        messages.caveats.push(("b".into(), "And this.".into()));
        assert_eq!(
            console::strip_ansi_codes(&messages.render()),
            "==> Caveats\n\
             zsh completions have been installed to:\n  /p/share/zsh/site-functions\n\
             ==> a\nRead this.\n\
             ==> b\nAnd this.\n"
        );
    }

    #[test]
    fn a_casks_caveats_come_with_its_placeholders_filled() {
        let places = Places {
            prefix: "/p".into(),
            appdir: "/Applications".into(),
            home: "/Users/me".into(),
        };
        let json = json!({"caveats": "Run $APPDIR/A.app and /$HOME/x, not $HOME/y.\n\n"});
        assert_eq!(
            cask_caveats(&json, "a", &places).as_deref(),
            Some("Run /Applications/A.app and /Users/me/x, not $HOME/y.\n")
        );
        assert_eq!(
            places.expand_formula("$HOMEBREW_CELLAR/a and \"$HOME/.a\""),
            "/p/Cellar/a and \"$HOME/.a\""
        );
        assert_eq!(cask_caveats(&json!({}), "a", &places), None);
    }

    #[test]
    fn finds_a_linked_sibling_version() {
        let tmp = tempfile::tempdir().unwrap();
        let cellar = tmp.path().join("Cellar");
        let bin = cellar.join("python@3.12/3.12.1/bin/python3");
        executable(&bin);
        assert_eq!(
            sibling_keg_name("python@3.13", &cellar, &bin).as_deref(),
            Some("python@3.12")
        );
        assert_eq!(sibling_keg_name("python@3.12", &cellar, &bin), None);
        assert_eq!(sibling_keg_name("node", &cellar, &bin), None);
    }

    #[test]
    fn says_where_completions_went() {
        let tmp = tempfile::tempdir().unwrap();
        let keg = tmp.path();
        std::fs::create_dir_all(keg.join("share/zsh/site-functions")).unwrap();
        std::fs::write(keg.join("share/zsh/site-functions/_x"), "").unwrap();
        assert_eq!(installed_for(keg, "zsh"), (true, false));
        std::fs::write(keg.join("share/zsh/site-functions/x-fn"), "").unwrap();
        assert_eq!(installed_for(keg, "zsh"), (true, true));
        assert_eq!(installed_for(keg, "bash"), (false, false));
    }

    #[test]
    fn spells_shell_settings() {
        assert_eq!(
            keg_only_reason(&json!({"reason": ":versioned_formula", "explanation": ""})),
            "this is an alternate version of another formula"
        );
        assert_eq!(
            keg_only_reason(
                &json!({"reason": ":provided_by_macos", "explanation": "macOS provides BSD libedit"})
            ),
            "macOS provides BSD libedit"
        );
        assert_eq!(
            export_value("LDFLAGS", "-L/p/opt/x/lib", Some("/bin/zsh")),
            "export LDFLAGS=-L/p/opt/x/lib"
        );
        assert_eq!(
            export_value("CPPFLAGS", "-I/p/a b", Some("/usr/local/bin/fish")),
            "set -gx CPPFLAGS -I/p/a\\ b"
        );
    }
}
