//! Casks that are more than a binary: desktop apps and casks with
//! `*flight` hooks. The download (DMG, zip or tarball) is unpacked into the
//! keg's staged directory with the system tools, which keep the symlinks
//! and extended attributes a signed `.app` bundle needs. Then, as in
//! Homebrew: preflight hooks, apps moved into the Applications folder,
//! binaries, postflight hooks. The keg keeps a symlink to every bundle it
//! placed and the cask's JSON, so uninstall knows what to undo.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nb_core::{Error, PostInstallStep};
use serde_json::Value;
use tracing::warn;

use super::Installer;
use super::bottle::{resolve_cask_source_path, stage_cask_binaries};
use super::cask_uninstall::run_stanza;
use crate::installer::cask::{ResolvedCask, resolve_cask};
use crate::installer::post_install::{
    CaskDirs, PostInstallContext, remove_uninstall_links, run_steps,
};
use crate::installer::sudo;

/// Keg directory with one symlink per placed bundle. Not a link dir, so
/// the linker leaves it alone.
const KEG_APPS_DIR: &str = "Applications";
/// Keg directory the download is unpacked into: the cask's `staged_path`.
const STAGED_DIR: &str = "staged";
/// The cask's API JSON, kept for its uninstall hooks.
const CASK_JSON: &str = "cask.json";
/// Volume clutter Homebrew leaves out when copying a DMG.
const DMG_SKIP: &[&str] = &[
    ".DocumentRevisions-V100",
    ".fseventsd",
    ".Spotlight-V100",
    ".TemporaryItems",
    ".Trashes",
    ".VolumeIcon.icns",
];

/// UDIF images end with a 512-byte trailer that starts with `koly`.
pub(super) fn is_dmg(path: &Path) -> Result<bool, Error> {
    let mut file = File::open(path).map_err(Error::store("failed to open cask download"))?;
    let len = file
        .metadata()
        .map_err(Error::store("failed to read cask download"))?
        .len();
    if len < 512 {
        return Ok(false);
    }
    let mut magic = [0u8; 4];
    file.seek(SeekFrom::End(-512))
        .and_then(|_| file.read_exact(&mut magic))
        .map_err(Error::store("failed to read cask download"))?;
    Ok(&magic == b"koly")
}

fn is_zip(path: &Path) -> Result<bool, Error> {
    let mut magic = [0u8; 4];
    let mut file = File::open(path).map_err(Error::store("failed to open cask download"))?;
    let n = file
        .read(&mut magic)
        .map_err(Error::store("failed to read cask download"))?;
    Ok(n == 4 && magic == *b"PK\x03\x04")
}

/// `/Applications` like Homebrew, or `~/Applications` when the user cannot
/// write there.
pub(super) fn app_dir() -> Result<PathBuf, Error> {
    if !cfg!(target_os = "macos") {
        return Err(Error::InvalidArgument {
            message: "app, DMG and hook casks are only supported on macOS".to_string(),
        });
    }

    let system = PathBuf::from("/Applications");
    let probe = system.join(".nextbrew-write-test");
    if File::create(&probe).is_ok() {
        let _ = fs::remove_file(&probe);
        return Ok(system);
    }

    let home = std::env::var_os("HOME").ok_or_else(|| Error::InvalidArgument {
        message: "/Applications is not writable and HOME is not set".to_string(),
    })?;
    let user = PathBuf::from(home).join("Applications");
    fs::create_dir_all(&user).map_err(Error::store("failed to create ~/Applications"))?;
    Ok(user)
}

