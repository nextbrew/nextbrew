//! `brew bundle`'s extensions for other package managers: `mas` (the Mac
//! App Store), `vscode` (VS Code and its forks), `go`, `cargo`, `uv`,
//! `krew` and `npm`. As in Homebrew, nextbrew drives their own command-line
//! tools and installs those tools first when they are missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

use crate::brewfile::{self, Entry, Kind, Value};

/// The extensions nextbrew handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext {
    Mas,
    Vscode,
    Go,
    Cargo,
    Uv,
    Krew,
    Npm,
}

/// In the order Homebrew registers them, which is the order it dumps and
/// cleans them up in.
pub const HANDLED: &[Ext] = &[
    Ext::Mas,
    Ext::Vscode,
    Ext::Go,
    Ext::Cargo,
    Ext::Uv,
    Ext::Krew,
    Ext::Npm,
];

/// The order `brew bundle check` looks at extension entries in: the
/// registered extensions (`flatpak` and `winget` among them), then Mac App
/// Store apps.
pub const CHECK_ORDER: &[&str] = &[
    "vscode", "go", "cargo", "uv", "flatpak", "winget", "krew", "npm", "mas",
];

impl Ext {
    pub fn of(entry: &Entry) -> Option<Ext> {
        match entry.kind {
            Kind::Extension(name) => Ext::named(name),
            _ => None,
        }
    }

    pub fn named(name: &str) -> Option<Ext> {
        HANDLED.iter().copied().find(|ext| ext.name() == name)
    }

    /// The Brewfile directive.
    pub fn name(self) -> &'static str {
        match self {
            Ext::Mas => "mas",
            Ext::Vscode => "vscode",
            Ext::Go => "go",
            Ext::Cargo => "cargo",
            Ext::Uv => "uv",
            Ext::Krew => "krew",
            Ext::Npm => "npm",
        }
    }

    /// `check_label`.
    pub fn label(self) -> &'static str {
        match self {
            Ext::Mas => "App",
            Ext::Vscode => "VSCode Extension",
            Ext::Go => "Go Package",
            Ext::Cargo => "Cargo Package",
            Ext::Uv => "uv Tool",
            Ext::Krew => "Krew Plugin",
            Ext::Npm => "npm Package",
        }
    }

    /// `cleanup_heading` (`banner_name`).
    pub fn cleanup_heading(self) -> &'static str {
        match self {
            Ext::Mas => "Mac App Store apps",
            Ext::Vscode => "VSCode extensions",
            Ext::Go => "Go packages",
            Ext::Cargo => "Cargo packages",
            Ext::Uv => "uv tools",
            Ext::Krew => "Krew plugins",
            Ext::Npm => "npm packages",
        }
    }

    /// The tool's executable on `PATH` or in the prefix.
    pub fn executable(self, prefix: &Path) -> Option<PathBuf> {
        let names: &[&str] = match self {
            Ext::Mas => &["mas"],
            Ext::Vscode => &["code", "codium", "cursor", "code-insiders"],
            Ext::Go => &["go"],
            Ext::Cargo => &["cargo"],
            Ext::Uv => &["uv"],
            Ext::Krew => &["kubectl-krew"],
            Ext::Npm => &["npm"],
        };
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        dirs.push(prefix.join("bin"));
        names.iter().find_map(|name| {
            dirs.iter()
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
    }

    /// What nextbrew installs to get the tool: a formula or a cask.
    pub fn provider(self) -> &'static str {
        match self {
            Ext::Mas => "mas",
            Ext::Vscode => "cask:visual-studio-code",
            Ext::Go => "go",
            Ext::Cargo => "rust",
            Ext::Uv => "uv",
            Ext::Krew => "krew",
            Ext::Npm => "node",
        }
    }

    /// Why `name` cannot be installed when the tool could not be.
    pub fn tool_missing(self, name: &str) -> String {
        match self {
            Ext::Vscode => {
                format!("Unable to install {name} VSCode extension. VSCode is not installed.")
            }
            _ => format!(
                "Unable to install {name} {}. {} installation failed.",
                self.label().to_lowercase(),
                self.provider()
            ),
        }
    }

    /// `batch_installable?`: installed all at once after Homebrew's own
    /// batch, rather than one by one in the Brewfile's order.
    pub fn batched(self) -> bool {
        matches!(self, Ext::Mas | Ext::Vscode | Ext::Cargo | Ext::Npm)
    }

    /// `install_verb`.
    pub fn verb(self, entry: &Entry, installed: &Installed) -> &'static str {
        if self == Ext::Npm && installed.has(self, entry) {
            "Upgrading"
        } else {
            "Installing"
        }
    }

    /// `failure_reason` for `brew bundle check`.
    pub fn failure_reason(self, entry: &Entry, installed: &Installed, no_upgrade: bool) -> String {
        let needs = match self {
            Ext::Mas if no_upgrade => "needs to be installed.",
            Ext::Mas => "needs to be installed or updated.",
            Ext::Npm if !no_upgrade && installed.has(self, entry) => "needs to be upgraded.",
            _ => "needs to be installed.",
        };
        format!("{} {} {needs}", self.label(), entry.name)
    }
}

