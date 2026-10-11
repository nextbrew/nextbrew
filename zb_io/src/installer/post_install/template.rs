//! `{{token}}` expansion in step strings.
//!
//! Homebrew serialises formula paths as tokens so the same steps work in
//! any prefix. A token we do not know is left as it is, which is also what
//! Homebrew does, so literal braces in a step are never rewritten.

use std::sync::OnceLock;

use regex::Regex;
use zb_core::PathBase;

use super::{PostInstallContext, StepError};

pub(super) fn expand(ctx: &PostInstallContext, text: &str) -> Result<String, StepError> {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let token = TOKEN.get_or_init(|| Regex::new(r"\{\{([A-Za-z_][\w.]*)\}\}").unwrap());

    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in token.captures_iter(text) {
        let whole = found.get(0).unwrap();
        out.push_str(&text[last..whole.start()]);
        match value(ctx, &found[1])? {
            Some(value) => out.push_str(&value),
            None => out.push_str(whole.as_str()),
        }
        last = whole.end();
    }
    out.push_str(&text[last..]);
    Ok(out)
}

fn value(ctx: &PostInstallContext, token: &str) -> Result<Option<String>, StepError> {
    let path = |base: PathBase| {
        ctx.base_path(base, None)
            .map(|p| Some(p.display().to_string()))
    };
    match token {
        "HOMEBREW_PREFIX" => path(PathBase::HomebrewPrefix),
        "HOMEBREW_CELLAR" => Ok(Some(ctx.cellar.display().to_string())),
        "formula_name" | "name" => Ok(Some(ctx.name.clone())),
        "arch" => Ok(Some(arch().to_string())),
        "user" => std::env::var("USER")
            .map(Some)
            .map_err(|_| StepError::Failed("USER is not set".to_string())),
        "version" => Ok(Some(ctx.version.clone())),
        "version.major" => Ok(Some(version_tokens(&ctx.version).take(1).collect())),
        "version.major_minor" => Ok(Some(
            version_tokens(&ctx.version)
                .take(2)
                .collect::<Vec<_>>()
                .join("."),
        )),
        "prefix" => path(PathBase::Prefix),
        "opt_prefix" => path(PathBase::OptPrefix),
        "bin" => path(PathBase::Bin),
        "sbin" => path(PathBase::Sbin),
        "lib" => path(PathBase::Lib),
        "libexec" => path(PathBase::Libexec),
        "share" => path(PathBase::Share),
        "pkgshare" => path(PathBase::Pkgshare),
        "var" => path(PathBase::Var),
        "etc" => path(PathBase::Etc),
        "pkgetc" => path(PathBase::Pkgetc),
        "temp" => path(PathBase::Temp),
        "rack" => path(PathBase::Rack),
        "bash_completion" => path(PathBase::BashCompletion),
        "zsh_completion" => path(PathBase::ZshCompletion),
        "fish_completion" => path(PathBase::FishCompletion),
        "pwsh_completion" => path(PathBase::PwshCompletion),
        "token" if ctx.cask.is_some() => Ok(Some(ctx.name.clone())),
        "staged_path" if ctx.cask.is_some() => path(PathBase::StagedPath),
        "appdir" if ctx.cask.is_some() => path(PathBase::Appdir),
        "caskroom_path" if ctx.cask.is_some() => path(PathBase::CaskroomPath),
        _ => Ok(None),
    }
}

/// Homebrew's name for the CPU: `arm64` or `x86_64`.
pub(super) fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

/// The numeric parts of a version, split the way Homebrew's `Version`
/// tokenises: `3.14.8` gives `3`, `14`, `8`; `1.2.3_1` gives `1`, `2`,
/// `3`, `1`.
fn version_tokens(version: &str) -> impl Iterator<Item = &str> {
    version
        .split(['.', '-', '_'])
        .filter(|part| !part.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::context;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn expands_paths_and_names() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "python@3.14", "3.14.8");
        let prefix = ctx.prefix.display().to_string();
        let keg = ctx.keg.display().to_string();
        assert_eq!(
            expand(&ctx, "{{HOMEBREW_PREFIX}}/lib/node_modules").unwrap(),
            format!("{prefix}/lib/node_modules")
        );
        assert_eq!(
            expand(&ctx, "{{pkgetc}}/cert.pem").unwrap(),
            format!("{prefix}/etc/python@3.14/cert.pem")
        );
        assert_eq!(
            expand(&ctx, "{{pkgshare}}").unwrap(),
            format!("{keg}/share/python@3.14")
        );
        assert_eq!(
            expand(&ctx, "{{libexec}}/x").unwrap(),
            format!("{keg}/libexec/x")
        );
        assert_eq!(
            expand(&ctx, "{{opt_prefix}}").unwrap(),
            format!("{prefix}/opt/python@3.14")
        );
        assert_eq!(
            expand(&ctx, "{{formula_name}}-{{name}}").unwrap(),
            "python@3.14-python@3.14"
        );
        assert_eq!(
            expand(&ctx, "{{HOMEBREW_CELLAR}}").unwrap(),
            ctx.cellar.display().to_string()
        );
    }

    #[test]
    fn version_tokens_match_homebrew() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "python@3.14", "3.14.8");
        assert_eq!(
            expand(&ctx, "python{{version.major_minor}}").unwrap(),
            "python3.14"
        );
        assert_eq!(expand(&ctx, "{{version.major}}").unwrap(), "3");
        assert_eq!(expand(&ctx, "{{version}}").unwrap(), "3.14.8");

        let ctx = context(tmp.path(), "libedit", "20260512-3.1");
        assert_eq!(
            expand(&ctx, "{{version.major_minor}}").unwrap(),
            "20260512.3"
        );
        let ctx = context(tmp.path(), "one", "7");
        assert_eq!(expand(&ctx, "{{version.major_minor}}").unwrap(), "7");
    }

    #[test]
    fn unknown_tokens_and_stray_braces_are_left_alone() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        assert_eq!(
            expand(&ctx, "{{token}} {{ not a token }} {x}").unwrap(),
            "{{token}} {{ not a token }} {x}"
        );
    }

    #[test]
    fn arch_and_user() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "x", "1");
        assert!(matches!(
            expand(&ctx, "{{arch}}").unwrap().as_str(),
            "arm64" | "x86_64"
        ));
        if let Ok(user) = std::env::var("USER") {
            assert_eq!(expand(&ctx, "u={{user}}").unwrap(), format!("u={user}"));
        }
    }
}