impl Installer {
    /// What a cask's hooks run against. The keg doubles as `caskroom_path`.
    pub(super) fn cask_context(
        &self,
        cask: &ResolvedCask,
        keg: &Path,
        appdir: PathBuf,
    ) -> PostInstallContext {
        PostInstallContext {
            prefix: self.prefix.clone(),
            cellar: self.cellar.dir().to_path_buf(),
            name: cask.token.clone(),
            version: cask.version.clone(),
            keg: keg.to_path_buf(),
            temp: self.prefix.join("tmp"),
            cache: self.cache_dir.join("post-install"),
            cask: Some(CaskDirs {
                staged: keg.join(STAGED_DIR),
                appdir,
                caskroom: keg.to_path_buf(),
            }),
        }
    }

    /// Undo what [`stage_cask`] did for the keg, if it is a cask's.
    /// `zap` also runs the cask's `zap` stanza, removing its user data.
    pub(super) fn remove_cask(&self, keg: &Path, zap: bool) -> Result<(), Error> {
        let Ok(json) = fs::read(keg.join(CASK_JSON)) else {
            return remove_cask_apps(keg);
        };
        let json: Value =
            serde_json::from_slice(&json).map_err(Error::store("failed to read cask.json"))?;
        let token = json
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let cask = resolve_cask(token, &json)?;
        let appdir = match placed_appdir(keg) {
            Some(dir) => dir,
            None => app_dir()?,
        };
        let ctx = self.cask_context(&cask, keg, appdir);
        uninstall_cask(&ctx, &cask, &json)?;
        if zap {
            for warning in run_stanza(&ctx, &json, "zap") {
                warn!(cask = %cask.token, "{warning}");
            }
        }
        Ok(())
    }
}

/// Unpack `blob` into the staged directory and install the cask from it.
pub(super) fn stage_cask(
    blob: &Path,
    cask: &ResolvedCask,
    json: &Value,
    ctx: &PostInstallContext,
) -> Result<(), Error> {
    let dirs = ctx.cask.as_ref().expect("cask context");
    let _ = fs::remove_dir_all(&dirs.staged);
    fs::create_dir_all(&dirs.staged).map_err(Error::store("failed to create staged dir"))?;
    unpack(blob, &dirs.staged)?;
    fs::write(ctx.keg.join(CASK_JSON), json.to_string())
        .map_err(Error::store("failed to save cask.json"))?;

    run_hooks(ctx, cask, "preflight", &cask.hooks.preflight)?;
    for artifact in &cask.installers {
        run_installer(ctx, cask, artifact)?;
    }

    let keg_apps = ctx.keg.join(KEG_APPS_DIR);
    fs::create_dir_all(&keg_apps).map_err(Error::store("failed to create keg apps dir"))?;
    for app in &cask.apps {
        let source = resolve_cask_source_path(&dirs.staged, None, cask, &app.source)?;
        if !source.exists() {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' app '{}' not found in download",
                    cask.token, app.source
                ),
            });
        }

        let target = dirs.appdir.join(&app.target);
        let marker = keg_apps.join(&app.target);
        if target.symlink_metadata().is_ok() {
            // Reinstalling replaces this keg's own copy; anything else
            // (a manual install, Homebrew's) is left alone.
            if fs::read_link(&marker).ok().as_deref() != Some(target.as_path()) {
                return Err(Error::InvalidArgument {
                    message: format!(
                        "'{}' already exists; remove it before installing cask '{}'",
                        target.display(),
                        cask.token
                    ),
                });
            }
            fs::remove_dir_all(&target).map_err(Error::store("failed to replace existing app"))?;
        }

        // Same volume: a rename. Otherwise copy and drop the staged one.
        if fs::rename(&source, &target).is_err() {
            if let Err(e) = copy(&source, &target) {
                let _ = fs::remove_dir_all(&target);
                return Err(e);
            }
            let _ = fs::remove_dir_all(&source);
        }
        let _ = fs::remove_file(&marker);
        symlink(&target, &marker).map_err(Error::store("failed to record placed app"))?;
    }

    stage_cask_binaries(&dirs.staged, Some(&dirs.appdir), &ctx.keg, cask)?;
    run_hooks(ctx, cask, "postflight", &cask.hooks.postflight)
}