/// The tool run as Homebrew runs it: its own directory first on `PATH`,
/// and Go's and Cargo's locations taken from `HOMEBREW_GOBIN`,
/// `HOMEBREW_CARGO_HOME` and the like rather than the user's environment,
/// which `brew` does not pass on.
fn command(ext: Ext, exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    let mut path: Vec<PathBuf> = exe.parent().map(Path::to_path_buf).into_iter().collect();
    if let Some(current) = std::env::var_os("PATH") {
        path.extend(std::env::split_paths(&current));
    }
    if let Ok(path) = std::env::join_paths(path) {
        cmd.env("PATH", path);
    }
    let vars: &[(&str, &str)] = match ext {
        Ext::Go => &[("GOBIN", "HOMEBREW_GOBIN"), ("GOPATH", "HOMEBREW_GOPATH")],
        Ext::Cargo => &[
            ("CARGO_HOME", "HOMEBREW_CARGO_HOME"),
            ("CARGO_INSTALL_ROOT", "HOMEBREW_CARGO_INSTALL_ROOT"),
            ("RUSTUP_HOME", "HOMEBREW_RUSTUP_HOME"),
        ],
        _ => &[],
    };
    for (var, from) in vars {
        match std::env::var_os(from) {
            Some(value) => cmd.env(var, value),
            None => cmd.env_remove(var),
        };
    }
    cmd
}

fn read(ext: Ext, exe: &Path, args: &[&str]) -> String {
    command(ext, exe)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// `Bundle.system`: unless `verbose`, the tool's output (stdout and stderr
/// together) is kept back and shown only if it fails.
fn run(mut cmd: Command, verbose: bool) -> bool {
    if verbose {
        return cmd.status().is_ok_and(|status| status.success());
    }
    let Ok((mut reader, writer)) = std::io::pipe() else {
        return false;
    };
    let Ok(stdout) = writer.try_clone() else {
        return false;
    };
    cmd.stdout(stdout).stderr(writer);
    let child = cmd.spawn();
    // The command keeps its copies of the pipe's writing end until dropped.
    drop(cmd);
    let Ok(mut child) = child else {
        return false;
    };
    let mut logs = Vec::new();
    let _ = std::io::Read::read_to_end(&mut reader, &mut logs);
    let ok = child.wait().is_ok_and(|status| status.success());
    if !ok {
        print!("{}", String::from_utf8_lossy(&logs));
    }
    ok
}

/// An installed package as its tool lists it. `with` and `source` are only
/// for `uv` and `cargo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub with: Vec<String>,
    pub source: Option<String>,
}

impl Package {
    fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// What `entry` asks for, normalised as its extension compares it.
    fn wanted(ext: Ext, entry: &Entry) -> Self {
        let source = || entry.option("source").and_then(Value::as_str);
        match ext {
            Ext::Vscode => Self::named(entry.name.to_lowercase()),
            Ext::Cargo => Self {
                name: entry.name.trim().to_string(),
                with: Vec::new(),
                source: source().and_then(brewfile::cargo_source),
            },
            Ext::Uv => Self {
                name: uv_name(&entry.name),
                with: brewfile::uv_with(
                    match entry.option("with") {
                        Some(Value::Array(items)) => {
                            items.iter().filter_map(Value::as_str).collect()
                        }
                        _ => Vec::new(),
                    }
                    .into_iter(),
                ),
                source: source().and_then(brewfile::uv_source),
            },
            _ => Self::named(entry.name.clone()),
        }
    }
}

