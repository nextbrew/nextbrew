//! The cask artifacts besides apps, binaries and installers, placed as
//! Homebrew places them: `font`, `qlplugin`, `artifact` and the like moved
//! to their target, `manpage` and shell completions linked, command
//! wrappers and generated scripts written, completions generated.
//!
//! A target in the prefix's `bin`, `share`, `etc`, … is made in the keg,
//! and linking the keg puts it in the prefix, as with a formula. Anywhere
//! else (`~/Library/Fonts`, `/Library/Keyboard Layouts`) the artifact goes
//! straight there, through sudo when the folder is not ours to write, and a
//! marker in the keg says what to take away on uninstall.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use nb_core::Error;
use tracing::warn;

use super::bottle::resolve_cask_source_path;
use super::cask_app::run;
use crate::installer::cask::{CommandWrapper, GeneratedCompletion, PlacedArtifact, ResolvedCask};
use crate::installer::sudo;

/// Markers for artifacts placed outside the keg: `<n>` links to the target,
/// `<n>`[`LINK_MARKER`] to a symlink made there.
const PLACED_DIR: &str = "Placed";
const LINK_MARKER: &str = "-link";
/// Where command wrapper scripts are written, as in Homebrew.
const WRAPPERS_DIR: &str = ".homebrew-command-wrappers";
/// The prefix directories a keg is linked into.
const LINKED_DIRS: &[&str] = &["bin", "sbin", "lib", "include", "share", "etc"];

/// Where a cask's artifacts go.
pub(super) struct Places<'a> {
    pub(super) prefix: &'a Path,
    pub(super) keg: &'a Path,
    pub(super) staged: &'a Path,
    pub(super) appdir: &'a Path,
    pub(super) home: &'a Path,
}

impl Places<'_> {
    /// Replace the placeholders Homebrew's API leaves in cask data
    /// (`deep_remove_placeholders`): `/$HOME`, `$HOMEBREW_PREFIX`,
    /// `$HOMEBREW_CELLAR`, `$APPDIR`. Homebrew's `Caskroom/<token>/<version>`
    /// is the staged directory here.
    pub(super) fn expand(&self, cask: &ResolvedCask, text: &str) -> String {
        let caskroom = format!("$HOMEBREW_PREFIX/Caskroom/{}/{}", cask.token, cask.version);
        let prefix = self.prefix.to_string_lossy();
        text.replace(&caskroom, &self.staged.to_string_lossy())
            .replace("/$HOME", &self.home.to_string_lossy())
            .replace("$HOMEBREW_PREFIX", &prefix)
            .replace("$HOMEBREW_CELLAR", &format!("{prefix}/Cellar"))
            .replace("$APPDIR", &self.appdir.to_string_lossy())
    }

    /// Where `target` is made: in the keg when it is under one of the
    /// prefix's linked directories, otherwise `target` itself.
    fn destination(&self, target: &Path) -> (PathBuf, bool) {
        if let Ok(rel) = target.strip_prefix(self.prefix)
            && let Some(Component::Normal(top)) = rel.components().next()
            && LINKED_DIRS.iter().any(|dir| top == *dir)
        {
            return (self.keg.join(rel), true);
        }
        (target.to_path_buf(), false)
    }
}

