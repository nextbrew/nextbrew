//! Turn a step's `PathSpec` into real paths: base directories, token
//! expansion and glob matching.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use globset::GlobBuilder;
use nb_core::{PathBase, PathSpec};
use walkdir::WalkDir;

use super::template::expand;
use super::{PostInstallContext, StepError};

impl PostInstallContext {
    /// The directory a base refers to. `formula` is the other formula for
    /// the `formula_*` bases.
    pub(super) fn base_path(
        &self,
        base: PathBase,
        formula: Option<&str>,
    ) -> Result<PathBuf, StepError> {
        let other = || {
            formula
                .map(|name| name.rsplit('/').next().unwrap_or(name).to_string())
                .ok_or_else(|| StepError::Failed("step base needs a formula".to_string()))
        };
        Ok(match base {
            PathBase::Absolute | PathBase::Relative | PathBase::SearchPath => {
                return Err(StepError::Failed(format!(
                    "base {base:?} does not name a directory"
                )));
            }
            PathBase::Prefix => self.keg.clone(),
            PathBase::OptPrefix => self.prefix.join("opt").join(&self.name),
            PathBase::Bin => self.keg.join("bin"),
            PathBase::Sbin => self.keg.join("sbin"),
            PathBase::Lib => self.keg.join("lib"),
            PathBase::Libexec => self.keg.join("libexec"),
            PathBase::Share => self.keg.join("share"),
            PathBase::Pkgshare => self.keg.join("share").join(&self.name),
            PathBase::Var => self.prefix.join("var"),
            PathBase::Etc => self.prefix.join("etc"),
            PathBase::Pkgetc => self.prefix.join("etc").join(&self.name),
            PathBase::Frameworks => self.keg.join("Frameworks"),
            PathBase::Rack => self.cellar.join(&self.name),
            PathBase::HomebrewPrefix => self.prefix.clone(),
            PathBase::Home => std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| StepError::Failed("HOME is not set".to_string()))?,
            PathBase::Temp => self.temp.clone(),
            PathBase::BashCompletion => self.keg.join("etc/bash_completion.d"),
            PathBase::ZshCompletion => self.keg.join("share/zsh/site-functions"),
            PathBase::FishCompletion => self.keg.join("share/fish/vendor_completions.d"),
            PathBase::PwshCompletion => self.keg.join("share/pwsh/completions"),
            PathBase::FormulaPkgetc => self.prefix.join("etc").join(other()?),
            PathBase::FormulaOptPrefix => self.prefix.join("opt").join(other()?),
            PathBase::StagedPath | PathBase::Appdir | PathBase::CaskroomPath => {
                let cask = self
                    .cask
                    .as_ref()
                    .ok_or_else(|| StepError::Failed(format!("base {base:?} is only for casks")))?;
                match base {
                    PathBase::StagedPath => cask.staged.clone(),
                    PathBase::Appdir => cask.appdir.clone(),
                    _ => cask.caskroom.clone(),
                }
            }
        })
    }
}

/// The path a spec names. Globs are not expanded here.
pub(super) fn resolve(ctx: &PostInstallContext, spec: &PathSpec) -> Result<PathBuf, StepError> {
    let path = PathBuf::from(expand(ctx, &spec.path)?);
    Ok(match spec.base {
        PathBase::Absolute => absolute(path)?,
        PathBase::Relative => path,
        PathBase::SearchPath => {
            return Err(StepError::Failed(
                "a search_path spec names several paths; expand it as a glob".to_string(),
            ));
        }
        base => ctx.base_path(base, spec.formula.as_deref())?.join(path),
    })
}

/// A command to run: a bare name is looked up on `PATH`, anything with a
/// base is a path.
pub(super) fn command(ctx: &PostInstallContext, spec: &PathSpec) -> Result<OsString, StepError> {
    match spec.base {
        PathBase::Absolute | PathBase::Relative => Ok(expand(ctx, &spec.path)?.into()),
        _ => resolve(ctx, spec).map(PathBuf::into_os_string),
    }
}

/// What a symlink points at. A `relative` source is written verbatim.
pub(super) fn link_target(ctx: &PostInstallContext, spec: &PathSpec) -> Result<PathBuf, StepError> {
    match spec.base {
        PathBase::Relative => Ok(PathBuf::from(expand(ctx, &spec.path)?)),
        _ => resolve(ctx, spec),
    }
}

/// Every path a spec names, expanding `*`, `?`, `[..]`, `{a,b}` and `**`.
/// A spec without glob characters yields itself whether or not it exists.
pub(super) fn expand_glob(
    ctx: &PostInstallContext,
    spec: &PathSpec,
) -> Result<Vec<PathBuf>, StepError> {
    if spec.base == PathBase::SearchPath {
        let relative = PathBuf::from(expand(ctx, &spec.path)?);
        let mut found = Vec::new();
        for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            let candidate = dir.join(&relative);
            if has_glob_chars(&candidate) {
                found.extend(glob(&candidate)?);
            } else {
                found.push(candidate);
            }
        }
        return Ok(found);
    }

    let path = resolve(ctx, spec)?;
    if has_glob_chars(&path) {
        glob(&path)
    } else {
        Ok(vec![path])
    }
}

