//! `brew bundle`'s extensions for other package managers: `mas` (the Mac
//! App Store), `vscode` (VS Code and its forks), `go`, `cargo`, `uv`,
//! `flatpak`, `winget` (from WSL), `krew` and `npm`. As in Homebrew,
//! nextbrew drives their own command-line tools and installs those tools
//! first when they are missing (but not Flatpak or WinGet).

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
    Flatpak,
    Winget,
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
    Ext::Flatpak,
    Ext::Winget,
    Ext::Krew,
    Ext::Npm,
];

/// The order `brew bundle check` looks at extension entries in: the
/// registered extensions, then Mac App Store apps.
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
            Ext::Flatpak => "flatpak",
            Ext::Winget => "winget",
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
            Ext::Flatpak => "Flatpak",
            Ext::Winget => "WinGet Package",
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
            Ext::Flatpak => "flatpaks",
            Ext::Winget => "WinGet packages",
            Ext::Krew => "Krew plugins",
            Ext::Npm => "npm packages",
        }
    }

    /// The tool's executable on `PATH` or in the prefix.
    pub fn executable(self, prefix: &Path) -> Option<PathBuf> {
        let names: &[&str] = match self {
            Ext::Winget => return winget_executable(),
            Ext::Flatpak => &["flatpak"],
            Ext::Mas => &["mas"],
            Ext::Vscode => &["code", "codium", "cursor", "code-insiders"],
            Ext::Go => &["go"],
            Ext::Cargo => &["cargo"],
            Ext::Uv => &["uv"],
            Ext::Krew => &["kubectl-krew"],
            Ext::Npm => &["npm"],
        };
        names.iter().find_map(|name| {
            which(name).or_else(|| Some(prefix.join("bin").join(name)).filter(|p| p.is_file()))
        })
    }

    /// What nextbrew installs to get the tool: a formula or a cask. Homebrew
    /// leaves Flatpak and WinGet to the system.
    pub fn provider(self) -> Option<&'static str> {
        Some(match self {
            Ext::Mas => "mas",
            Ext::Vscode => "cask:visual-studio-code",
            Ext::Go => "go",
            Ext::Cargo => "rust",
            Ext::Uv => "uv",
            Ext::Krew => "krew",
            Ext::Npm => "node",
            Ext::Flatpak | Ext::Winget => return None,
        })
    }

    /// Why `name` cannot be installed when the tool could not be.
    pub fn tool_missing(self, name: &str) -> String {
        match self {
            Ext::Vscode => {
                format!("Unable to install {name} VSCode extension. VSCode is not installed.")
            }
            Ext::Flatpak => {
                "flatpak is not installed. Install it with your distribution's package manager."
                    .to_string()
            }
            Ext::Winget => {
                format!("Unable to install {name} WinGet package. winget.exe is not installed.")
            }
            _ => format!(
                "Unable to install {name} {}. {} installation failed.",
                self.label().to_lowercase(),
                self.provider().unwrap_or_default()
            ),
        }
    }

    /// `batch_installable?`: installed all at once after Homebrew's own
    /// batch, rather than one by one in the Brewfile's order. Flatpaks are,
    /// unless their remote is an address.
    pub fn batched(self, entry: &Entry) -> bool {
        match self {
            Ext::Mas | Ext::Vscode | Ext::Cargo | Ext::Npm => true,
            Ext::Flatpak => !is_url(flatpak_remote(entry).0),
            _ => false,
        }
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

/// `which`: `name` in a `PATH` directory.
fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `Bundle.system`: unless `verbose`, the tool's output (stdout and stderr
/// together) is kept back and shown only if it fails.
fn run(mut cmd: Command, verbose: bool) -> bool {
    if verbose {
        return cmd.status().is_ok_and(|status| status.success());
    }
    let (ok, logs) = capture(cmd);
    if !ok {
        print!("{logs}");
    }
    ok
}

/// Whether `cmd` succeeded, and its stdout and stderr together.
fn capture(mut cmd: Command) -> (bool, String) {
    let Ok((mut reader, writer)) = std::io::pipe() else {
        return (false, String::new());
    };
    let Ok(stdout) = writer.try_clone() else {
        return (false, String::new());
    };
    cmd.stdout(stdout).stderr(writer);
    let child = cmd.spawn();
    // The command keeps its copies of the pipe's writing end until dropped.
    drop(cmd);
    let Ok(mut child) = child else {
        return (false, String::new());
    };
    let mut logs = Vec::new();
    let _ = std::io::Read::read_to_end(&mut reader, &mut logs);
    let ok = child.wait().is_ok_and(|status| status.success());
    (ok, String::from_utf8_lossy(&logs).into_owned())
}

/// An installed package as its tool lists it. `with` is only for `uv`;
/// `source` for `uv`, `cargo`, `winget` and (its remote) `flatpak`; `id`
/// (the package identifier) for `winget`; `url` (its remote's) for
/// `flatpak`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub with: Vec<String>,
    pub source: Option<String>,
    pub id: Option<String>,
    pub url: Option<String>,
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
                ..Self::default()
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
                ..Self::default()
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
            Ext::Flatpak => Self {
                packages: parse_flatpak_list(
                    &read(&["list", "--app", "--columns=application,origin"]),
                    &parse_flatpak_remotes(&read(&[
                        "remote-list",
                        "--system",
                        "--columns=name,url",
                    ])),
                ),
                ..Self::default()
            },
            Ext::Winget => Self {
                packages: WINGET_SOURCES
                    .iter()
                    .flat_map(|source| winget_apps(&exe, source))
                    .collect(),
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
            // `installed_and_up_to_date?`: from the remote asked for, a URL
            // one being `<app-id>-origin` (any for a `.flatpakref`).
            Ext::Flatpak => {
                let (remote, url) = flatpak_remote(entry);
                let remote = match remote {
                    remote if url.is_some() || !is_url(remote) => remote.to_string(),
                    remote if remote.ends_with(".flatpakref") => return self.present(ext, entry),
                    _ => format!("{}-origin", entry.name),
                };
                self.packages
                    .iter()
                    .any(|p| p.name == entry.name && p.source.as_deref() == Some(remote.as_str()))
            }
            // `app_installed?`: the id in any case, from the same source.
            Ext::Winget => {
                let (id, source) = winget_id(entry);
                self.packages.iter().any(|p| {
                    p.id.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(id))
                        && p.source.as_deref() == Some(source)
                })
            }
            _ => self.packages.contains(&Package::wanted(ext, entry)),
        }
    }

    /// `preinstall!`'s check: as `has`, but a Flatpak from any remote.
    pub fn present(&self, ext: Ext, entry: &Entry) -> bool {
        match ext {
            Ext::Flatpak => self.packages.iter().any(|p| p.name == entry.name),
            _ => self.has(ext, entry),
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
            ..Package::default()
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
                ..Package::default()
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

fn is_url(remote: &str) -> bool {
    remote.starts_with("http://") || remote.starts_with("https://")
}

/// A `flatpak` entry's `remote:` (`flathub` by default) and `url:`.
fn flatpak_remote(entry: &Entry) -> (&str, Option<&str>) {
    let get = |key| entry.option(key).and_then(Value::as_str);
    (
        get("remote").unwrap_or("flathub"),
        get("url").filter(|url| !url.is_empty()),
    )
}

/// `remote_urls`: `flatpak remote-list --columns=name,url`, tab-separated.
fn parse_flatpak_remotes(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (name, url) = line.trim().split_once('\t')?;
            let url = url.split('\t').next()?;
            (!name.is_empty() && !url.is_empty()).then(|| (name.to_string(), url.to_string()))
        })
        .collect()
}