/// What a tool says is installed: Mac App Store `(id, name)`s, other
/// packages, and the outdated app ids or npm packages.
#[derive(Debug, Default)]
pub struct Installed {
    pub apps: Vec<(String, String)>,
    pub packages: Vec<Package>,
    pub outdated: Vec<String>,
}

impl Installed {
    pub fn read(ext: Ext, prefix: &Path) -> Self {
        let Some(exe) = ext.executable(prefix) else {
            return Self::default();
        };
        let read = |args: &[&str]| read(ext, &exe, args);
        let packages = |names: Vec<String>| names.into_iter().map(Package::named).collect();
        match ext {
            Ext::Mas => Self {
                apps: parse_mas_list(&read(&["list"])),
                outdated: read(&["outdated"])
                    .lines()
                    .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                    .collect(),
                ..Self::default()
            },
            Ext::Vscode => Self {
                packages: packages(
                    read(&["--list-extensions"])
                        .lines()
                        .map(str::trim)
                        .filter(|id| is_extension_id(id))
                        .map(str::to_lowercase)
                        .collect(),
                ),
                ..Self::default()
            },
            Ext::Go => {
                let mut paths: Vec<String> = Vec::new();
                for (_, path) in go_binaries(&exe) {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                Self {
                    packages: packages(paths),
                    ..Self::default()
                }
            }
            Ext::Cargo => Self {
                packages: parse_cargo_list(&read(&["install", "--list"])),
                ..Self::default()
            },
            Ext::Uv => Self {
                packages: parse_uv_list(&read(&[
                    "tool",
                    "list",
                    "--show-with",
                    "--show-extras",
                    "--show-version-specifiers",
                ])),
                ..Self::default()
            },
            Ext::Krew => Self {
                packages: packages(parse_krew_list(&read(&["list"]))),
                ..Self::default()
            },
            Ext::Npm => Self {
                packages: packages(npm_names(
                    &read(&["list", "-g", "--depth=0", "--json"]),
                    true,
                )),
                outdated: npm_names(&read(&["outdated", "-g", "--depth=0", "--json"]), false),
                ..Self::default()
            },
        }
    }

    pub fn has(&self, ext: Ext, entry: &Entry) -> bool {
        match ext {
            Ext::Mas => {
                app_id(entry).is_some_and(|id| self.apps.iter().any(|(app, _)| same_id(app, id)))
            }
            _ => self.packages.contains(&Package::wanted(ext, entry)),
        }
    }

    pub fn outdated(&self, ext: Ext, entry: &Entry) -> bool {
        match ext {
            Ext::Mas => {
                app_id(entry).is_some_and(|id| self.outdated.iter().any(|app| same_id(app, id)))
            }
            Ext::Npm => self.outdated.contains(&entry.name),
            _ => false,
        }
    }
}

fn same_id(app: &str, id: i64) -> bool {
    app.parse::<i64>().is_ok_and(|app| app == id)
}

pub fn app_id(entry: &Entry) -> Option<i64> {
    match entry.option("id") {
        Some(Value::Int(id)) => Some(*id),
        _ => None,
    }
}

/// `mas list`: `<id>  <name>  (<version>)` per line, read as Homebrew's
/// pattern `\A\s*(\d+)\s+(.*?)\s+\(?([\d.]*)\)?\Z` does.
fn parse_mas_list(text: &str) -> Vec<(String, String)> {
    text.lines().filter_map(parse_mas_line).collect()
}

fn parse_mas_line(row: &str) -> Option<(String, String)> {
    let row = row.trim_start();
    let id_end = row.find(|c: char| !c.is_ascii_digit())?;
    if id_end == 0 {
        return None;
    }
    let (id, rest) = row.split_at(id_end);
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // The version at the end, maybe in parentheses, after some whitespace.
    let mut tail = rest.trim_start();
    tail = tail.strip_suffix(')').unwrap_or(tail);
    tail = tail.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    tail = tail.strip_suffix('(').unwrap_or(tail);
    let name = tail.trim_end();
    if name.len() == tail.len() || name.is_empty() {
        return None;
    }
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    Some((id.to_string(), name))
}

/// `VscodeExtension::EXTENSION_ID_REGEX`.
fn is_extension_id(id: &str) -> bool {
    let Some((publisher, name)) = id.split_once('.') else {
        return false;
    };
    let ok = |part: &str, more: &str| {
        part.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || more.contains(c))
    };
    ok(publisher, "-") && ok(name, "-._")
}