/// Write the `generated_script`s into the staged directory; they run as
/// installers, so they come before them.
pub(super) fn write_generated_scripts(places: &Places, cask: &ResolvedCask) -> Result<(), Error> {
    for (path, content) in &cask.generated_scripts {
        let path = places.staged.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(Error::store("failed to create script directory"))?;
        }
        fs::write(&path, places.expand(cask, content))
            .map_err(Error::store("failed to write generated script"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(Error::store("failed to make generated script executable"))?;
    }
    Ok(())
}

/// Move the moved artifacts into place, after the apps they may come from.
pub(super) fn place_moved(places: &Places, cask: &ResolvedCask) -> Result<(), Error> {
    for artifact in cask.placed.iter().filter(|a| !a.linked) {
        let source = source_path(places, cask, artifact)?;
        let target = PathBuf::from(places.expand(cask, &artifact.target));
        let (destination, in_keg) = places.destination(&target);
        let marker = (!in_keg).then(|| next_marker(places.keg)).transpose()?;
        move_into_place(&source, &destination, marker.as_deref(), cask)?;
        // As Homebrew's post_move, so a binary under a moved suite is found.
        symlink(&destination, &source).map_err(Error::store("failed to link moved artifact"))?;
        if artifact.kind == "app" {
            super::cask_app::fix_app_permissions(&destination, places.home)?;
        }
        if let Some(marker) = marker {
            symlink(&destination, &marker)
                .map_err(Error::store("failed to record a placed artifact"))?;
        }
    }
    Ok(())
}

/// Write the command wrappers and link them into `bin`, beside binaries.
pub(super) fn place_wrappers(places: &Places, cask: &ResolvedCask) -> Result<(), Error> {
    for wrapper in &cask.wrappers {
        let script = places.staged.join(WRAPPERS_DIR).join(&wrapper.name);
        fs::create_dir_all(script.parent().expect("wrapper dir"))
            .map_err(Error::store("failed to create wrapper directory"))?;
        let _ = fs::remove_file(&script);
        fs::write(&script, wrapper_script(places, cask, wrapper))
            .map_err(Error::store("failed to write command wrapper"))?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o555))
            .map_err(Error::store("failed to make command wrapper executable"))?;
        let link = places.keg.join("bin").join(&wrapper.name);
        fs::create_dir_all(link.parent().expect("bin dir"))
            .map_err(Error::store("failed to create cask bin dir"))?;
        let _ = fs::remove_file(&link);
        symlink(&script, &link).map_err(Error::store("failed to link command wrapper"))?;
    }
    Ok(())
}

/// Link the man pages, shell completions and binaries with a path in the
/// prefix or the home folder as their target, then generate the rest.
pub(super) fn place_linked(places: &Places, cask: &ResolvedCask) -> Result<(), Error> {
    for artifact in cask.placed.iter().filter(|a| a.linked) {
        let source = source_path(places, cask, artifact)?;
        let target = PathBuf::from(places.expand(cask, &artifact.target));
        let (destination, in_keg) = places.destination(&target);
        let in_home = !in_keg && target.starts_with(places.home);
        if !in_keg && !in_home {
            return Err(Error::InvalidArgument {
                message: format!(
                    "cask '{}' links its {} to '{}', outside the prefix and the home folder",
                    cask.token,
                    artifact.kind,
                    target.display()
                ),
            });
        }
        if artifact.kind == "binary"
            && let Ok(meta) = fs::metadata(&source)
            && meta.permissions().mode() & 0o111 == 0
        {
            // `Binary#link`: make it executable.
            let mut permissions = meta.permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            fs::set_permissions(&source, permissions)
                .map_err(Error::store("failed to make a binary executable"))?;
        }
        if in_home {
            link_into_place(places.keg, &source, &destination, cask)?;
            continue;
        }
        fs::create_dir_all(destination.parent().expect("link dir"))
            .map_err(Error::store("failed to create link directory"))?;
        let _ = fs::remove_file(&destination);
        symlink(&source, &destination).map_err(Error::store("failed to link a cask artifact"))?;
    }
    for completion in &cask.completions {
        generate_completions(places, cask, completion);
    }
    Ok(())
}

/// Take away what was placed outside the keg; the keg's own goes with it.
pub(super) fn remove_placed(keg: &Path) -> Result<(), Error> {
    let Ok(entries) = fs::read_dir(keg.join(PLACED_DIR)) else {
        return Ok(());
    };
    let mut markers: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    // Last placed first, as Homebrew uninstalls in reverse.
    markers.sort_by_key(|m| std::cmp::Reverse(marker_index(m)));
    for marker in markers {
        let Ok(target) = fs::read_link(&marker) else {
            continue;
        };
        // `Symlinked#unlink`: a link only while it is still one, so a file
        // put there since is left alone.
        let linked = marker.to_string_lossy().ends_with(LINK_MARKER);
        match target.symlink_metadata() {
            Ok(meta) if linked && !meta.file_type().is_symlink() => {}
            Ok(_) => remove_path(&target)?,
            Err(_) => {}
        }
        let _ = fs::remove_file(&marker);
    }
    Ok(())
}

