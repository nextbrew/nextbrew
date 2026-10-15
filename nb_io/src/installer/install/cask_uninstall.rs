//! A cask's `uninstall` and `zap` stanzas: directives run in Homebrew's
//! order. As in Homebrew, what needs root (kexts, pkg receipts, `sudo:
//! true` scripts, system launchd jobs and paths) goes through `sudo`, one
//! command at a time. No failure stops the uninstall. `trash` moves to
//! `~/.Trash`, so zapped user data can be restored.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use globset::Glob;
use serde_json::Value;

use super::cask_app::run;
use super::undeletable::undeletable;
use crate::installer::post_install::PostInstallContext;
use crate::installer::sudo;

const ORDER: &[&str] = &[
    "early_script",
    "launchctl",
    "quit",
    "signal",
    "login_item",
    "kext",
    "script",
    "pkgutil",
    "delete",
    "trash",
    "rmdir",
];

/// Run every `stanza` ("uninstall" or "zap") directive of the cask JSON.
/// Returns what was skipped or failed.
pub(super) fn run_stanza(ctx: &PostInstallContext, json: &Value, stanza: &str) -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(home) = home else {
        return vec!["HOME is not set; uninstall directives skipped".to_string()];
    };
    run_stanza_in(ctx, json, stanza, &home)
}

fn run_stanza_in(ctx: &PostInstallContext, json: &Value, stanza: &str, home: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let blocks: Vec<&Value> = json["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a.get(stanza)?.as_array())
        .flatten()
        .collect();
    for directive in ORDER {
        for value in blocks.iter().filter_map(|b| b.get(*directive)) {
            if *directive == "login_item" {
                for script in login_item_scripts(ctx, json, &items(value)) {
                    if let Err(e) = osascript(&script) {
                        warnings.push(format!("{stanza} login_item: {e}"));
                    }
                }
                continue;
            }
            for item in directive_items(directive, value) {
                if let Err(e) = run_directive(ctx, home, directive, item) {
                    warnings.push(format!("{stanza} {directive}: {e}"));
                }
            }
        }
    }
    warnings
}

/// The items of `directive`'s value. A `signal` is a `[signal, bundle id]`
/// pair or a list of them; every other directive takes one value or a
/// list.
fn directive_items<'a>(directive: &str, value: &'a Value) -> Vec<&'a Value> {
    if directive == "signal" && value.get(0).is_some_and(Value::is_string) {
        return vec![value];
    }
    items(value)
}

/// A directive takes one value or a list of them.
fn items(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(list) => list.iter().collect(),
        other => vec![other],
    }
}

