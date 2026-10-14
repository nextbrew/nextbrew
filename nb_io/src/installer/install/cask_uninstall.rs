//! A cask's `uninstall` and `zap` stanzas: directives run in Homebrew's
//! order, as the current user. What needs root (kexts, pkg receipts,
//! `sudo: true` scripts, system paths) is reported and left alone, and no
//! failure stops the uninstall. `trash` moves to `~/.Trash`, so zapped
//! user data can be restored.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use globset::Glob;
use serde_json::Value;

use super::cask_app::run;
use crate::installer::post_install::PostInstallContext;

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
            for item in items(value) {
                if let Err(e) = run_directive(ctx, home, directive, item) {
                    warnings.push(format!("{stanza} {directive}: {e}"));
                }
            }
        }
    }
    warnings
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
        "quit" => osascript(&format!(
            r#"if application id "{}" is running then tell application id "{0}" to quit"#,
            applescript_str(text)
        )),
        "signal" => signal(item),
        "login_item" => osascript(&format!(
            r#"tell application "System Events" to delete every login item whose name is "{}""#,
            applescript_str(text)
        )),
        "kext" => Err(format!("{text}: unloading kexts needs root; skipped")),
        "pkgutil" => pkgutil(text),
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
    if item["sudo"].as_bool() == Some(true) {
        return Err(format!("{exe}: needs root; skipped"));
    }
    let exe = expand_tokens(ctx, home, exe);
    let exe = match ctx.cask.as_ref() {
        Some(dirs) if Path::new(&exe).is_relative() => dirs.staged.join(&exe),
        _ => PathBuf::from(exe),
    };
    let result = run(Command::new(&exe).args(args)).map_err(|e| e.to_string());
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
    for dir in ["/Library/LaunchAgents", "/Library/LaunchDaemons"] {
        if Path::new(dir).join(format!("{label}.plist")).exists() {
            return Err(format!("{label}: system launchd job needs root; skipped"));
        }
    }
    Ok(())
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

fn pkgutil(id: &str) -> Result<(), String> {
    let output = Command::new("pkgutil")
        .arg(format!("--pkgs={id}"))
        .output()
        .map_err(|e| e.to_string())?;
    if output.stdout.is_empty() {
        return Ok(());
    }
    Err(format!("{id}: removing pkg receipts needs root; skipped"))
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

/// The existing paths `pattern` names. Only the last component may be a
/// glob.
// ponytail: last-component globs only (22 of ~23k cask paths glob a parent), walk with globset if they matter.
fn expand(ctx: &PostInstallContext, home: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let path = PathBuf::from(expand_tokens(ctx, home, pattern));
    if !path.is_absolute() || path.components().any(|c| c.as_os_str() == "..") {
        return Err(format!("{pattern}: not an absolute path; skipped"));
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if !name.contains(['*', '?', '[', '{']) {
        return Ok(if path.symlink_metadata().is_ok() {
            vec![path]
        } else {
            Vec::new()
        });
    }
    let parent = path.parent().unwrap_or(Path::new("/"));
    if parent.to_string_lossy().contains(['*', '?', '[', '{']) {
        return Err(format!(
            "{pattern}: globbed parent directories are not supported; skipped"
        ));
    }
    let matcher = Glob::new(&name)
        .map_err(|e| e.to_string())?
        .compile_matcher();
    let Ok(entries) = fs::read_dir(parent) else {
        return Ok(Vec::new());
    };
    Ok(entries
        .flatten()
        .filter(|e| matcher.is_match(e.file_name()))
        .map(|e| e.path())
        .collect())
}

/// Directories a cask may name but must never take away.
fn protected(home: &Path, path: &Path) -> bool {
    const SYSTEM: &[&str] = &[
        "/",
        "/Applications",
        "/Library",
        "/System",
        "/Users",
        "/opt",
        "/private",
        "/usr",
        "/var",
    ];
    const HOME: &[&str] = &[
        "",
        "Applications",
        "Desktop",
        "Documents",
        "Downloads",
        "Library",
        "Library/Application Support",
        "Library/Caches",
        "Library/Containers",
        "Library/Group Containers",
        "Library/LaunchAgents",
        "Library/Logs",
        "Library/Preferences",
        "Movies",
        "Music",
        "Pictures",
    ];
    if SYSTEM.iter().any(|p| path == Path::new(p)) {
        return true;
    }
    HOME.iter().any(|p| path == home.join(p))
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
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
                other => other,
            }
        }
        "rmdir" => Ok(()),
        "trash" => return trash(home, path),
        _ if is_dir => fs::remove_dir_all(path),
        _ => fs::remove_file(path),
    };
    result.map_err(|e| e.to_string())
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
    fn sudo_scripts_are_skipped_and_failing_scripts_reported() {
        let tmp = TempDir::new().unwrap();
        let json: Value = serde_json::from_str(
            r#"{"artifacts": [{"uninstall": [{
                "script": [{"executable": "/bin/true", "sudo": true}],
                "early_script": {"executable": "/bin/false", "must_succeed": false}
            }, {
                "script": {"executable": "/bin/false"}
            }]}]}"#,
        )
        .unwrap();
        let warnings = run_stanza_in(&ctx(tmp.path()), &json, "uninstall", tmp.path());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("needs root"));
    }
}