/// An `installer` or `pkg` artifact, as Homebrew runs it: `pkg` through
/// `sudo installer -target /`, `installer script:` as given, and
/// `installer manual:` left for the user.
fn run_installer(
    ctx: &PostInstallContext,
    cask: &ResolvedCask,
    artifact: &Value,
) -> Result<(), Error> {
    let staged = &ctx.cask.as_ref().expect("cask context").staged;
    let failed = |message: String| Error::ExecutionError {
        message: format!("cask '{}': {message}", cask.token),
    };
    if let Some(pkg) = artifact.get("pkg") {
        let path = pkg[0]
            .as_str()
            .ok_or_else(|| failed(format!("malformed pkg {pkg}")))?;
        let options = &pkg[1];
        let user = std::env::var("USER").unwrap_or_default();
        let mut cmd = sudo::command("/usr/bin/env");
        cmd.args([
            format!("LOGNAME={user}"),
            format!("USER={user}"),
            format!("USERNAME={user}"),
        ])
        .arg("/usr/sbin/installer")
        .arg("-pkg")
        .arg(staged.join(path))
        .args(["-target", "/"]);
        if options["allow_untrusted"].as_bool() == Some(true) {
            cmd.arg("-allowUntrusted");
        }
        let choices = match options.get("choices") {
            Some(choices) => {
                let file = ctx.keg.join("choices.xml");
                fs::write(&file, choices_plist(choices))
                    .map_err(Error::store("failed to write pkg choices"))?;
                cmd.arg("-applyChoiceChangesXML").arg(&file);
                Some(file)
            }
            None => None,
        };
        let result = run(&mut cmd);
        if let Some(file) = choices {
            let _ = fs::remove_file(file);
        }
        return result;
    }
    for entry in artifact["installer"].as_array().into_iter().flatten() {
        if let Some(manual) = entry["manual"].as_str() {
            warn!(
                "To complete the installation of cask {}, you must also run the installer at:\n  {}",
                cask.token,
                staged.join(manual).display()
            );
        } else if let Some(script) = entry.get("script") {
            let exe = script["executable"]
                .as_str()
                .ok_or_else(|| failed(format!("malformed installer script {script}")))?;
            let exe = staged.join(exe);
            let mut cmd = if script["sudo"].as_bool() == Some(true) {
                sudo::command(&exe)
            } else {
                Command::new(&exe)
            };
            let args = script["args"].as_array().into_iter().flatten();
            cmd.args(args.filter_map(Value::as_str));
            if let Err(e) = run(&mut cmd)
                && script["must_succeed"].as_bool() != Some(false)
            {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// The `-applyChoiceChangesXML` plist for a pkg's `choices`.
fn choices_plist(choices: &Value) -> String {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<array>\n",
    );
    for choice in choices.as_array().into_iter().flatten() {
        xml.push_str("<dict>\n");
        for (key, value) in choice.as_object().into_iter().flatten() {
            xml.push_str(&format!("<key>{}</key>", escape(key)));
            xml.push_str(&match value {
                Value::Bool(true) => "<true/>".to_string(),
                Value::Bool(false) => "<false/>".to_string(),
                Value::Number(n) if n.is_i64() => format!("<integer>{n}</integer>"),
                Value::Number(n) => format!("<real>{n}</real>"),
                other => format!(
                    "<string>{}</string>",
                    escape(other.as_str().unwrap_or_default())
                ),
            });
            xml.push('\n');
        }
        xml.push_str("</dict>\n");
    }
    xml.push_str("</array>\n</plist>\n");
    xml
}

/// Uninstall hooks around removing the apps. Hook failures are reported but
/// do not stop the uninstall, so a cask can always be removed.
fn uninstall_cask(
    ctx: &PostInstallContext,
    cask: &ResolvedCask,
    json: &Value,
) -> Result<(), Error> {
    let hooks = &cask.hooks;
    if let Err(e) = run_hooks(ctx, cask, "uninstall_preflight", &hooks.uninstall_preflight) {
        warn!(cask = %cask.token, error = %e, "uninstall hook failed");
    }
    for warning in run_stanza(ctx, json, "uninstall") {
        warn!(cask = %cask.token, "{warning}");
    }
    remove_cask_apps(&ctx.keg)?;
    for steps in [&hooks.preflight, &hooks.postflight] {
        if let Err(e) = remove_uninstall_links(ctx, steps) {
            warn!(cask = %cask.token, error = %e, "failed to remove a hook's link");
        }
    }
    if let Err(e) = run_hooks(
        ctx,
        cask,
        "uninstall_postflight",
        &hooks.uninstall_postflight,
    ) {
        warn!(cask = %cask.token, error = %e, "uninstall hook failed");
    }
    Ok(())
}

fn run_hooks(
    ctx: &PostInstallContext,
    cask: &ResolvedCask,
    hook: &str,
    steps: &[PostInstallStep],
) -> Result<(), Error> {
    if steps.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&ctx.temp).map_err(Error::file("failed to create tmp directory"))?;
    let notices = run_steps(ctx, steps).map_err(|failure| Error::PostInstallFailed {
        name: cask.install_name.clone(),
        step: format!("{hook} {}", failure.kind),
        message: failure.error.to_string(),
    })?;
    for notice in notices {
        warn!(cask = %cask.token, "{notice}");
    }
    Ok(())
}

/// Remove the bundles a keg placed in the Applications folder.
pub(super) fn remove_cask_apps(keg_path: &Path) -> Result<(), Error> {
    let Ok(entries) = fs::read_dir(keg_path.join(KEG_APPS_DIR)) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        // Every marker is a bundle or suite this keg placed.
        if target.symlink_metadata().is_ok() {
            fs::remove_dir_all(&target).map_err(|e| Error::StoreCorruption {
                message: format!("failed to remove '{}': {e}", target.display()),
            })?;
        }
    }
    Ok(())
}

/// The Applications folder the keg's apps went to.
fn placed_appdir(keg: &Path) -> Option<PathBuf> {
    let entry = fs::read_dir(keg.join(KEG_APPS_DIR))
        .ok()?
        .flatten()
        .next()?;
    Some(fs::read_link(entry.path()).ok()?.parent()?.to_path_buf())
}

/// Unpack `blob` into the existing directory `dest`. A DMG is mounted and
/// its contents copied, as Homebrew does, so hooks can change them.
fn unpack(blob: &Path, dest: &Path) -> Result<(), Error> {
    if !is_dmg(blob)? {
        return if is_zip(blob)? {
            run(Command::new("ditto")
                .arg("-x")
                .arg("-k")
                .arg(blob)
                .arg(dest))
        } else {
            run(Command::new("tar").arg("-xf").arg(blob).arg("-C").arg(dest))
        };
    }

    let mount = tempfile::tempdir().map_err(Error::store("failed to create mount point"))?;
    // ponytail: DMGs with a license agreement fail to attach here;
    // convert them first (`hdiutil convert -format UDTO`) if needed.
    run(Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-noautoopen",
            "-noverify",
            "-mountpoint",
        ])
        .arg(mount.path())
        .arg(blob))?;
    let copied = copy_volume(mount.path(), dest);
    let _ = Command::new("hdiutil")
        .args(["detach", "-force"])
        .arg(mount.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    copied
}