fn run_directive(
    ctx: &PostInstallContext,
    home: &Path,
    directive: &str,
    item: &Value,
) -> Result<(), String> {
    let text = item.as_str().unwrap_or_default();
    match directive {
        "early_script" | "script" => script(ctx, home, item),
        "launchctl" => launchctl(home, text),
        "quit" => quit(text),
        "signal" => signal(item),
        "kext" => kext(text),
        "pkgutil" => pkgutil(home, text),
        "delete" | "trash" | "rmdir" => {
            for path in expand(ctx, home, text)? {
                remove(home, directive, &path).map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Ok(())
        }
        _ => Err(format!("unknown directive {item}")),
    }
}

fn script(ctx: &PostInstallContext, home: &Path, item: &Value) -> Result<(), String> {
    let (exe, args) = match item {
        Value::String(exe) => (exe.as_str(), Vec::new()),
        _ => (
            item["executable"].as_str().unwrap_or_default(),
            items(&item["args"])
                .into_iter()
                .filter_map(Value::as_str)
                .collect(),
        ),
    };
    let exe = expand_tokens(ctx, home, exe);
    let exe = match ctx.cask.as_ref() {
        Some(dirs) if Path::new(&exe).is_relative() => dirs.staged.join(&exe),
        _ => PathBuf::from(exe),
    };
    let mut cmd = if item["sudo"].as_bool() == Some(true) {
        sudo::command(&exe)
    } else {
        Command::new(&exe)
    };
    let result = run(cmd.args(args)).map_err(|e| e.to_string());
    if item["must_succeed"].as_bool() == Some(false) {
        return Ok(());
    }
    result
}

fn launchctl(home: &Path, label: &str) -> Result<(), String> {
    if run(Command::new("launchctl").args(["list", label])).is_ok() {
        run(Command::new("launchctl").args(["remove", label])).map_err(|e| e.to_string())?;
    }
    let _ = fs::remove_file(home.join(format!("Library/LaunchAgents/{label}.plist")));
    let system: Vec<PathBuf> = ["/Library/LaunchAgents", "/Library/LaunchDaemons"]
        .iter()
        .map(|dir| Path::new(dir).join(format!("{label}.plist")))
        .filter(|plist| plist.exists())
        .collect();
    if system.is_empty() {
        return Ok(());
    }
    // Not loaded is fine; the plist still has to go.
    let _ = run(sudo::command("/bin/launchctl").args(["remove", label]));
    run(sudo::command("/bin/rm").arg("-f").args(&system)).map_err(|e| e.to_string())
}

fn kext(id: &str) -> Result<(), String> {
    // Not loaded is fine; the bundle still has to go.
    let _ = run(sudo::command("/sbin/kextunload").args(["-b", id]));
    let found = Command::new("/usr/sbin/kextfind")
        .args(["-b", id])
        .output()
        .map_err(|e| e.to_string())?;
    let paths: Vec<&str> = std::str::from_utf8(&found.stdout)
        .unwrap_or_default()
        .lines()
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    run(sudo::command("/bin/rm").arg("-rf").arg("--").args(paths)).map_err(|e| e.to_string())
}

fn signal(item: &Value) -> Result<(), String> {
    let (Some(sig), Some(bundle)) = (item[0].as_str(), item[1].as_str()) else {
        return Err(format!("malformed signal {item}"));
    };
    let Ok(output) = Command::new("lsappinfo")
        .args(["info", "-only", "pid", "-app", bundle])
        .output()
    else {
        return Ok(());
    };
    // `"pid"=1234`, or nothing when the app is not running.
    let out = String::from_utf8_lossy(&output.stdout);
    let pid: String = out.chars().filter(char::is_ascii_digit).collect();
    if pid.is_empty() {
        return Ok(());
    }
    run(Command::new("kill").arg(format!("-{sig}")).arg(pid)).map_err(|e| e.to_string())
}

/// Remove what the receipts matching `pattern` installed, then forget
/// them, as Homebrew's `Pkg#uninstall` does.
fn pkgutil(home: &Path, pattern: &str) -> Result<(), String> {
    for id in pkgutil_lines(&[&format!("--pkgs={pattern}")])? {
        let mut root = PathBuf::from("/");
        for line in pkgutil_lines(&["--pkg-info", &id])? {
            if let Some(dir) = line.strip_prefix("volume: ") {
                root = PathBuf::from(dir);
            } else if let Some(dir) = line.strip_prefix("location: ") {
                root = root.join(dir);
            }
        }
        let files: Vec<PathBuf> = pkgutil_lines(&["--only-files", "--files", &id])?
            .iter()
            .map(|f| root.join(f))
            .filter(|f| f.symlink_metadata().is_ok())
            .collect();
        let mut dirs: Vec<PathBuf> = pkgutil_lines(&["--only-dirs", "--files", &id])?
            .iter()
            .map(|d| root.join(d))
            .filter(|d| d.is_dir() && !undeletable(d, home))
            .collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        if !files.is_empty() {
            xargs(&["/bin/rm", "-f", "--"], &files)?;
        }
        // Only the directories the package left empty go.
        let _ = xargs(&["/bin/rmdir", "--"], &dirs);
        run(sudo::command("/usr/sbin/pkgutil").args(["--forget", &id]))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn pkgutil_lines(args: &[&str]) -> Result<Vec<String>, String> {
    let output = Command::new("/usr/sbin/pkgutil")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect())
}

/// `sudo xargs -0 cmd`, so a package's thousands of files are one prompt
/// and no argument list overflows.
fn xargs(cmd: &[&str], paths: &[PathBuf]) -> Result<(), String> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_os_str().as_encoded_bytes());
        input.push(0);
    }
    let mut child = sudo::command("/usr/bin/xargs")
        .arg("-0")
        .args(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&input).map_err(|e| e.to_string())?;
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} failed with {status}", cmd[0]))
    }
}