fn source_path(
    places: &Places,
    cask: &ResolvedCask,
    artifact: &PlacedArtifact,
) -> Result<PathBuf, Error> {
    let source = resolve_cask_source_path(
        places.staged,
        Some(places.appdir),
        Some(places.keg),
        cask,
        &artifact.source,
    )?;
    if source.symlink_metadata().is_err() {
        return Err(Error::InvalidArgument {
            message: format!(
                "cask '{}' {} '{}' not found in download",
                cask.token, artifact.kind, artifact.source
            ),
        });
    }
    Ok(source)
}

fn next_marker(keg: &Path) -> Result<PathBuf, Error> {
    let dir = keg.join(PLACED_DIR);
    fs::create_dir_all(&dir).map_err(Error::store("failed to create placed dir"))?;
    let next = fs::read_dir(&dir)
        .map_err(Error::store("failed to read placed dir"))?
        .flatten()
        .filter_map(|e| marker_index(&e.path()))
        .max()
        .map_or(0, |n| n + 1);
    Ok(dir.join(next.to_string()))
}

fn marker_index(marker: &Path) -> Option<usize> {
    let name = marker.file_name()?.to_str()?;
    name.strip_suffix(LINK_MARKER).unwrap_or(name).parse().ok()
}

/// Whether `keg` has a marker for `target`: it placed what is there.
fn placed_by(keg: &Path, target: &Path) -> bool {
    fs::read_dir(keg.join(PLACED_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| fs::read_link(e.path()).ok().as_deref() == Some(target))
}

fn already_exists(target: &Path, cask: &ResolvedCask) -> Error {
    Error::InvalidArgument {
        message: format!(
            "'{}' already exists; remove it before installing cask '{}'",
            target.display(),
            cask.token
        ),
    }
}

/// `Symlinked#link` outside the keg: link `target` to `source`, with a
/// marker in `keg` to take it away on uninstall. A link already there is
/// replaced when it leads nowhere, to `source`, or was placed by this keg;
/// anything else is left for the user, as Homebrew does.
fn link_into_place(
    keg: &Path,
    source: &Path,
    target: &Path,
    cask: &ResolvedCask,
) -> Result<(), Error> {
    let placed = placed_by(keg, target);
    if let Ok(meta) = target.symlink_metadata() {
        let ours = meta.file_type().is_symlink()
            && (placed
                || target.metadata().is_err()
                || fs::canonicalize(target).ok() == fs::canonicalize(source).ok());
        if !ours {
            return Err(already_exists(target, cask));
        }
        remove_path(target)?;
    }
    fs::create_dir_all(target.parent().expect("absolute target"))
        .map_err(Error::store("failed to create link directory"))?;
    symlink(source, target).map_err(Error::store("failed to link a cask artifact"))?;
    if !placed {
        let marker = next_marker(keg)?;
        let marker = marker.with_file_name(format!(
            "{}{LINK_MARKER}",
            marker.file_name().expect("marker").to_string_lossy()
        ));
        symlink(target, marker).map_err(Error::store("failed to record a placed artifact"))?;
    }
    Ok(())
}

/// Move `source` to `target`. A `target` already there is replaced only
/// when `marker` would point at it (this keg placed it); anything else is
/// left for the user, as Homebrew does.
fn move_into_place(
    source: &Path,
    target: &Path,
    marker: Option<&Path>,
    cask: &ResolvedCask,
) -> Result<(), Error> {
    if target.symlink_metadata().is_ok() {
        let ours = marker.is_none()
            || marker
                .and_then(Path::parent)
                .and_then(Path::parent)
                .is_some_and(|keg| placed_by(keg, target));
        if !ours {
            return Err(already_exists(target, cask));
        }
        remove_path(target)?;
    }
    let parent = target.parent().expect("absolute target");
    if !parent.exists() && fs::create_dir_all(parent).is_err() {
        run(sudo::command("/bin/mkdir").arg("-p").arg(parent))?;
    }
    if writable(parent) {
        if fs::rename(source, target).is_err() {
            run(Command::new("/usr/bin/ditto").arg(source).arg(target))?;
            let _ = fs::remove_dir_all(source).or_else(|_| fs::remove_file(source));
        }
        Ok(())
    } else {
        run(sudo::command("/bin/mv").arg("--").arg(source).arg(target))
    }
}

fn remove_path(path: &Path) -> Result<(), Error> {
    let parent = path.parent().unwrap_or(path);
    if !writable(parent) {
        return run(sudo::command("/bin/rm").args(["-rf", "--"]).arg(path));
    }
    let removed = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        _ => fs::remove_file(path),
    };
    removed.map_err(|e| Error::StoreCorruption {
        message: format!("failed to remove '{}': {e}", path.display()),
    })
}