/// `packages_with_remotes`: `flatpak list --app
/// --columns=application,origin`, each with its remote's URL, by name.
fn parse_flatpak_list(text: &str, remotes: &[(String, String)]) -> Vec<Package> {
    let mut packages: Vec<Package> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().split('\t');
            let name = parts.next().filter(|name| !name.is_empty())?;
            let remote = parts.next().filter(|r| !r.is_empty()).unwrap_or("flathub");
            Some(Package {
                name: name.to_string(),
                source: Some(remote.to_string()),
                url: remotes
                    .iter()
                    .rev()
                    .find(|(known, _)| known == remote)
                    .map(|(_, url)| url.clone()),
                ..Package::default()
            })
        })
        .collect();
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    packages
}

/// `dump_entry`: nothing for `flathub`, the URL alone for a single-app
/// `-origin` remote, the remote's name and URL for a shared one.
fn flatpak_dump_line(package: &Package) -> String {
    let remote = package.source.as_deref().unwrap_or("flathub");
    let line = format!("flatpak {:?}", package.name);
    match &package.url {
        _ if remote == "flathub" => line,
        Some(url) if remote.ends_with("-origin") => format!("{line}, remote: {url:?}"),
        Some(url) => format!("{line}, remote: {remote:?}, url: {url:?}"),
        None => format!("{line}, remote: {remote:?}"),
    }
}