/// The programs `go install` put in `$GOBIN` (or `$GOPATH/bin`), by file
/// name, each with the module path `go version -m` reads from it.
fn go_binaries(go: &Path) -> Vec<(PathBuf, String)> {
    let gobin = read(Ext::Go, go, &["env", "GOBIN"]).trim().to_string();
    let bin_dir = if gobin.is_empty() {
        let gopath = read(Ext::Go, go, &["env", "GOPATH"]).trim().to_string();
        PathBuf::from(format!("{gopath}/bin"))
    } else {
        PathBuf::from(gobin)
    };
    let Ok(dir) = std::fs::read_dir(&bin_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = dir
        .filter_map(Result::ok)
        .filter(|item| {
            use std::os::unix::fs::PermissionsExt;
            // Executable files, not directories or symlinks.
            item.metadata().is_ok_and(|meta| {
                meta.file_type().is_file() && meta.permissions().mode() & 0o111 != 0
            })
        })
        .map(|item| item.path())
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|binary| {
            let info = read(Ext::Go, go, &["version", "-m", &binary.to_string_lossy()]);
            let line = info
                .lines()
                .find(|line| line.trim().starts_with("path\t"))?;
            let path = line.split('\t').nth(2)?.trim().to_string();
            (path != "command-line-arguments").then_some((binary, path))
        })
        .collect()
}

/// `cargo install --list`: `name v1.2.3 (origin):` lines, each followed by
/// the indented programs it installed.
fn parse_cargo_list(text: &str) -> Vec<Package> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\A(?<name>[^\s:]+)\s+v[0-9A-Za-z.+-]+(?:\s+\((?<origin>[^)]+)\))?").unwrap()
    });
    let mut packages: Vec<Package> = Vec::new();
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(found) = LINE.captures(line) else {
            continue;
        };
        let package = Package {
            name: found["name"].to_string(),
            with: Vec::new(),
            source: found
                .name("origin")
                .and_then(|origin| brewfile::cargo_source(origin.as_str())),
        };
        if !packages.contains(&package) {
            packages.push(package);
        }
    }
    packages
}

/// `uv tool list --show-with --show-extras --show-version-specifiers`:
/// `name v1.2.3 [required: …] [extras: …] [with: …]` lines, each followed
/// by the tool's executables. Sorted by name.
fn parse_uv_list(text: &str) -> Vec<Package> {
    static LINE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A([A-Za-z0-9]\S*)\s+v\S+").unwrap());
    let bracket = |line: &str, key: &str| -> Option<String> {
        let start = line.find(&format!("[{key}:"))? + key.len() + 2;
        let rest = &line[start..];
        let end = rest.find(']')?;
        let inner = rest[..end].trim_start();
        (!inner.is_empty() && end > 0).then(|| inner.to_string())
    };
    let mut packages: Vec<Package> = text
        .lines()
        .filter_map(|line| {
            let name = LINE.captures(line)?.get(1)?.as_str();
            Some(Package {
                name: with_extras(name, bracket(line, "extras").as_deref()),
                with: uv_requirements(bracket(line, "with").as_deref()),
                source: bracket(line, "required").and_then(|required| uv_listed_source(&required)),
            })
        })
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

/// `name[a,b]`: the extras sorted, without duplicates.
fn with_extras(name: &str, extras: Option<&str>) -> String {
    let mut extras: Vec<&str> = extras
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|extra| !extra.is_empty())
        .collect();
    extras.sort();
    extras.dedup();
    if extras.is_empty() {
        name.to_string()
    } else {
        format!("{name}[{}]", extras.join(","))
    }
}