/// Whether any path a spec names exists. A dangling symlink does not.
pub(super) fn any_exists(ctx: &PostInstallContext, spec: &PathSpec) -> Result<bool, StepError> {
    Ok(expand_glob(ctx, spec)?.iter().any(|path| path.exists()))
}

/// Make a relative path absolute against the working directory.
/// Ruby's `Pathname#expand_path`: a leading `~` is the home folder, and a
/// relative path is taken from the working directory.
fn absolute(path: PathBuf) -> Result<PathBuf, StepError> {
    if path.is_absolute() {
        return Ok(path);
    }
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Ok(PathBuf::from(home).join(rest));
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(StepError::io("cannot resolve a relative path"))
}

fn has_glob_chars(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .contains(['*', '?', '[', '{'])
}

/// Paths matching an absolute glob pattern, sorted.
fn glob(pattern: &Path) -> Result<Vec<PathBuf>, StepError> {
    let pattern_text = pattern.to_string_lossy();
    let matcher = GlobBuilder::new(&pattern_text)
        .literal_separator(true)
        .build()
        .map_err(|e| StepError::Failed(format!("bad glob '{pattern_text}': {e}")))?
        .compile_matcher();

    // Walk from the deepest directory that has no glob characters.
    let mut root = PathBuf::new();
    for component in pattern.components() {
        let next = root.join(component.as_os_str());
        if matches!(component, Component::Normal(_))
            && has_glob_chars(Path::new(component.as_os_str()))
        {
            break;
        }
        root = next;
    }
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    // As Ruby's `Dir.glob` without `File::FNM_DOTMATCH`, a name starting
    // with a dot only matches a pattern segment that starts with one.
    let dot_segments: Vec<globset::GlobMatcher> = pattern
        .strip_prefix(&root)
        .unwrap_or(pattern)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .filter(|segment| segment.starts_with('.') || segment.starts_with("{."))
        .filter_map(|segment| GlobBuilder::new(&segment).build().ok())
        .map(|glob| glob.compile_matcher())
        .collect();
    let hidden_allowed = |path: &Path| {
        path.strip_prefix(&root)
            .unwrap_or(path)
            .components()
            .map(|c| c.as_os_str())
            .filter(|name| name.to_string_lossy().starts_with('.'))
            .all(|name| dot_segments.iter().any(|m| m.is_match(name)))
    };

    let mut found: Vec<PathBuf> = WalkDir::new(&root)
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .map(walkdir::DirEntry::into_path)
        .filter(|path| matcher.is_match(path) && hidden_allowed(path))
        .collect();
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::context;
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn bases_resolve_like_homebrew() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "node", "25.0.0_1");
        let p = |base, path| resolve(&ctx, &PathSpec::based(base, path)).unwrap();
        assert_eq!(p(PathBase::Prefix, "lib"), ctx.keg.join("lib"));
        assert_eq!(p(PathBase::Bin, "npm"), ctx.keg.join("bin/npm"));
        assert_eq!(p(PathBase::Libexec, "x"), ctx.keg.join("libexec/x"));
        assert_eq!(p(PathBase::Frameworks, "P"), ctx.keg.join("Frameworks/P"));
        assert_eq!(p(PathBase::Pkgshare, "a"), ctx.keg.join("share/node/a"));
        assert_eq!(p(PathBase::Var, "run"), ctx.prefix.join("var/run"));
        assert_eq!(p(PathBase::Etc, "x"), ctx.prefix.join("etc/x"));
        assert_eq!(p(PathBase::Pkgetc, "x"), ctx.prefix.join("etc/node/x"));
        assert_eq!(
            p(PathBase::OptPrefix, "bin"),
            ctx.prefix.join("opt/node/bin")
        );
        assert_eq!(p(PathBase::Rack, "25"), ctx.cellar.join("node/25"));
        assert_eq!(p(PathBase::HomebrewPrefix, "lib"), ctx.prefix.join("lib"));
        assert_eq!(p(PathBase::Temp, "t"), ctx.temp.join("t"));
        assert_eq!(
            p(PathBase::ZshCompletion, "_n"),
            ctx.keg.join("share/zsh/site-functions/_n")
        );
    }

    #[test]
    fn formula_bases_need_a_formula() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "node", "1");
        let spec = PathSpec {
            path: "cert.pem".into(),
            base: PathBase::FormulaPkgetc,
            formula: Some("openssl@3".into()),
        };
        assert_eq!(
            resolve(&ctx, &spec).unwrap(),
            ctx.prefix.join("etc/openssl@3/cert.pem")
        );
        let spec = PathSpec::based(PathBase::FormulaOptPrefix, "bin");
        assert!(matches!(resolve(&ctx, &spec), Err(StepError::Failed(_))));
    }

    #[test]
    fn absolute_specs_expand_a_leading_tilde_like_ruby() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let spec = PathSpec::absolute("~/.config/gcloud/virtenv");
        assert_eq!(
            resolve(&ctx, &spec).unwrap(),
            home.join(".config/gcloud/virtenv")
        );
    }

    #[test]
    fn absolute_specs_expand_tokens() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "node", "1");
        let spec = PathSpec::absolute("{{HOMEBREW_PREFIX}}/lib/node_modules");
        assert_eq!(
            resolve(&ctx, &spec).unwrap(),
            ctx.prefix.join("lib/node_modules")
        );
    }

    #[test]
    fn command_keeps_bare_names_for_path_lookup() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        assert_eq!(
            command(&ctx, &PathSpec::absolute("fc-cache")).unwrap(),
            OsString::from("fc-cache")
        );
        assert_eq!(
            command(&ctx, &PathSpec::based(PathBase::Bin, "fc-cache")).unwrap(),
            ctx.keg.join("bin/fc-cache").into_os_string()
        );
    }

    #[test]
    fn relative_link_targets_are_verbatim() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let spec = PathSpec::based(PathBase::Relative, "../lib/libx.dylib");
        assert_eq!(
            link_target(&ctx, &spec).unwrap(),
            PathBuf::from("../lib/libx.dylib")
        );
    }

    #[test]
    fn globs_expand_braces_stars_and_double_stars() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let man = ctx.prefix.join("man1");
        fs::create_dir_all(man.join("deep")).unwrap();
        for name in [
            "npm.1",
            "npx.1",
            "package-json.5",
            "other.1",
            "deep/npm-x.1",
        ] {
            fs::write(man.join(name), "").unwrap();
        }
        let spec = PathSpec::absolute(format!("{}/{{npm,npx,package-}}*", man.display()));
        let found = expand_glob(&ctx, &spec).unwrap();
        assert_eq!(
            found,
            vec![
                man.join("npm.1"),
                man.join("npx.1"),
                man.join("package-json.5")
            ]
        );

        let spec = PathSpec::absolute(format!("{}/**/*", man.display()));
        let found = expand_glob(&ctx, &spec).unwrap();
        assert!(found.contains(&man.join("deep/npm-x.1")));
        assert!(found.contains(&man.join("other.1")));

        let spec = PathSpec::absolute(format!("{}/*crt?.o", man.display()));
        assert!(expand_glob(&ctx, &spec).unwrap().is_empty());
    }

    #[test]
    fn globs_skip_dot_names_unless_the_pattern_names_them() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let channels = ctx.prefix.join("pear/.channels");
        fs::create_dir_all(channels.join(".alias")).unwrap();
        fs::write(channels.join("pecl.reg"), "").unwrap();
        fs::write(channels.join(".alias/pecl.txt"), "").unwrap();
        let glob = |pattern: &str| {
            let spec = PathSpec::absolute(format!("{}/{pattern}", ctx.prefix.display()));
            expand_glob(&ctx, &spec).unwrap()
        };
        assert_eq!(glob("pear/.channels/*"), vec![channels.join("pecl.reg")]);
        assert_eq!(
            glob("pear/.channels/.alias/*"),
            vec![channels.join(".alias/pecl.txt")]
        );
        assert_eq!(glob("pear/.channels/.*"), vec![channels.join(".alias")]);
        assert_eq!(glob("**/*.reg"), Vec::<PathBuf>::new());
    }

    #[test]
    fn plain_paths_are_returned_even_when_missing() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let spec = PathSpec::based(PathBase::Var, "missing/file");
        assert_eq!(
            expand_glob(&ctx, &spec).unwrap(),
            vec![ctx.prefix.join("var/missing/file")]
        );
        assert!(!any_exists(&ctx, &spec).unwrap());
    }

    #[test]
    fn search_path_looks_in_every_path_directory() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("tool"), "").unwrap();
        // Other tests run commands concurrently, so PATH is extended, not
        // replaced, and put back afterwards.
        let saved = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![bin.clone()];
        dirs.extend(std::env::split_paths(&saved));
        let extended = std::env::join_paths(dirs).unwrap();
        // SAFETY: no other thread reads PATH through a borrowed pointer.
        unsafe { std::env::set_var("PATH", &extended) };
        let found = any_exists(&ctx, &PathSpec::based(PathBase::SearchPath, "tool"));
        unsafe { std::env::set_var("PATH", &saved) };
        assert!(found.unwrap());
    }
}