/// `ensure_named_remote_exists!` (`replace` false) and
/// `ensure_single_app_remote_exists!` (`replace` true): a remote at `url`
/// called `name`, an existing one at another address replaced or kept.
fn ensure_flatpak_remote(exe: &Path, name: &str, url: &str, replace: bool, verbose: bool) {
    let remotes = read(
        Ext::Flatpak,
        exe,
        &["remote-list", "--system", "--columns=name,url"],
    );
    let flatpak = |args: &[&str]| {
        let mut cmd = command(Ext::Flatpak, exe);
        cmd.args(args);
        run(cmd, verbose)
    };
    match parse_flatpak_remotes(&remotes)
        .into_iter()
        .find(|(known, _)| known == name)
    {
        Some((_, existing)) if existing == url => return,
        Some((_, existing)) if !replace => {
            println!(
                "Warning: Remote '{name}' exists with different URL ({existing}), using existing"
            );
            return;
        }
        Some(_) => {
            flatpak(&["remote-delete", "--system", "--force", name]);
        }
        None => {}
    }
    // `add_remote!`: a bare repository without GPG verification.
    if url.ends_with(".flatpakrepo") {
        flatpak(&["remote-add", "--if-not-exists", "--system", name, url]);
    } else {
        flatpak(&[
            "remote-add",
            "--if-not-exists",
            "--system",
            "--no-gpg-verify",
            name,
            url,
        ]);
    }
}

/// `Winget::SOURCES`, the first the default.
const WINGET_SOURCES: &[&str] = &["winget", "msstore"];

/// A `winget` entry's `id:` (its name by default) and `source:`.
fn winget_id(entry: &Entry) -> (&str, &str) {
    let get = |key| entry.option(key).and_then(Value::as_str);
    (
        get("id").unwrap_or(&entry.name),
        get("source").unwrap_or(WINGET_SOURCES[0]),
    )
}

/// `package_manager_executable`: under WSL only, `winget.exe` on `PATH` or
/// in the Windows user's `WindowsApps`.
fn winget_executable() -> Option<PathBuf> {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    if !kernel.to_lowercase().contains("-microsoft") {
        return None;
    }
    if let Some(exe) = which("winget.exe") {
        return Some(exe);
    }
    // `windows_local_appdata`.
    let cmd = which("cmd.exe").unwrap_or_else(|| "/mnt/c/Windows/System32/cmd.exe".into());
    let local_appdata = cmd
        .is_file()
        .then(|| read(Ext::Winget, &cmd, &["/d", "/c", "echo", "%LOCALAPPDATA%"]))
        .map(|out| out.trim().to_string())
        .filter(|dir| !dir.is_empty());
    let apps = "\\Microsoft\\WindowsApps\\winget.exe";
    [
        std::env::var("LOCALAPPDATA").ok().map(|dir| dir + apps),
        std::env::var("USERPROFILE")
            .ok()
            .map(|dir| dir + "\\AppData\\Local" + apps),
        local_appdata.map(|dir| dir + apps),
    ]
    .into_iter()
    .flatten()
    .filter(|path| !path.contains('%'))
    .filter_map(|path| wsl_path(&path))
    .find(|path| path.is_file())
}