/// A Brewfile's `uv` name as Homebrew compares it: trimmed, its extras
/// sorted.
fn uv_name(name: &str) -> String {
    static NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A(?<base>[^\[\]]+)(?:\[(?<extras>[^\]]+)\])?\z").unwrap());
    let name = name.trim();
    match NAME.captures(name) {
        Some(found) => with_extras(
            found["base"].trim(),
            found.name("extras").map(|extras| extras.as_str()),
        ),
        None => name.to_string(),
    }
}

/// The `[with: …]` requirements, a version constraint split off by `, `
/// joined back to the requirement before it.
fn uv_requirements(with: Option<&str>) -> Vec<String> {
    static CONTINUATION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A(?:<=|>=|!=|==|~=|<|>)\s*\S").unwrap());
    static OPERATOR_SPACE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A(<=|>=|!=|==|~=|<|>)\s+").unwrap());
    let mut requirements: Vec<String> = Vec::new();
    for token in with.unwrap_or_default().split(", ") {
        let requirement = token.trim();
        if requirement.is_empty() {
            continue;
        }
        match requirements.last_mut() {
            Some(last) if CONTINUATION.is_match(requirement) => {
                *last = format!("{last}, {}", OPERATOR_SPACE.replace(requirement, "$1"));
            }
            _ => requirements.push(requirement.to_string()),
        }
    }
    requirements.sort();
    requirements.dedup();
    requirements
}

