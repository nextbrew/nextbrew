//! `brew bundle`'s extensions for other package managers: `mas` (the Mac
//! App Store) and `vscode` (VS Code and its forks). As in Homebrew, nextbrew
//! drives their own command-line tools and installs those tools first when
//! they are missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::brewfile::{Entry, Kind, Value};

/// The extensions nextbrew handles, in Homebrew's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext {
    Mas,
    Vscode,
}

pub const HANDLED: &[Ext] = &[Ext::Mas, Ext::Vscode];

impl Ext {
    pub fn of(entry: &Entry) -> Option<Ext> {
        match entry.kind {
            Kind::Extension("mas") => Some(Ext::Mas),
            Kind::Extension("vscode") => Some(Ext::Vscode),
            _ => None,
        }
    }

    /// `check_label`.
    pub fn label(self) -> &'static str {
        match self {
            Ext::Mas => "App",
            Ext::Vscode => "VSCode Extension",
        }
    }

    /// `cleanup_heading`.
    pub fn cleanup_heading(self) -> &'static str {
        match self {
            Ext::Mas => "Mac App Store apps",
            Ext::Vscode => "VSCode extensions",
        }
    }

    /// The tool's executable on `PATH` or in the prefix.
    pub fn executable(self, prefix: &Path) -> Option<PathBuf> {
        let names: &[&str] = match self {
            Ext::Mas => &["mas"],
            Ext::Vscode => &["code", "codium", "cursor", "code-insiders"],
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
        }
    }
}

/// What a tool says is installed: Mac App Store `(id, name)`s or extension
/// ids, and the outdated app ids.
#[derive(Debug, Default)]
pub struct Installed {
    pub apps: Vec<(String, String)>,
    pub outdated: Vec<String>,
    pub extensions: Vec<String>,
}

fn read(executable: &Path, args: &[&str]) -> String {
    Command::new(executable)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

impl Installed {
    pub fn read(ext: Ext, prefix: &Path) -> Self {
        let Some(exe) = ext.executable(prefix) else {
            return Self::default();
        };
        match ext {
            Ext::Mas => Self {
                apps: parse_mas_list(&read(&exe, &["list"])),
                outdated: read(&exe, &["outdated"])
                    .lines()
                    .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                    .collect(),
                ..Self::default()
            },
            Ext::Vscode => Self {
                extensions: read(&exe, &["--list-extensions"])
                    .lines()
                    .map(str::trim)
                    .filter(|id| is_extension_id(id))
                    .map(str::to_lowercase)
                    .collect(),
                ..Self::default()
            },
        }
    }

    pub fn has(&self, ext: Ext, entry: &Entry) -> bool {
        match ext {
            Ext::Mas => {
                app_id(entry).is_some_and(|id| self.apps.iter().any(|(app, _)| same_id(app, id)))
            }
            Ext::Vscode => self.extensions.contains(&entry.name.to_lowercase()),
        }
    }

    pub fn outdated(&self, ext: Ext, entry: &Entry) -> bool {
        ext == Ext::Mas
            && app_id(entry).is_some_and(|id| self.outdated.iter().any(|app| same_id(app, id)))
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

/// Install `entries` (all of one extension) at once, as `install_batch!`
/// does: Mac App Store apps already there are upgraded, the rest installed
/// (`mas get` if `mas install` fails); VS Code extensions in one run.
pub fn install_batch(ext: Ext, exe: &Path, entries: &[&Entry], installed: &Installed) -> bool {
    let run = |args: Vec<String>| {
        Command::new(exe)
            .args(&args)
            .status()
            .is_ok_and(|status| status.success())
    };
    match ext {
        Ext::Mas => {
            let (outdated, fresh): (Vec<&&Entry>, Vec<&&Entry>) =
                entries.iter().partition(|entry| installed.has(ext, entry));
            let ids = |list: &[&&Entry]| -> Vec<String> {
                list.iter()
                    .filter_map(|e| app_id(e))
                    .map(|id| id.to_string())
                    .collect()
            };
            let upgraded =
                outdated.is_empty() || run([vec!["upgrade".to_string()], ids(&outdated)].concat());
            let fresh_ok = fresh.is_empty()
                || run([vec!["install".to_string()], ids(&fresh)].concat())
                || run([vec!["get".to_string()], ids(&fresh)].concat());
            upgraded && fresh_ok
        }
        Ext::Vscode => run(entries
            .iter()
            .flat_map(|entry| ["--install-extension".to_string(), entry.name.clone()])
            .collect()),
    }
}

/// Install one entry on its own, after a batch failed.
pub fn install_one(ext: Ext, exe: &Path, entry: &Entry, installed: &Installed) -> bool {
    install_batch(ext, exe, &[entry], installed)
}

/// `dump`: `mas "Name", id: 123` by name, `vscode "publisher.name"`.
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
        Ext::Vscode => installed
            .extensions
            .iter()
            .map(|id| format!("vscode {id:?}"))
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
            .extensions
            .iter()
            .filter(|id| !kept.iter().any(|e| e.name.to_lowercase() == **id))
            .map(|id| (id.clone(), id.clone()))
            .collect(),
    }
}

/// `cleanup!`.
pub fn cleanup(ext: Ext, exe: &Path, items: &[(String, String)]) {
    for (id, _) in items {
        let args: &[&str] = match ext {
            Ext::Mas => &["uninstall", id],
            Ext::Vscode => &["--uninstall-extension", id],
        };
        let _ = Command::new(exe).args(args).status();
    }
    if ext == Ext::Mas {
        let n = items.len();
        println!(
            "Uninstalled {n} Mac App Store app{}",
            if n == 1 { "" } else { "s" }
        );
    }
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
            extensions: vec!["golang.go".into(), "ms-python.python".into()],
            ..Default::default()
        };
        assert_eq!(
            dump(Ext::Mas, &installed),
            "mas \"Alpha\", id: 1\nmas \"beta\", id: 2"
        );
        assert_eq!(
            dump(Ext::Vscode, &installed),
            "vscode \"golang.go\"\nvscode \"ms-python.python\""
        );
        let entries =
            crate::brewfile::parse("mas \"Alpha\", id: 1\nvscode \"GoLang.Go\"\n").unwrap();
        assert_eq!(
            cleanup_items(Ext::Mas, &entries, &installed),
            [("2".to_string(), "beta (2)".to_string())]
        );
        assert_eq!(
            cleanup_items(Ext::Vscode, &entries, &installed),
            [(
                "ms-python.python".to_string(),
                "ms-python.python".to_string()
            )]
        );
        // Nothing is cleaned for a kind the Brewfile does not list.
        assert!(cleanup_items(Ext::Mas, &[], &installed).is_empty());
    }
}