/// `windows_path_to_wsl_path`: `C:\x` as `/mnt/c/x`.
fn wsl_path(path: &str) -> Option<PathBuf> {
    let path = path.replace('\\', "/");
    if path.starts_with('/') {
        return Some(path.into());
    }
    let (drive, rest) = path.split_once(":/")?;
    (drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic()) && !rest.is_empty())
        .then(|| format!("/mnt/{}/{rest}", drive.to_lowercase()).into())
}

/// `windows_export_path`: `wslpath -w`, or the path as it is.
fn windows_path(path: &str) -> String {
    which("wslpath")
        .and_then(|wslpath| Command::new(wslpath).args(["-w", path]).output().ok())
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| path.to_string())
}

/// `export_apps`: what `winget export` has from `source`, named as `winget
/// list` shows them.
fn winget_apps(winget: &Path, source: &str) -> Vec<Package> {
    let names = parse_winget_names(&read(
        Ext::Winget,
        winget,
        &[
            "list",
            "--source",
            source,
            "--accept-source-agreements",
            "--disable-interactivity",
            "--nowarn",
        ],
    ));
    winget_export(winget, source)
        .into_iter()
        .map(|id| Package {
            name: names
                .iter()
                .rev()
                .find(|(known, _)| *known == id.to_lowercase())
                .map_or_else(|| id.clone(), |(_, name)| name.clone()),
            id: Some(id),
            source: Some(source.to_string()),
            ..Package::default()
        })
        .collect()
}

/// `exported_apps`: the package ids `winget export` writes for `source`.
fn winget_export(winget: &Path, source: &str) -> Vec<String> {
    let Ok(file) = tempfile::Builder::new()
        .prefix("brew-bundle-winget")
        .suffix(".json")
        .tempfile()
    else {
        return Vec::new();
    };
    let output = windows_path(&file.path().to_string_lossy());
    let exported = command(Ext::Winget, winget)
        .args(["export", "--source", source, "--output", &output])
        .args(["--accept-source-agreements", "--disable-interactivity"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !exported {
        return Vec::new();
    }
    parse_winget_export(&std::fs::read_to_string(file.path()).unwrap_or_default())
}

/// `parse_export`: each `Sources[].Packages[].PackageIdentifier`.
fn parse_winget_export(json: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let list = |value: &serde_json::Value| value.as_array().cloned().unwrap_or_default();
    list(&json["Sources"])
        .iter()
        .flat_map(|source| list(&source["Packages"]))
        .filter_map(|package| package["PackageIdentifier"].as_str().map(str::to_string))
        .filter(|id| !id.trim().is_empty())
        .collect()
}

/// `parse_list_names`: `(lowercased id, name)` from `winget list`'s table,
/// its columns where the `Name  Id  Version` header puts them.
fn parse_winget_names(text: &str) -> Vec<(String, String)> {
    static HEADER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bName\s+Id\s+Version\b").unwrap());
    let text = text.replace('\r', "");
    let mut lines = text.lines().skip_while(|line| !HEADER.is_match(line));
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    // Columns in characters, as Ruby counts them.
    let header: Vec<char> = header.chars().collect();
    let column = |word: &str, from: usize| {
        let word: Vec<char> = word.chars().collect();
        (from..header.len()).find(|&at| header[at..].starts_with(&word))
    };
    let Some(start) = column("Name", 0) else {
        return Vec::new();
    };
    let (Some(id_at), Some(version_at)) = (column("Id", start), column("Version", start)) else {
        return Vec::new();
    };
    let cut = |line: &[char], from: usize, to: usize| -> String {
        line.get(from..to.min(line.len()))
            .unwrap_or_default()
            .iter()
            .collect()
    };
    lines
        .filter_map(|line| {
            let line: Vec<char> = line.chars().collect();
            let rest = cut(&line, start, usize::MAX);
            if line.iter().all(|c| c.is_whitespace())
                || !rest.is_empty() && rest.chars().all(|c| c == '-')
            {
                return None;
            }
            let name = cut(&line, start, id_at).trim().to_string();
            let id = cut(&line, id_at, version_at).trim().to_lowercase();
            (!name.is_empty() && !id.is_empty()).then_some((id, name))
        })
        .collect()
}

/// `internal_package?`: what Windows itself installs, never dumped or
/// cleaned up.
fn winget_internal(id: &str, name: &str) -> bool {
    static INTERNAL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)\AApp Installer\z|\A9NBLGGH4NNS1\z|\AMicrosoft Store\z",
            r"|\AStore Experience Host\z|\AWindows (?:Feature|Web) Experience Pack\z",
            r"|\AMicrosoft Edge WebView2 Runtime\z|\AMicrosoft Visual C\+\+|\AWindows App Runtime",
            r"|\AMicrosoft\.(?:AppInstaller|DesktopAppInstaller|DirectX|DotNet|Edge|EdgeWebView2Runtime|GameInput)\b",
            r"|\AMicrosoft\.(?:HEVCVideoExtension|NET\.Native|RawImageExtension)\b",
            r"|\AMicrosoft\.(?:OneDrive|WSL)\z",
            r"|\AMicrosoft\.(?:Services\.Store\.Engagement|StorePurchaseApp|UI\.Xaml|VCLibs|WindowsAppRuntime)\b",
            r"|\AMicrosoft\.(?:WindowsStore|WebMediaExtensions|WebpImageExtension|VP9VideoExtensions)\b",
            r"|\AMicrosoft\.VCRedist\.|\ANvidia\.PhysX\z",
        ))
        .unwrap()
    });
    INTERNAL.is_match(id) || INTERNAL.is_match(name)
}