/// `[required: …]` as a Brewfile `source:`: only a git or web address.
fn uv_listed_source(required: &str) -> Option<String> {
    static REMOTE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\A(?:git\+|https?://)|\.git\z").unwrap());
    brewfile::uv_source(required).filter(|source| REMOTE.is_match(source))
}

/// `kubectl krew list`: the first word of each line.
fn parse_krew_list(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        if !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// The package names in `npm list --json` (under `dependencies`) or `npm
/// outdated --json` (at the top), without npm itself.
fn npm_names(json: &str, listed: bool) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let packages = if listed { &json["dependencies"] } else { &json };
    packages
        .as_object()
        .map(|packages| {
            packages
                .keys()
                .filter(|name| *name != "npm")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// `Language::Node.npm_install_security_args`.
fn npm_install_args(prefix: &Path) -> Vec<String> {
    vec![
        "--min-release-age=1".to_string(),
        format!("--cache={}", prefix.join("cache/npm_cache").display()),
        "--ignore-scripts".to_string(),
    ]
}

/// `cargo install`'s arguments for a `source:`: `--git <url>` and the
/// `?branch=`, `?tag=` or `?rev=` it selects.
fn cargo_source_args(source: Option<&str>) -> Vec<String> {
    let Some(source) = source.and_then(brewfile::cargo_source) else {
        return Vec::new();
    };
    let (url, query) = source.split_once('?').unwrap_or((&source, ""));
    let (key, value) = query.split_once('=').unwrap_or((query, ""));
    let mut args = vec!["--git".to_string(), url.to_string()];
    if brewfile::CARGO_GIT_REFERENCES.contains(&key) {
        args.extend([format!("--{key}"), value.to_string()]);
    }
    args
}

/// The newest `python@X.Y` installed in the prefix, which `uv` installs
/// tools with, as Homebrew points `UV_PYTHON` at it.
fn newest_python(prefix: &Path) -> Option<PathBuf> {
    static PYTHON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\Apython(?:@.+)?\z").unwrap());
    let version = |path: &Path| -> Option<(u64, u64)> {
        let name = path.file_name()?.to_str()?.strip_prefix("python")?;
        let (major, minor) = name.split_once('.')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    };
    std::fs::read_dir(prefix.join("opt"))
        .ok()?
        .filter_map(Result::ok)
        .filter(|formula| PYTHON.is_match(&formula.file_name().to_string_lossy()))
        .filter_map(|formula| std::fs::read_dir(formula.path().join("bin")).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|binary| binary.path())
        .filter(|path| path.is_file())
        .filter_map(|path| version(&path).map(|version| (version, path)))
        .max_by_key(|(version, _)| *version)
        .map(|(_, path)| path)
}

/// Install `entries` (all of one extension): together for those Homebrew
/// installs in one batch (`install_batch!`), one at a time otherwise.
/// Mac App Store apps already there are upgraded, the rest installed (`mas
/// get` if `mas install` fails).
pub fn install(
    ext: Ext,
    exe: &Path,
    prefix: &Path,
    entries: &[&Entry],
    installed: &Installed,
    verbose: bool,
) -> bool {
    let run_args = |args: Vec<String>| {
        let mut cmd = command(ext, exe);
        cmd.args(args);
        run(cmd, verbose)
    };
    let names = |list: &[&Entry]| -> Vec<String> { list.iter().map(|e| e.name.clone()).collect() };
    match ext {
        Ext::Mas => {
            let (outdated, fresh): (Vec<&Entry>, Vec<&Entry>) =
                entries.iter().partition(|entry| installed.has(ext, entry));
            let ids = |list: &[&Entry]| -> Vec<String> {
                list.iter()
                    .filter_map(|e| app_id(e))
                    .map(|id| id.to_string())
                    .collect()
            };
            let upgraded = outdated.is_empty()
                || run_args([vec!["upgrade".to_string()], ids(&outdated)].concat());
            let fresh_ok = fresh.is_empty()
                || run_args([vec!["install".to_string()], ids(&fresh)].concat())
                || run_args([vec!["get".to_string()], ids(&fresh)].concat());
            upgraded && fresh_ok
        }
        Ext::Vscode => run_args(
            entries
                .iter()
                .flat_map(|entry| ["--install-extension".to_string(), entry.name.clone()])
                .collect(),
        ),
        Ext::Npm => run_args(
            [
                vec!["install".to_string()],
                npm_install_args(prefix),
                vec!["-g".to_string()],
                names(entries),
            ]
            .concat(),
        ),
        Ext::Cargo => {
            // One `cargo install` per source, in the order they come.
            let mut groups: Vec<(Option<&str>, Vec<&Entry>)> = Vec::new();
            for entry in entries {
                let source = entry.option("source").and_then(Value::as_str);
                match groups.iter_mut().find(|(s, _)| *s == source) {
                    Some((_, group)) => group.push(entry),
                    None => groups.push((source, vec![entry])),
                }
            }
            groups.into_iter().fold(true, |ok, (source, group)| {
                let args = [
                    vec!["install".to_string(), "--locked".to_string()],
                    cargo_source_args(source),
                    names(&group),
                ]
                .concat();
                run_args(args) && ok
            })
        }
        Ext::Go => entries.iter().all(|entry| {
            run_args(vec![
                "install".to_string(),
                format!("{}@latest", entry.name),
            ])
        }),
        Ext::Krew => entries
            .iter()
            .all(|entry| run_args(vec!["install".to_string(), entry.name.clone()])),
        Ext::Uv => entries.iter().all(|entry| {
            let wanted = Package::wanted(ext, entry);
            let mut args = vec![
                "tool".to_string(),
                "install".to_string(),
                wanted.source.unwrap_or_else(|| entry.name.clone()),
            ];
            for requirement in wanted.with {
                args.extend(["--with".to_string(), requirement]);
            }
            let mut cmd = command(ext, exe);
            if let Some(python) = newest_python(prefix) {
                cmd.env("UV_PYTHON", python);
            }
            cmd.args(&args);
            run(cmd, verbose)
        }),
    }
}

/// `dump`: `mas "Name", id: 123` by name; the others as their tool lists
/// them, with `with:` and `source:` where they have them.
pub fn dump(ext: Ext, installed: &Installed) -> String {
    match ext {
        Ext::Mas => {
            let mut apps = installed.apps.clone();
            apps.sort_by_key(|(_, name)| name.to_lowercase());
            apps.iter()
                .map(|(id, name)| format!("mas {name:?}, id: {id}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => installed
            .packages
            .iter()
            .map(|package| {
                let mut line = format!("{} {:?}", ext.name(), package.name);
                if !package.with.is_empty() {
                    let with: Vec<String> = package.with.iter().map(|r| format!("{r:?}")).collect();
                    line.push_str(&format!(", with: [{}]", with.join(", ")));
                }
                if let Some(source) = &package.source {
                    line.push_str(&format!(", source: {source:?}"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// `cleanup_items`: what is installed but not in the Brewfile, when the
/// Brewfile lists any of this extension at all. Each is `(id, shown)`.
pub fn cleanup_items(ext: Ext, entries: &[Entry], installed: &Installed) -> Vec<(String, String)> {
    let kept: Vec<&Entry> = entries.iter().filter(|e| Ext::of(e) == Some(ext)).collect();
    if kept.is_empty() {
        return Vec::new();
    }
    match ext {
        Ext::Mas => {
            let mut apps = installed.apps.clone();
            apps.sort_by_key(|(_, name)| name.to_lowercase());
            apps.into_iter()
                .filter(|(id, _)| {
                    id.parse::<i64>().is_ok_and(|n| n != 0)
                        && !kept
                            .iter()
                            .any(|e| app_id(e).is_some_and(|k| same_id(id, k)))
                })
                .map(|(id, name)| (id.clone(), format!("{name} ({id})")))
                .collect()
        }
        Ext::Vscode => installed
            .packages
            .iter()
            .filter(|package| !kept.iter().any(|e| e.name.to_lowercase() == package.name))
            .map(|package| (package.name.clone(), package.name.clone()))
            .collect(),
        _ => installed
            .packages
            .iter()
            .filter(|package| !kept.iter().any(|e| e.name == package.name))
            .map(|package| (package.name.clone(), package.name.clone()))
            .collect(),
    }
}

/// `cleanup!`.
pub fn cleanup(ext: Ext, exe: &Path, items: &[(String, String)]) {
    let uninstall: &[&str] = match ext {
        Ext::Mas => &["uninstall"],
        Ext::Vscode => &["--uninstall-extension"],
        Ext::Cargo | Ext::Krew => &["uninstall"],
        Ext::Uv => &["tool", "uninstall"],
        Ext::Npm => &["uninstall", "-g"],
        Ext::Go => {
            // Go has no uninstall: the programs built from these modules go.
            let modules: Vec<&String> = items.iter().map(|(id, _)| id).collect();
            let mut removed = 0;
            for (binary, module) in go_binaries(exe) {
                if modules.contains(&&module) && std::fs::remove_file(&binary).is_ok() {
                    removed += 1;
                }
            }
            uninstalled(ext, removed);
            return;
        }
    };
    for (id, _) in items {
        let mut cmd = command(ext, exe);
        cmd.args(uninstall).arg(id);
        // VS Code's own output is shown (`Kernel.system`); the others'
        // only when they fail.
        run(cmd, ext == Ext::Vscode);
    }
    match ext {
        Ext::Vscode => {}
        Ext::Mas => {
            let n = items.len();
            println!(
                "Uninstalled {n} Mac App Store app{}",
                if n == 1 { "" } else { "s" }
            );
        }
        _ => uninstalled(ext, items.len()),
    }
}

/// `Extension.cleanup!`'s count. Homebrew adds an `s` to the already plural
/// heading (`Uninstalled 2 Go packagess`); nextbrew prints it the same.
fn uninstalled(ext: Ext, n: usize) {
    println!(
        "Uninstalled {n} {}{}",
        ext.cleanup_heading(),
        if n == 1 { "" } else { "s" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mas_list_and_extension_ids() {
        let apps =
            parse_mas_list("  497799835  Xcode       (16.0)\n409183694 Keynote  (14.2)\nnoise\n");
        assert_eq!(
            apps,
            [
                ("497799835".to_string(), "Xcode".to_string()),
                ("409183694".to_string(), "Keynote".to_string())
            ]
        );
        assert!(is_extension_id("golang.Go"));
        assert!(is_extension_id("ms-python.python"));
        assert!(!is_extension_id("Extensions installed on SSH:"));
        assert!(!is_extension_id("nodot"));
    }

    #[test]
    fn dumps_and_cleans_up_like_homebrew() {
        let installed = Installed {
            apps: vec![("2".into(), "beta".into()), ("1".into(), "Alpha".into())],
            ..Default::default()
        };
        let extensions = Installed {
            packages: vec![
                Package::named("golang.go"),
                Package::named("ms-python.python"),
            ],
            ..Default::default()
        };
        assert_eq!(
            dump(Ext::Mas, &installed),
            "mas \"Alpha\", id: 1\nmas \"beta\", id: 2"
        );
        assert_eq!(
            dump(Ext::Vscode, &extensions),
            "vscode \"golang.go\"\nvscode \"ms-python.python\""
        );
        let entries =
            crate::brewfile::parse("mas \"Alpha\", id: 1\nvscode \"GoLang.Go\"\n").unwrap();
        assert_eq!(
            cleanup_items(Ext::Mas, &entries, &installed),
            [("2".to_string(), "beta (2)".to_string())]
        );
        assert_eq!(
            cleanup_items(Ext::Vscode, &entries, &extensions),
            [(
                "ms-python.python".to_string(),
                "ms-python.python".to_string()
            )]
        );
        // Nothing is cleaned for a kind the Brewfile does not list.
        assert!(cleanup_items(Ext::Mas, &[], &installed).is_empty());
    }

    #[test]
    fn reads_cargo_and_uv_lists() {
        let cargo = parse_cargo_list(
            "ripgrep v14.1.1:\n    rg\nfoo v0.1.0 (https://github.com/x/foo?branch=main#0123abc):\n    foo\nbar v0.2.0 (/Users/me/bar):\n    bar\n",
        );
        assert_eq!(
            cargo,
            [
                Package::named("ripgrep"),
                Package {
                    name: "foo".into(),
                    with: vec![],
                    source: Some("https://github.com/x/foo?branch=main".into())
                },
                Package::named("bar"),
            ]
        );
        assert_eq!(
            cargo_source_args(Some("https://github.com/x/foo?branch=main")),
            ["--git", "https://github.com/x/foo", "--branch", "main"]
        );

        let uv = parse_uv_list(
            "ruff v0.6.0 [required: ==0.6.0]\n- ruff\nblack v24.1.0 [extras: jupyter, d] [with: requests>=2, < 3, attrs]\n- black\nmytool v1.0 [required: git+https://github.com/x/mytool]\n- mytool\n",
        );
        assert_eq!(
            uv,
            [
                Package {
                    name: "black[d,jupyter]".into(),
                    with: vec!["attrs".into(), "requests>=2, <3".into()],
                    source: None
                },
                Package {
                    name: "mytool".into(),
                    with: vec![],
                    source: Some("git+https://github.com/x/mytool".into())
                },
                Package::named("ruff"),
            ]
        );
        assert_eq!(
            dump(
                Ext::Uv,
                &Installed {
                    packages: uv,
                    ..Default::default()
                }
            ),
            "uv \"black[d,jupyter]\", with: [\"attrs\", \"requests>=2, <3\"]\n\
             uv \"mytool\", source: \"git+https://github.com/x/mytool\"\n\
             uv \"ruff\""
        );
        let entries = crate::brewfile::parse(
            "uv \"black[jupyter,d]\", with: [\"requests>=2, <3\", \"attrs\"]\n",
        )
        .unwrap();
        let installed = Installed {
            packages: parse_uv_list(
                "black v24.1.0 [extras: jupyter, d] [with: requests>=2, < 3, attrs]\n",
            ),
            ..Default::default()
        };
        assert!(installed.has(Ext::Uv, &entries[0]));
    }

    #[test]
    fn reads_npm_and_krew_lists() {
        assert_eq!(
            npm_names(
                r#"{"dependencies": {"npm": {}, "typescript": {}, "corepack": {}}}"#,
                true
            ),
            ["corepack", "typescript"]
        );
        assert_eq!(
            npm_names(r#"{"typescript": {"current": "5.0.0"}}"#, false),
            ["typescript"]
        );
        assert!(npm_names("not json", true).is_empty());
        assert_eq!(
            parse_krew_list("ctx\nns  \n\nctx\n"),
            ["ctx".to_string(), "ns".to_string()]
        );
    }
}