/// Quit a running app as Homebrew does: through JavaScript for Automation,
/// where an app LaunchServices does not know is simply not running, then
/// up to 10 seconds for it to exit.
fn quit(bundle_id: &str) -> Result<(), String> {
    const RUNNING: &str = "ObjC.import('stdlib'); function run(argv) { \
        try { if (Application(argv[0]).running()) $.exit(0) } catch (err) {} $.exit(1) }";
    const QUIT: &str = "ObjC.import('stdlib'); function run(argv) { \
        var app = Application(argv[0]); \
        try { app.quit() } catch (err) { if (app.running()) $.exit(1) } $.exit(0) }";
    let jxa = |script: &str| {
        Command::new("osascript")
            .args(["-l", "JavaScript", "-e", script, bundle_id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };

    if !jxa(RUNNING) {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if jxa(QUIT) && !jxa(RUNNING) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(format!("application '{bundle_id}' did not quit"))
}

/// Homebrew's `uninstall_login_item`: the login items opening one of the
/// cask's apps go first, then those with the names given.
fn login_item_scripts(ctx: &PostInstallContext, json: &Value, names: &[&Value]) -> Vec<String> {
    let delete = |field: &str, value: &str| {
        format!(
            r#"tell application "System Events" to delete every login item whose {field} is "{}""#,
            applescript_str(value)
        )
    };
    let appdir = ctx.cask.as_ref().map(|dirs| dirs.appdir.clone());
    let apps = json["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|artifact| artifact.get("app").is_some())
        .filter_map(|artifact| {
            let target = artifact["target"]
                .as_str()
                .or_else(|| artifact["app"].get(0)?.as_str())?;
            let name = Path::new(target).file_name()?;
            Some(appdir.as_ref()?.join(name))
        });
    apps.map(|app| delete("path", &app.to_string_lossy()))
        .chain(
            names
                .iter()
                .filter_map(|n| n.as_str())
                .map(|n| delete("name", n)),
        )
        .collect()
}

fn osascript(script: &str) -> Result<(), String> {
    run(Command::new("osascript").args(["-e", script])).map_err(|e| e.to_string())
}

fn applescript_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn expand_tokens(ctx: &PostInstallContext, home: &Path, s: &str) -> String {
    let mut s = s.replace("$HOMEBREW_PREFIX", &ctx.prefix.to_string_lossy());
    if let Some(dirs) = &ctx.cask {
        s = s.replace("$APPDIR", &dirs.appdir.to_string_lossy());
    }
    match s.strip_prefix("~/") {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => s,
    }
}

/// The existing paths `pattern` names, globbed component by component as
/// Ruby's `Dir.glob` (Homebrew's `Pathname.glob`) does: `*`, `?`, `[...]`
/// and `{a,b}` within a component, never matching a leading `.` unless the
/// pattern starts with one, and `**` for any number of directories.
fn expand(ctx: &PostInstallContext, home: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let path = PathBuf::from(expand_tokens(ctx, home, pattern));
    if !path.is_absolute() || path.components().any(|c| c.as_os_str() == "..") {
        return Err(format!("{pattern}: not an absolute path; skipped"));
    }
    let mut found = vec![PathBuf::from("/")];
    for component in path.components().skip(1) {
        let part = component.as_os_str().to_string_lossy();
        found = if part == "**" {
            found.iter().flat_map(|dir| subdirectories(dir)).collect()
        } else if part.contains(['*', '?', '[', '{']) {
            let matcher = Glob::new(&part)
                .map_err(|e| format!("{pattern}: {e}"))?
                .compile_matcher();
            let dot = part.starts_with('.');
            found
                .iter()
                .filter_map(|dir| fs::read_dir(dir).ok())
                .flat_map(|entries| entries.flatten())
                .filter(|e| {
                    let name = e.file_name();
                    (dot || !name.to_string_lossy().starts_with('.')) && matcher.is_match(&name)
                })
                .map(|e| e.path())
                .collect()
        } else {
            found.iter().map(|dir| dir.join(component)).collect()
        };
    }
    found.retain(|p| p.symlink_metadata().is_ok());
    found.sort();
    found.dedup();
    Ok(found)
}

/// `dir` and every directory below it, not entering dot directories or
/// following links, as `**` does.
fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let mut all = vec![dir.to_path_buf()];
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && !entry.file_name().to_string_lossy().starts_with('.') {
                all.extend(subdirectories(&entry.path()));
            }
        }
    }
    all
}

/// Homebrew skips a system or home folder it is told to delete
/// (`MacOS.undeletable?`).
fn protected(home: &Path, path: &Path) -> bool {
    undeletable(path, home)
}

fn remove(home: &Path, directive: &str, path: &Path) -> Result<(), String> {
    if protected(home, path) {
        return Err("protected directory; skipped".to_string());
    }
    let is_dir = path.symlink_metadata().is_ok_and(|m| m.is_dir());
    let result = match directive {
        "rmdir" if is_dir => {
            let _ = fs::remove_file(path.join(".DS_Store"));
            // Not empty is fine: rmdir only takes empty directories.
            match fs::remove_dir(path) {
                Err(e) if e.kind() == ErrorKind::DirectoryNotEmpty => Ok(()),
                other => other,
            }
        }
        "rmdir" => Ok(()),
        "trash" => return trash(home, path),
        _ if is_dir => fs::remove_dir_all(path),
        _ => fs::remove_file(path),
    };
    match result {
        // System paths: as root, like Homebrew.
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let mut cmd = if directive == "rmdir" {
                sudo::command("/bin/rmdir")
            } else {
                let mut cmd = sudo::command("/bin/rm");
                cmd.arg("-rf");
                cmd
            };
            run(cmd.arg("--").arg(path)).map_err(|e| e.to_string())
        }
        other => other.map_err(|e| e.to_string()),
    }
}