/// `packages`: the apps not Windows' own, by source then name.
fn winget_packages(installed: &Installed) -> Vec<&Package> {
    let mut apps: Vec<&Package> = installed
        .packages
        .iter()
        .filter(|app| !winget_internal(app.id.as_deref().unwrap_or_default(), &app.name))
        .collect();
    let source = |app: &Package| {
        WINGET_SOURCES
            .iter()
            .position(|s| app.source.as_deref() == Some(*s))
            .unwrap_or(WINGET_SOURCES.len())
    };
    apps.sort_by_key(|app| (source(app), app.name.to_lowercase()));
    apps
}

/// `Winget.install!`: elevated through PowerShell if the installer seems to
/// want it, with a hint on how to install it by hand if that fails too.
fn winget_install(exe: &Path, entry: &Entry, verbose: bool) -> bool {
    static ELEVATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)Installer failed with exit code:\s*1603|\b(?:admin|administrator|elevat|UAC)\b",
        )
        .unwrap()
    });
    static INSTALLER_UI: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(?:interactive|user input|user cancelled)\b").unwrap());
    let (id, source) = winget_id(entry);
    let args = [
        "install",
        "--id",
        id,
        "--exact",
        "--source",
        source,
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--disable-interactivity",
    ];
    let mut cmd = command(Ext::Winget, exe);
    cmd.args(args);
    let (mut ok, mut output) = capture(cmd);
    if verbose {
        print!("{output}");
    }
    if !ok && ELEVATION.is_match(&output) {
        println!(
            "WinGet install for {} may require Windows UAC/elevation; retrying elevated.",
            entry.name
        );
        let elevated;
        (ok, elevated) = winget_elevated(exe, &args, verbose);
        if !elevated.is_empty() {
            output = elevated;
        }
    }
    if ok {
        return true;
    }
    // `report_install_failure`.
    println!(
        "WinGet failed to install {} ({id}) from {source}.",
        entry.name
    );
    let manual = format!("winget install --id {id} --exact --source {source}");
    if ELEVATION.is_match(&output) {
        println!("The installer may require Windows UAC/elevation.");
        println!("Try installing it from an elevated Windows Terminal:");
        println!("  {manual} --disable-interactivity");
    } else if INSTALLER_UI.is_match(&output) {
        println!(
            "The installer appears to require installer UI or user input, which brew bundle does not automate."
        );
        println!("Install it manually from Windows:");
        println!("  {manual}");
    } else {
        println!("Try installing it manually from Windows:");
        println!("  {manual}");
    }
    false
}