fn copy_volume(volume: &Path, dest: &Path) -> Result<(), Error> {
    let entries = fs::read_dir(volume).map_err(Error::store("failed to read mounted DMG"))?;
    for entry in entries {
        let entry = entry.map_err(Error::store("failed to read mounted DMG"))?;
        let name = entry.file_name();
        if !DMG_SKIP.iter().any(|skip| name == *skip) {
            copy(&entry.path(), &dest.join(&name))?;
        }
    }
    Ok(())
}

/// Copy a file or tree, keeping symlinks, modes and extended attributes.
fn copy(source: &Path, target: &Path) -> Result<(), Error> {
    let tool = if cfg!(target_os = "macos") {
        ["ditto", "--"]
    } else {
        ["cp", "-a"]
    };
    run(Command::new(tool[0]).arg(tool[1]).arg(source).arg(target))
}

pub(super) fn run(cmd: &mut Command) -> Result<(), Error> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::ExecutionError {
            message: format!("failed to run {program}: {e}"),
        })?;
    if !output.status.success() {
        return Err(Error::ExecutionError {
            message: format!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// `Foo.app` with a CLI, and hooks that touch every cask base.
    fn app_cask(root: &Path) -> (ResolvedCask, Value) {
        let json = serde_json::json!({
            "token": "foo",
            "version": "1.0",
            "url": "https://example.com/foo.tar.gz",
            "sha256": "aaa",
            "artifacts": [
                { "app": ["Foo.app"] },
                { "binary": ["$APPDIR/Foo.app/Contents/MacOS/foo", { "target": "foo" }] },
                { "preflight_steps": [{ "steps": [{
                    "type": "write",
                    "path": { "base": "staged_path", "path": "Foo.app/Contents/pre" },
                    "content": "{{token}} {{version}}"
                }] }] },
                { "postflight_steps": [{ "steps": [{
                    "type": "symlink",
                    "source": { "path": "{{appdir}}/Foo.app" },
                    "target": { "path": root.join("link").to_str().unwrap() },
                    "uninstall": true
                }] }] },
                { "uninstall_postflight_steps": [{ "steps": [{
                    "type": "touch",
                    "path": { "base": "caskroom_path", "path": "uninstalled" }
                }] }] }
            ]
        });
        (resolve_cask("foo", &json).unwrap(), json)
    }

    fn context(root: &Path) -> PostInstallContext {
        let keg = root.join("keg");
        let appdir = root.join("Applications");
        fs::create_dir_all(&keg).unwrap();
        fs::create_dir_all(&appdir).unwrap();
        PostInstallContext {
            prefix: root.join("prefix"),
            cellar: root.join("prefix/Cellar"),
            name: "foo".to_string(),
            version: "1.0".to_string(),
            temp: root.join("tmp"),
            cache: root.join("cache"),
            cask: Some(CaskDirs {
                staged: keg.join(STAGED_DIR),
                appdir,
                caskroom: keg.clone(),
            }),
            keg,
        }
    }

    /// A tarball of `Foo.app` with a framework-style symlink inside.
    fn app_tarball(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        let macos = src.join("Foo.app/Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::write(macos.join("foo"), "#!/bin/sh\n").unwrap();
        symlink("MacOS/foo", src.join("Foo.app/Contents/Current")).unwrap();

        let blob = dir.join("foo.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&blob)
            .arg("-C")
            .arg(&src)
            .arg("Foo.app")
            .status()
            .unwrap();
        assert!(status.success());
        blob
    }

    #[test]
    fn stages_app_runs_hooks_and_uninstalls() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let blob = app_tarball(root);
        let (cask, json) = app_cask(root);
        let ctx = context(root);

        stage_cask(&blob, &cask, &json, &ctx).unwrap();

        let app = root.join("Applications/Foo.app");
        assert!(app.join("Contents/MacOS/foo").is_file());
        assert!(
            app.join("Contents/Current")
                .symlink_metadata()
                .unwrap()
                .is_symlink()
        );
        // The preflight ran on the staged copy, before the move.
        assert_eq!(
            fs::read_to_string(app.join("Contents/pre")).unwrap(),
            "foo 1.0"
        );
        assert_eq!(fs::read_link(root.join("link")).unwrap(), app);
        assert_eq!(
            fs::read_link(ctx.keg.join("Applications/Foo.app")).unwrap(),
            app
        );
        assert_eq!(
            fs::read_link(ctx.keg.join("bin/foo")).unwrap(),
            app.join("Contents/MacOS/foo")
        );

        // Uninstall reads the hooks back from the keg.
        let saved: Value =
            serde_json::from_slice(&fs::read(ctx.keg.join(CASK_JSON)).unwrap()).unwrap();
        uninstall_cask(&ctx, &resolve_cask("foo", &saved).unwrap(), &saved).unwrap();
        assert!(!app.exists());
        assert!(root.join("link").symlink_metadata().is_err());
        assert!(ctx.keg.join("uninstalled").exists());
    }

    #[test]
    fn a_failing_hook_fails_the_install() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let blob = app_tarball(root);
        let (mut cask, json) = app_cask(root);
        cask.hooks.preflight = serde_json::from_value(serde_json::json!([
            { "type": "something_new" }
        ]))
        .unwrap();

        let err = stage_cask(&blob, &cask, &json, &context(root))
            .unwrap_err()
            .to_string();
        assert!(err.contains("preflight something_new"), "got: {err}");
        assert!(!root.join("Applications/Foo.app").exists());
    }

    #[test]
    fn refuses_to_replace_an_app_it_did_not_place() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let blob = app_tarball(root);
        let (cask, json) = app_cask(root);
        let ctx = context(root);
        fs::create_dir_all(root.join("Applications/Foo.app")).unwrap();

        let err = stage_cask(&blob, &cask, &json, &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "got: {err}");
        assert!(root.join("Applications/Foo.app").exists());
    }

    #[test]
    fn writes_pkg_choices_as_a_plist() {
        let choices = serde_json::json!([
            {"choiceIdentifier": "a&b", "choiceAttribute": "selected", "attributeSetting": 0}
        ]);
        let xml = choices_plist(&choices);
        assert!(xml.contains("<key>choiceIdentifier</key><string>a&amp;b</string>"));
        assert!(xml.contains("<key>attributeSetting</key><integer>0</integer>"));
    }

    #[test]
    fn runs_installer_scripts_from_the_staged_dir() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path());
        let staged = &ctx.cask.as_ref().unwrap().staged;
        fs::create_dir_all(staged).unwrap();
        let script = staged.join("setup.sh");
        fs::write(&script, "#!/bin/sh\ntouch \"$1\"\n").unwrap();
        fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let (cask, _) = app_cask(tmp.path());
        let marker = tmp.path().join("ran");
        let artifact = serde_json::json!({"installer": [
            {"manual": "Setup.app"},
            {"script": {"executable": "setup.sh", "args": [marker]}}
        ]});
        run_installer(&ctx, &cask, &artifact).unwrap();
        assert!(marker.is_file());
    }

    #[test]
    fn removes_placed_suites() {
        let tmp = TempDir::new().unwrap();
        let keg = tmp.path().join("keg");
        let suite = tmp.path().join("Applications/AMPPS");
        fs::create_dir_all(suite.join("Ampps.app")).unwrap();
        fs::create_dir_all(keg.join(KEG_APPS_DIR)).unwrap();
        symlink(&suite, keg.join(KEG_APPS_DIR).join("AMPPS")).unwrap();
        remove_cask_apps(&keg).unwrap();
        assert!(!suite.exists());
    }

    #[test]
    fn detects_dmg_trailer() {
        let tmp = TempDir::new().unwrap();
        let dmg = tmp.path().join("a.dmg");
        let mut bytes = vec![0u8; 2048];
        bytes[2048 - 512..2048 - 508].copy_from_slice(b"koly");
        fs::write(&dmg, &bytes).unwrap();
        assert!(is_dmg(&dmg).unwrap());

        let other = tmp.path().join("b");
        fs::write(&other, vec![0u8; 2048]).unwrap();
        assert!(!is_dmg(&other).unwrap());
    }
}