fn trash(home: &Path, path: &Path) -> Result<(), String> {
    let bin = home.join(".Trash");
    fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut target = bin.join(&*name);
    let mut n = 1;
    while target.symlink_metadata().is_ok() {
        n += 1;
        target = bin.join(format!("{name} {n}"));
    }
    if fs::rename(path, &target).is_ok() {
        return Ok(());
    }
    // Another volume: copy, then delete.
    run(Command::new("cp").arg("-a").arg(path).arg(&target)).map_err(|e| e.to_string())?;
    remove(home, "delete", path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn quit_skips_an_app_that_is_not_running() {
        // AppleScript's `application id` fails to compile for an app
        // LaunchServices does not know; this must count as not running.
        assert_eq!(quit("dev.nextbrew.not-installed"), Ok(()));
    }

    fn ctx(root: &Path) -> PostInstallContext {
        PostInstallContext {
            prefix: root.join("prefix"),
            cellar: root.join("cellar"),
            name: "tool".into(),
            version: "1.0".into(),
            keg: root.join("keg"),
            temp: root.join("tmp"),
            cache: root.join("cache"),
            cask: None,
        }
    }

    fn names(paths: Vec<PathBuf>, home: &Path) -> Vec<String> {
        let mut names: Vec<String> = paths
            .iter()
            .map(|p| p.strip_prefix(home).unwrap().display().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn one_signal_pair_is_one_item() {
        // bing-wallpaper: "signal": ["TERM", "com.microsoft.bing.wallpaper"]
        let one = serde_json::json!(["TERM", "com.microsoft.bing.wallpaper"]);
        assert_eq!(directive_items("signal", &one), [&one]);
        let two = serde_json::json!([["TERM", "a"], ["KILL", "b"]]);
        assert_eq!(directive_items("signal", &two).len(), 2);
        let names = serde_json::json!(["a", "b"]);
        assert_eq!(directive_items("quit", &names).len(), 2);
    }

    #[test]
    fn login_items_go_by_app_path_first_then_by_name() {
        // aldente: login_item "AlDente", app AlDente.app.
        let tmp = TempDir::new().unwrap();
        let mut ctx = ctx(tmp.path());
        ctx.cask = Some(crate::installer::post_install::CaskDirs {
            staged: tmp.path().join("staged"),
            appdir: PathBuf::from("/Applications"),
            caskroom: tmp.path().join("caskroom"),
        });
        let json = serde_json::json!({ "artifacts": [
            { "app": ["AlDente.app"], "target": "/Applications/AlDente.app" }
        ]});
        let name = serde_json::json!("AlDente");

        assert_eq!(
            login_item_scripts(&ctx, &json, &[&name]),
            [
                r#"tell application "System Events" to delete every login item whose path is "/Applications/AlDente.app""#,
                r#"tell application "System Events" to delete every login item whose name is "AlDente""#,
            ]
        );
    }

    #[test]
    fn globs_in_parent_directories_match_like_ruby() {
        // adobe-creative-cloud: ~/Library/*/Adobe/CoreSync
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        for dir in [
            "Library/A/Adobe/CoreSync",
            "Library/B/Adobe/CoreSync",
            "Library/.h/Adobe/CoreSync",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        let found = expand(&ctx(home), home, "~/Library/*/Adobe/CoreSync").unwrap();
        assert_eq!(
            names(found, home),
            ["Library/A/Adobe/CoreSync", "Library/B/Adobe/CoreSync"],
            "a * does not match a dot directory"
        );
    }

    #[test]
    fn a_double_star_matches_any_depth_including_none() {
        // wine-stable: ~/.local/share/icons/hicolor/**/application-x-wine*
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let icons = home.join(".local/share/icons/hicolor");
        fs::create_dir_all(icons.join("48x48/apps")).unwrap();
        fs::write(icons.join("48x48/apps/application-x-wine-a.png"), "").unwrap();
        fs::write(icons.join("application-x-wine-b.png"), "").unwrap();
        fs::write(icons.join("48x48/other.png"), "").unwrap();

        let found = expand(
            &ctx(home),
            home,
            "~/.local/share/icons/hicolor/**/application-x-wine*",
        )
        .unwrap();

        assert_eq!(
            names(found, home),
            [
                ".local/share/icons/hicolor/48x48/apps/application-x-wine-a.png",
                ".local/share/icons/hicolor/application-x-wine-b.png",
            ]
        );
    }

    #[test]
    fn deletes_trashes_and_rmdirs_in_order() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let home = root.join("home");
        let support = home.join("Library/Application Support/Tool");
        fs::create_dir_all(&support).unwrap();
        fs::write(support.join("cache.db"), "x").unwrap();
        fs::write(home.join("Library/com.tool.a.plist"), "a").unwrap();
        fs::write(home.join("Library/com.tool.b.plist"), "b").unwrap();
        fs::write(home.join("Library/keep.plist"), "k").unwrap();
        fs::create_dir_all(root.join("prefix/var/tool")).unwrap();

        let json: Value = serde_json::from_str(
            r#"{"artifacts": [{"zap": [{
                "rmdir": "~/Library/Application Support/Tool",
                "trash": ["~/Library/Application Support/Tool/cache.db", "~/Library/com.tool.*.plist"],
                "delete": ["$HOMEBREW_PREFIX/var/tool", "~/Library", "relative/path"]
            }]}]}"#,
        )
        .unwrap();
        let warnings = run_stanza_in(&ctx(root), &json, "zap", &home);

        // delete runs before trash, trash before rmdir.
        assert!(!root.join("prefix/var/tool").exists());
        assert!(home.join(".Trash/cache.db").is_file());
        assert!(home.join(".Trash/com.tool.a.plist").is_file());
        assert!(home.join("Library/keep.plist").is_file());
        assert!(!support.exists());
        // ~/Library is protected, a relative path is refused.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(home.join("Library").is_dir());
    }

    #[test]
    fn failing_scripts_are_reported_unless_allowed_to_fail() {
        let tmp = TempDir::new().unwrap();
        let json: Value = serde_json::from_str(
            r#"{"artifacts": [{"uninstall": [{
                "script": [{"executable": "/bin/sh", "args": ["-c", "exit 0"]}],
                "early_script": {"executable": "/bin/false", "must_succeed": false}
            }, {
                "script": {"executable": "/bin/false"}
            }]}]}"#,
        )
        .unwrap();
        let warnings = run_stanza_in(&ctx(tmp.path()), &json, "uninstall", tmp.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("uninstall script"));
    }
}