fn writable(dir: &Path) -> bool {
    tempfile::tempfile_in(dir).is_ok()
}

/// The wrapper script: the cask's `content`, or Homebrew's
/// `write_env_script` running `executable`.
fn wrapper_script(places: &Places, cask: &ResolvedCask, wrapper: &CommandWrapper) -> String {
    if let Some(content) = &wrapper.content {
        return places.expand(cask, content);
    }
    let executable = places.expand(cask, wrapper.executable.as_deref().unwrap_or_default());
    let env: String = wrapper
        .env
        .iter()
        .map(|(key, value)| format!("{key}=\"{}\" ", places.expand(cask, value)))
        .collect();
    let args: Vec<String> = wrapper
        .args
        .iter()
        .map(|arg| shellescape(&places.expand(cask, arg)))
        .collect();
    format!(
        "#!/bin/bash\n{env}exec \"{executable}\" {} \"$@\"\n",
        args.join(" ")
    )
}

/// Ruby's `Shellwords.shellescape`.
fn shellescape(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    let mut out = String::new();
    for c in arg.chars() {
        match c {
            '\n' => out.push_str("'\n'"),
            c if c.is_ascii_alphanumeric() || "_-.,:+/@".contains(c) => out.push(c),
            c => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    out
}

/// Run the cask's command once per shell and keep what it prints as that
/// shell's completions, in the keg. A command that fails is reported and
/// skipped, as in Homebrew.
fn generate_completions(places: &Places, cask: &ResolvedCask, completion: &GeneratedCompletion) {
    let Some(first) = completion.commands.first() else {
        return;
    };
    let first = places.expand(cask, first);
    // A command in the prefix's bin is this keg's own, not linked yet.
    let executable = if Path::new(&first).is_absolute() {
        places.destination(Path::new(&first)).0
    } else {
        places.staged.join(&first)
    };
    let base = completion.base_name.clone().unwrap_or_else(|| {
        executable
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    for shell in &completion.shells {
        let (param, mut env) = shell_parameter(completion.format.as_deref(), shell, &executable);
        env.push(("SHELL".to_string(), shell.clone()));
        let mut command = Command::new(&executable);
        command
            .args(
                completion.commands[1..]
                    .iter()
                    .map(|c| places.expand(cask, c)),
            )
            .args(param)
            .envs(env);
        let output = match output_within(&mut command, COMPLETION_TIMEOUT) {
            Ok(Some(out)) if out.status.success() => out.stdout,
            Ok(Some(out)) => {
                warn!(cask = %cask.token, shell, status = %out.status, "failed to generate completions");
                continue;
            }
            Ok(None) => {
                warn!(cask = %cask.token, shell, "generating completions did not finish; skipped");
                continue;
            }
            Err(e) => {
                warn!(cask = %cask.token, shell, error = %e, "failed to generate completions");
                continue;
            }
        };
        let stem = Path::new(&base)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| base.clone());
        let rel = match shell.as_str() {
            "bash" => format!("etc/bash_completion.d/{stem}"),
            "zsh" if base.starts_with('_') => format!("share/zsh/site-functions/{base}"),
            "zsh" => format!("share/zsh/site-functions/_{stem}"),
            "fish" if base.ends_with(".fish") => format!("share/fish/vendor_completions.d/{base}"),
            "fish" => format!("share/fish/vendor_completions.d/{stem}.fish"),
            "pwsh" => format!("share/pwsh/completions/_{stem}.ps1"),
            _ => continue,
        };
        let path = places.keg.join(rel);
        if let Some(parent) = path.parent()
            && fs::create_dir_all(parent).is_ok()
            && fs::write(&path, output).is_ok()
        {
            continue;
        }
        warn!(cask = %cask.token, shell, "failed to write generated completions");
    }
}

/// How long a cask's command may take to print its completions. A tool that
/// waits on something (a Gatekeeper check that cannot reach Apple, a
/// prompt) must not hold up the install.
const COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run `command` and collect its stdout, or kill it and give `None` once
/// `limit` has passed.
fn output_within(
    command: &mut Command,
    limit: std::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let stdout = reader.join().unwrap_or_default();
    Ok(Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    }))
}

/// Homebrew's `completion_shell_parameter`: the arguments that ask for
/// `shell`'s completions in `format`, and any environment it needs.
fn shell_parameter(
    format: Option<&str>,
    shell: &str,
    executable: &Path,
) -> (Vec<String>, Vec<(String, String)>) {
    let name = if shell == "pwsh" { "powershell" } else { shell };
    match format {
        None => (vec![name.to_string()], vec![]),
        Some("arg") => (vec![format!("--shell={name}")], vec![]),
        Some("clap") => (vec![], vec![("COMPLETE".to_string(), name.to_string())]),
        Some("click") => {
            let prog = executable
                .file_name()
                .map(|n| n.to_string_lossy().to_uppercase().replace('-', "_"))
                .unwrap_or_default();
            (
                vec![],
                vec![(format!("_{prog}_COMPLETE"), format!("{name}_source"))],
            )
        }
        Some("cobra") => (vec!["completion".to_string(), name.to_string()], vec![]),
        Some("flag") => (vec![format!("--{name}")], vec![]),
        Some("none") => (vec![], vec![]),
        Some("typer") => (
            vec!["--show-completion".to_string(), name.to_string()],
            vec![(
                "_TYPER_COMPLETE_TEST_DISABLE_SHELL_DETECTION".to_string(),
                "1".to_string(),
            )],
        ),
        Some(other) => (vec![format!("{other}{shell}")], vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        prefix: PathBuf,
        keg: PathBuf,
        staged: PathBuf,
        appdir: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let root = tmp.path().to_path_buf();
            let fixture = Self {
                prefix: root.join("prefix"),
                keg: root.join("prefix/Cellar/cask:x/1.0"),
                staged: root.join("prefix/Cellar/cask:x/1.0/staged"),
                appdir: root.join("Applications"),
                home: root.join("home"),
                _tmp: tmp,
            };
            for dir in [&fixture.staged, &fixture.appdir, &fixture.home] {
                fs::create_dir_all(dir).unwrap();
            }
            fixture
        }

        fn places(&self) -> Places<'_> {
            Places {
                prefix: &self.prefix,
                keg: &self.keg,
                staged: &self.staged,
                appdir: &self.appdir,
                home: &self.home,
            }
        }
    }

    fn cask(placed: Vec<PlacedArtifact>) -> ResolvedCask {
        ResolvedCask {
            token: "x".into(),
            version: "1.0".into(),
            placed,
            ..Default::default()
        }
    }

    fn placed(kind: &str, source: &str, target: &str, linked: bool) -> PlacedArtifact {
        PlacedArtifact {
            kind: kind.into(),
            source: source.into(),
            target: target.into(),
            linked,
        }
    }

    #[test]
    fn expands_the_placeholders_homebrew_does_and_no_others() {
        let f = Fixture::new();
        let places = f.places();
        let cask = cask(vec![]);
        assert_eq!(
            places.expand(&cask, "/$HOME/Library/Fonts/a.otf"),
            format!("{}/Library/Fonts/a.otf", f.home.display())
        );
        assert_eq!(
            places.expand(
                &cask,
                "cd '$HOMEBREW_PREFIX/Caskroom/x/1.0/gogs' && echo $HOME"
            ),
            format!("cd '{}/gogs' && echo $HOME", f.staged.display())
        );
        assert_eq!(
            places.expand(&cask, "$APPDIR/A.app"),
            format!("{}/A.app", f.appdir.display())
        );
    }

    #[test]
    fn fonts_move_to_their_target_and_go_on_uninstall() {
        let f = Fixture::new();
        fs::create_dir_all(f.staged.join("fonts")).unwrap();
        fs::write(f.staged.join("fonts/X.otf"), "font").unwrap();
        let cask = cask(vec![placed(
            "font",
            "fonts/X.otf",
            "/$HOME/Library/Fonts/X.otf",
            false,
        )]);

        place_moved(&f.places(), &cask).unwrap();

        let font = f.home.join("Library/Fonts/X.otf");
        assert_eq!(fs::read_to_string(&font).unwrap(), "font");
        // As Homebrew's post_move, the staged path now leads to it.
        assert_eq!(fs::read_link(f.staged.join("fonts/X.otf")).unwrap(), font);

        // A reinstall unpacks afresh and replaces this keg's own font.
        fs::remove_file(f.staged.join("fonts/X.otf")).unwrap();
        fs::write(f.staged.join("fonts/X.otf"), "font v2").unwrap();
        place_moved(&f.places(), &cask).unwrap();
        assert_eq!(fs::read_to_string(&font).unwrap(), "font v2");

        remove_placed(&f.keg).unwrap();
        assert!(!font.exists());
    }

    #[test]
    fn a_target_someone_else_put_there_is_left_alone() {
        let f = Fixture::new();
        fs::write(f.staged.join("X.otf"), "ours").unwrap();
        let font = f.home.join("Library/Fonts/X.otf");
        fs::create_dir_all(font.parent().unwrap()).unwrap();
        fs::write(&font, "theirs").unwrap();
        let cask = cask(vec![placed(
            "font",
            "X.otf",
            "/$HOME/Library/Fonts/X.otf",
            false,
        )]);

        let err = place_moved(&f.places(), &cask).unwrap_err();

        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(fs::read_to_string(&font).unwrap(), "theirs");
    }

    #[test]
    fn a_binary_with_a_home_target_is_linked_there_and_unlinked_on_uninstall() {
        // alacritty's terminfo entry.
        let f = Fixture::new();
        fs::create_dir_all(f.appdir.join("A.app/61")).unwrap();
        fs::write(f.appdir.join("A.app/61/a"), "terminfo").unwrap();
        let cask = cask(vec![placed(
            "binary",
            "$APPDIR/A.app/61/a",
            "/$HOME/.terminfo/61/a",
            true,
        )]);

        place_linked(&f.places(), &cask).unwrap();

        let link = f.home.join(".terminfo/61/a");
        assert_eq!(fs::read_link(&link).unwrap(), f.appdir.join("A.app/61/a"));
        assert!(!f.keg.join("bin").exists(), "not into bin");
        // Placing it again keeps one marker.
        place_linked(&f.places(), &cask).unwrap();
        assert_eq!(fs::read_dir(f.keg.join(PLACED_DIR)).unwrap().count(), 1);

        remove_placed(&f.keg).unwrap();
        assert!(link.symlink_metadata().is_err());
        assert!(f.appdir.join("A.app/61/a").exists(), "the source stays");
    }

    #[test]
    fn a_file_put_over_a_home_link_is_not_removed_on_uninstall() {
        let f = Fixture::new();
        fs::write(f.staged.join("a"), "ours").unwrap();
        let cask = cask(vec![placed("binary", "a", "/$HOME/.terminfo/61/a", true)]);
        place_linked(&f.places(), &cask).unwrap();
        let link = f.home.join(".terminfo/61/a");
        fs::remove_file(&link).unwrap();
        fs::write(&link, "theirs").unwrap();

        remove_placed(&f.keg).unwrap();

        assert_eq!(fs::read_to_string(&link).unwrap(), "theirs");
        assert!(
            !f.keg
                .join(PLACED_DIR)
                .join(format!("0{LINK_MARKER}"))
                .exists()
        );
    }

    #[test]
    fn a_home_target_someone_else_put_there_is_left_alone() {
        let f = Fixture::new();
        fs::write(f.staged.join("a"), "ours").unwrap();
        let file = f.home.join(".terminfo/61/a");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "theirs").unwrap();
        let cask = cask(vec![placed("binary", "a", "/$HOME/.terminfo/61/a", true)]);

        let err = place_linked(&f.places(), &cask).unwrap_err();

        assert!(err.to_string().contains("already exists"), "{err}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "theirs");
    }

    #[test]
    fn prefix_targets_are_made_in_the_keg_for_linking() {
        let f = Fixture::new();
        fs::create_dir_all(f.appdir.join("A.app/man")).unwrap();
        fs::write(f.appdir.join("A.app/man/a.1"), "man").unwrap();
        fs::create_dir_all(f.staged.join("tools/bin")).unwrap();
        let cask = cask(vec![
            placed(
                "artifact",
                "tools",
                "$HOMEBREW_PREFIX/share/t/latest",
                false,
            ),
            placed(
                "manpage",
                "$APPDIR/A.app/man/a.1",
                "$HOMEBREW_PREFIX/share/man/man1/a.1",
                true,
            ),
        ]);

        place_moved(&f.places(), &cask).unwrap();
        place_linked(&f.places(), &cask).unwrap();

        assert!(f.keg.join("share/t/latest/bin").is_dir());
        assert_eq!(
            fs::read_link(f.keg.join("share/man/man1/a.1")).unwrap(),
            f.appdir.join("A.app/man/a.1")
        );
        assert!(!f.keg.join(PLACED_DIR).exists(), "nothing outside the keg");
    }

    #[test]
    fn wrappers_run_their_executable_like_homebrews_env_script() {
        let f = Fixture::new();
        let mut cask = cask(vec![]);
        cask.wrappers = vec![
            CommandWrapper {
                name: "keka".into(),
                executable: Some("$APPDIR/Keka.app/Keka".into()),
                args: vec!["--cli".into(), "a b".into()],
                env: vec![("MODE".into(), "x".into())],
                ..Default::default()
            },
            CommandWrapper {
                name: "gogs".into(),
                content: Some("#!/bin/sh\ncd '$HOMEBREW_PREFIX/Caskroom/x/1.0/gogs'\n".into()),
                ..Default::default()
            },
        ];

        place_wrappers(&f.places(), &cask).unwrap();

        let keka = fs::read_to_string(f.keg.join("bin/keka")).unwrap();
        assert_eq!(
            keka,
            format!(
                "#!/bin/bash\nMODE=\"x\" exec \"{}/Keka.app/Keka\" --cli a\\ b \"$@\"\n",
                f.appdir.display()
            )
        );
        let gogs = fs::read_to_string(f.keg.join("bin/gogs")).unwrap();
        assert!(
            gogs.contains(&format!("cd '{}/gogs'", f.staged.display())),
            "{gogs}"
        );
        let mode = fs::metadata(f.keg.join("bin/keka"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555);
    }

    #[test]
    fn completions_are_generated_per_shell_into_the_keg() {
        let f = Fixture::new();
        let tool = f.staged.join("op");
        fs::write(&tool, "#!/bin/sh\necho \"complete $1 $2 for $SHELL\"\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let mut cask = cask(vec![]);
        cask.completions = vec![GeneratedCompletion {
            commands: vec!["op".into(), "completion".into()],
            shells: vec!["bash".into(), "zsh".into(), "fish".into()],
            ..Default::default()
        }];

        place_linked(&f.places(), &cask).unwrap();

        assert_eq!(
            fs::read_to_string(f.keg.join("etc/bash_completion.d/op")).unwrap(),
            "complete completion bash for bash\n"
        );
        assert!(f.keg.join("share/zsh/site-functions/_op").is_file());
        assert!(
            f.keg
                .join("share/fish/vendor_completions.d/op.fish")
                .is_file()
        );
    }

    #[test]
    fn a_command_that_hangs_is_given_up_on() {
        let start = std::time::Instant::now();
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let out = output_within(&mut command, std::time::Duration::from_millis(300)).unwrap();
        assert!(out.is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn generated_scripts_are_written_executable_into_the_staged_dir() {
        let f = Fixture::new();
        let mut cask = cask(vec![]);
        cask.generated_scripts = vec![("Setup.sh".into(), "#!/bin/bash\nls '$APPDIR'\n".into())];

        write_generated_scripts(&f.places(), &cask).unwrap();

        let script = f.staged.join("Setup.sh");
        assert_eq!(
            fs::read_to_string(&script).unwrap(),
            format!("#!/bin/bash\nls '{}'\n", f.appdir.display())
        );
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn shellescape_matches_ruby() {
        assert_eq!(shellescape("--cli"), "--cli");
        assert_eq!(shellescape("a b"), "a\\ b");
        assert_eq!(shellescape("$x"), "\\$x");
        assert_eq!(shellescape(""), "''");
    }
}