/// `run_elevated_install_command`: `winget` through PowerShell's
/// `Start-Process -Verb RunAs`.
fn winget_elevated(exe: &Path, args: &[&str], verbose: bool) -> (bool, String) {
    let powershell = which("powershell.exe")
        .unwrap_or_else(|| "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe".into());
    if !powershell.is_file() {
        return (false, "powershell.exe is not available.\n".to_string());
    }
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    let winget = exe.to_string_lossy();
    let winget = if winget.contains('/') {
        windows_path(&winget)
    } else {
        winget.into_owned()
    };
    let arguments: Vec<String> = args.iter().map(|arg| quote(arg)).collect();
    let script = format!(
        "$startProcessArgs = @{{\n  FilePath = {}\n  ArgumentList = @({})\n  Verb = 'RunAs'\n  Wait = $true\n  PassThru = $true\n}}\n$process = Start-Process @startProcessArgs\n$process.WaitForExit()\nexit $process.ExitCode\n",
        quote(&winget),
        arguments.join(", ")
    );
    let mut cmd = Command::new(powershell);
    cmd.args(["-NoProfile", "-Command", &script]);
    (run(cmd, verbose), String::new())
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
        Ext::Flatpak => {
            // `install!`'s three kinds of remote: a named one (with its
            // `url:` added if missing), a `.flatpakref` installed as it is,
            // or another address as the app's own `<app-id>-origin`. One
            // `flatpak install` per named remote (`install_batch!`).
            let install = |remote: &str, names: Vec<String>| {
                let args = ["install", "-y", "--system", remote].map(str::to_string);
                run_args([args.to_vec(), names].concat())
            };
            let mut ok = true;
            let mut groups: Vec<(&str, Option<&str>, Vec<String>)> = Vec::new();
            for entry in entries {
                let (remote, url) = flatpak_remote(entry);
                let name = entry.name.clone();
                if url.is_some() || !is_url(remote) {
                    match groups
                        .iter_mut()
                        .find(|(r, u, _)| (*r, *u) == (remote, url))
                    {
                        Some((_, _, names)) => names.push(name),
                        None => groups.push((remote, url, vec![name])),
                    }
                } else if remote.ends_with(".flatpakref") {
                    ok = install(remote, Vec::new()) && ok;
                } else {
                    let origin = format!("{}-origin", entry.name);
                    ensure_flatpak_remote(exe, &origin, remote, true, verbose);
                    ok = install(&origin, vec![entry.name.clone()]) && ok;
                }
            }
            for (remote, url, names) in groups {
                if let Some(url) = url {
                    ensure_flatpak_remote(exe, remote, url, false, verbose);
                }
                ok = install(remote, names) && ok;
            }
            ok
        }
        Ext::Winget => entries
            .iter()
            .all(|entry| winget_install(exe, entry, verbose)),
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
        Ext::Flatpak => installed
            .packages
            .iter()
            .map(flatpak_dump_line)
            .collect::<Vec<_>>()
            .join("\n"),
        // `dump_entry`: the `id:` if not the name, the `source:` if not
        // `winget`.
        Ext::Winget => winget_packages(installed)
            .into_iter()
            .map(|app| {
                let id = app.id.as_deref().unwrap_or(&app.name);
                let mut line = format!("winget {:?}", app.name);
                if id != app.name {
                    line.push_str(&format!(", id: {id:?}"));
                }
                match app.source.as_deref() {
                    Some(source) if source != WINGET_SOURCES[0] => {
                        line.push_str(&format!(", source: {source:?}"))
                    }
                    _ => {}
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n"),
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
        // By id and source, `id<TAB>source`; shown as `winget export` names
        // them, by id (`cleanup_item_name`).
        Ext::Winget => {
            let mut apps: Vec<(&str, &str)> = installed
                .packages
                .iter()
                .filter_map(|app| Some((app.id.as_deref()?, app.source.as_deref()?)))
                .filter(|(id, _)| !winget_internal(id, id))
                .filter(|(id, source)| {
                    !kept.iter().any(|e| {
                        let (kept_id, kept_source) = winget_id(e);
                        kept_id.eq_ignore_ascii_case(id) && kept_source == *source
                    })
                })
                .collect();
            let rank = |source: &str| WINGET_SOURCES.iter().position(|s| *s == source);
            apps.sort_by_key(|(id, source)| (rank(source).unwrap_or(2), id.to_lowercase()));
            apps.into_iter()
                .map(|(id, source)| {
                    let shown = match source {
                        "winget" => id.to_string(),
                        _ => format!("{id} ({source})"),
                    };
                    (format!("{id}\t{source}"), shown)
                })
                .collect()
        }
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
        Ext::Flatpak | Ext::Winget => {
            // `Extension.cleanup!`: the Flatpaks' own output shown
            // (`Kernel.system`), WinGet's only when it fails.
            let removed = items
                .iter()
                .filter(|(item, shown)| {
                    let mut cmd = command(ext, exe);
                    let ok = if ext == Ext::Flatpak {
                        cmd.args(["uninstall", "-y", "--system", item]);
                        cmd.status().is_ok_and(|status| status.success())
                    } else {
                        let (id, source) = item.split_once('\t').unwrap_or((item, "winget"));
                        cmd.args(["uninstall", "--id", id, "--exact", "--source", source])
                            .args(["--accept-source-agreements", "--disable-interactivity"]);
                        run(cmd, false)
                    };
                    if !ok {
                        let banner = match ext {
                            Ext::Flatpak => "Flatpak packages",
                            _ => "WinGet packages",
                        };
                        eprintln!("Error: Failed to uninstall {banner}: {shown}");
                    }
                    ok
                })
                .count();
            // `Utils.pluralize(package_description, …)`.
            println!(
                "Uninstalled {removed} {}{}",
                ext.label().to_lowercase(),
                if removed == 1 { "" } else { "s" }
            );
            return;
        }
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
                    source: Some("https://github.com/x/foo?branch=main".into()),
                    ..Default::default()
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
                    ..Default::default()
                },
                Package {
                    name: "mytool".into(),
                    with: vec![],
                    source: Some("git+https://github.com/x/mytool".into()),
                    ..Default::default()
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

    #[test]
    fn reads_flatpak_lists_and_dumps_their_remotes() {
        let remotes = parse_flatpak_remotes(
            "flathub\thttps://dl.flathub.org/repo/\n\
             com.example.Foo-origin\thttps://example.com/foo\n\
             mine\thttps://example.com/mine.flatpakrepo\n\
             bare\n",
        );
        assert_eq!(remotes.len(), 3);
        let installed = Installed {
            packages: parse_flatpak_list(
                "org.gimp.GIMP\tflathub\n\
                 com.example.Foo\tcom.example.Foo-origin\n\
                 com.example.Bar\tmine\n\
                 com.example.Baz\tlocal\n\
                 org.blank\n\n",
                &remotes,
            ),
            ..Default::default()
        };
        assert_eq!(
            dump(Ext::Flatpak, &installed),
            "flatpak \"com.example.Bar\", remote: \"mine\", url: \"https://example.com/mine.flatpakrepo\"\n\
             flatpak \"com.example.Baz\", remote: \"local\"\n\
             flatpak \"com.example.Foo\", remote: \"https://example.com/foo\"\n\
             flatpak \"org.blank\"\n\
             flatpak \"org.gimp.GIMP\""
        );
        let entries = crate::brewfile::parse(
            "flatpak \"org.gimp.GIMP\"\n\
             flatpak \"org.gimp.GIMP\", remote: \"mine\"\n\
             flatpak \"com.example.Foo\", remote: \"https://example.com/foo\"\n\
             flatpak \"com.example.Bar\", remote: \"mine\", url: \"https://example.com/other\"\n\
             flatpak \"org.blank\", remote: \"https://example.com/blank.flatpakref\"\n\
             flatpak \"x.y\", remote: \"https://example.com/x.flatpakref\"\n",
        )
        .unwrap();
        let has: Vec<bool> = entries
            .iter()
            .map(|e| installed.has(Ext::Flatpak, e))
            .collect();
        assert_eq!(has, [true, false, true, true, true, false]);
        // Installing looks for the app from any remote.
        assert!(installed.present(Ext::Flatpak, &entries[1]));
        assert!(Ext::Flatpak.batched(&entries[1]));
        assert!(!Ext::Flatpak.batched(&entries[2]));
        assert_eq!(
            cleanup_items(Ext::Flatpak, &entries[..1], &installed)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [
                "com.example.Bar",
                "com.example.Baz",
                "com.example.Foo",
                "org.blank"
            ]
        );
    }

    #[test]
    fn reads_winget_lists_and_dumps_them() {
        let names = parse_winget_names(
            "Name      Id             Version   Source\r\n\
             -----------------------------------------\r\n\
             Git       Git.Git        2.45.1    winget\r\n\
             Café      Cafe.Tool      1.0       winget\r\n\
             \r\n",
        );
        assert_eq!(
            names,
            [
                ("git.git".to_string(), "Git".to_string()),
                ("cafe.tool".to_string(), "Café".to_string())
            ]
        );
        assert!(parse_winget_names("No installed package found.\n").is_empty());
        assert_eq!(
            parse_winget_export(
                r#"{"Sources": [{"Packages": [{"PackageIdentifier": "Git.Git"}, {"PackageIdentifier": ""}, {"x": 1}]}, {"Packages": [{"PackageIdentifier": "Microsoft.Edge"}]}]}"#
            ),
            ["Git.Git", "Microsoft.Edge"]
        );
        assert!(parse_winget_export("not json").is_empty());
        assert_eq!(
            wsl_path("C:\\Users\\me\\AppData"),
            Some(PathBuf::from("/mnt/c/Users/me/AppData"))
        );
        assert_eq!(wsl_path("relative\\path"), None);

        let app = |name: &str, id: &str, source: &str| Package {
            name: name.into(),
            id: Some(id.into()),
            source: Some(source.into()),
            ..Default::default()
        };
        let installed = Installed {
            packages: vec![
                app("Spotify", "9NCBCSZSJRSB", "msstore"),
                app("Git", "Git.Git", "winget"),
                app("Microsoft Edge", "Microsoft.Edge", "winget"),
                app("7zip.7zip", "7zip.7zip", "winget"),
            ],
            ..Default::default()
        };
        assert_eq!(
            dump(Ext::Winget, &installed),
            "winget \"7zip.7zip\"\n\
             winget \"Git\", id: \"Git.Git\"\n\
             winget \"Spotify\", id: \"9NCBCSZSJRSB\", source: \"msstore\""
        );
        let entries = crate::brewfile::parse(
            "winget \"Git\", id: \"git.git\"\nwinget \"Git.Git\", source: \"msstore\"\n",
        )
        .unwrap();
        assert!(installed.has(Ext::Winget, &entries[0]));
        assert!(!installed.has(Ext::Winget, &entries[1]));
        assert_eq!(
            cleanup_items(Ext::Winget, &entries[..1], &installed),
            [
                ("7zip.7zip\twinget".to_string(), "7zip.7zip".to_string()),
                (
                    "9NCBCSZSJRSB\tmsstore".to_string(),
                    "9NCBCSZSJRSB (msstore)".to_string()
                ),
            ]
        );
    }
}
