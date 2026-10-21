//! `configure_php` and `bootstrap_cpython`, after Homebrew's
//! `InstallSteps::Runner#run_configure_php` and `#run_bootstrap_cpython`.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use nb_core::PathBase;
use regex::{NoExpand, Regex};

use super::paths::ruby_glob;
use super::steps::{copy_tree, install_symlink, remove_any, run_tool, write_atomically};
use super::template::expand;
use super::{PostInstallContext, StepError};

fn io(what: &str, path: &Path) -> impl FnOnce(std::io::Error) -> StepError {
    StepError::io(format!("{what} {}", path.display()))
}

fn mkdir_p(path: &Path) -> Result<(), StepError> {
    fs::create_dir_all(path).map_err(io("creating", path))
}

fn chmod(path: &Path, mode: u32) -> Result<(), StepError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io("setting mode of", path))
}

/// Set up PEAR under the shared prefix and point it at the keg, and on a
/// versioned PHP load its own opcache.
pub(super) fn configure_php(ctx: &PostInstallContext) -> Result<(), StepError> {
    let pear_prefix = ctx.base_path(PathBase::Pkgshare, None)?.join("pear");
    let mut pear_files: Vec<PathBuf> = [".depdblock", ".filemap", ".depdb", ".lock"]
        .iter()
        .map(|file| pear_prefix.join(file))
        .filter(|path| path.is_file())
        .collect();
    for dir in [
        pear_prefix.join(".channels"),
        pear_prefix.join(".channels/.alias"),
    ] {
        if !dir.is_dir() {
            continue;
        }
        chmod(&dir, 0o755)?;
        for entry in fs::read_dir(&dir).map_err(io("reading", &dir))? {
            let path = entry.map_err(io("reading", &dir))?.path();
            if path.is_file() {
                pear_files.push(path);
            }
        }
    }
    for file in &pear_files {
        chmod(file, 0o644)?;
    }

    let pecl = ctx.prefix.join("lib/php/pecl");
    mkdir_p(&pecl)?;
    let keg_pecl = ctx.keg.join("pecl");
    if keg_pecl.is_symlink() {
        fs::remove_file(&keg_pecl).map_err(io("removing", &keg_pecl))?;
    }
    if !keg_pecl.exists() {
        symlink(&pecl, &keg_pecl).map_err(io("linking", &keg_pecl))?;
    }
    let bin = ctx.base_path(PathBase::Bin, None)?;
    let ext_dir = run_tool(
        ctx,
        &bin.join("php-config"),
        &["--extension-dir".into()],
        |_| {},
    )?;
    let php_basename = Path::new(String::from_utf8_lossy(&ext_dir).trim())
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| StepError::Failed("php-config printed no extension dir".to_string()))?;
    mkdir_p(&pecl.join(&php_basename))?;

    let major_minor = expand(ctx, "{{version.major_minor}}")?;
    let pear_path = ctx.prefix.join("share").join(if ctx.name == "php" {
        "pear".to_string()
    } else {
        format!("pear@{major_minor}")
    });
    copy_tree(&pear_prefix, &pear_path)?;
    let opt = ctx.base_path(PathBase::OptPrefix, None)?;
    let php_etc = ctx
        .base_path(PathBase::Etc, None)?
        .join("php")
        .join(&major_minor);
    let pear = bin.join("pear");
    for (key, value) in [
        ("php_ini", php_etc.join("php.ini")),
        ("php_dir", pear_path.clone()),
        ("doc_dir", pear_path.join("doc")),
        ("ext_dir", pecl.join(&php_basename)),
        ("bin_dir", opt.join("bin")),
        ("data_dir", pear_path.join("data")),
        ("cfg_dir", pear_path.join("cfg")),
        ("www_dir", pear_path.join("htdocs")),
        ("man_dir", ctx.prefix.join("share/man")),
        ("test_dir", pear_path.join("test")),
        ("php_bin", opt.join("bin/php")),
    ] {
        if key.ends_with("_dir") && key != "bin_dir" && key != "man_dir" {
            mkdir_p(&value)?;
        }
        let args = [
            "config-set".into(),
            key.into(),
            value.into(),
            "system".into(),
        ];
        run_tool(ctx, &pear, &args, |_| {})?;
    }
    run_tool(ctx, &pear, &["update-channels".into()], |_| {})?;
    if ctx.name == "php" {
        return Ok(());
    }

    let ini = php_etc.join("conf.d/ext-opcache.ini");
    let line = format!(
        "zend_extension=\"{}/opcache.so\"",
        opt.join("lib/php").join(&php_basename).display()
    );
    let content = match fs::read_to_string(&ini) {
        Ok(old) => Regex::new(r"(?m)^\s*zend_extension\s*=.*$")
            .unwrap()
            .replace_all(&old, NoExpand(&line))
            .into_owned(),
        Err(_) => format!("[opcache]\n{line}\n"),
    };
    mkdir_p(ini.parent().unwrap_or(&php_etc))?;
    write_atomically(&ini, content.as_bytes())
}

/// Share one site-packages under the prefix between a CPython's kegs and
/// install pip, setuptools and wheel into it.
pub(super) fn bootstrap_cpython(ctx: &PostInstallContext) -> Result<(), StepError> {
    let major_minor = expand(ctx, "{{version.major_minor}}")?;
    // ponytail: python@3.9's distutils.cfg and setuptools patch are left
    // out; 3.9 is gone from homebrew-core.
    if major_minor == "3.9" {
        return Err(StepError::Unsupported(
            "bootstrap_cpython for Python 3.9".to_string(),
        ));
    }
    let python_dir = format!("python{major_minor}");
    let site_packages = ctx
        .prefix
        .join("lib")
        .join(&python_dir)
        .join("site-packages");
    let lib_cellar = if cfg!(target_os = "macos") {
        ctx.base_path(PathBase::Frameworks, None)?
            .join("Python.framework/Versions")
            .join(&major_minor)
            .join("lib")
            .join(&python_dir)
    } else {
        ctx.base_path(PathBase::Lib, None)?.join(&python_dir)
    };
    mkdir_p(&site_packages)?;
    let site_packages_cellar = lib_cellar.join("site-packages");
    remove_any(&site_packages_cellar)?;
    install_symlink(&site_packages, &site_packages_cellar)?;
    let mut stale = ruby_glob(&site_packages.join("sitecustomize.py[co]"));
    for package in ["setuptools", "distribute", "pip", "wheel"] {
        stale.extend(ruby_glob(
            &site_packages.join(format!("{package}[-_.][0-9]*")),
        ));
        stale.push(site_packages.join(package));
    }
    for path in &stale {
        remove_any(path)?;
    }

    let bin = ctx.base_path(PathBase::Bin, None)?;
    let libexec = ctx.base_path(PathBase::Libexec, None)?;
    let python = bin.join(&python_dir);
    let no_pythonpath = |cmd: &mut Command| {
        cmd.env_remove("PYTHONPATH");
    };
    run_tool(
        ctx,
        &python,
        &["-Im".into(), "ensurepip".into()],
        no_pythonpath,
    )?;
    let bundled = lib_cellar.join("ensurepip/_bundled");
    let mut args: Vec<OsString> = ["-Im", "pip", "install", "-v", "--no-deps", "--no-index"]
        .into_iter()
        .chain(["--upgrade", "--isolated"])
        .map(OsString::from)
        .collect();
    args.push(format!("--target={}", site_packages.display()).into());
    for pattern in [
        bundled.join("setuptools-*-py3-none-any.whl"),
        bundled.join("pip-*-py3-none-any.whl"),
        libexec.join("wheel-*-py3-none-any.whl"),
    ] {
        match ruby_glob(&pattern).as_slice() {
            [wheel] => args.push(wheel.into()),
            _ => {
                return Err(StepError::Failed(format!(
                    "CPython bootstrap wheel must match exactly one path: {}",
                    pattern.display()
                )));
            }
        }
    }
    run_tool(ctx, &python, &args, no_pythonpath)?;

    let installed_bin = site_packages.join("bin");
    for entry in fs::read_dir(&installed_bin).map_err(io("reading", &installed_bin))? {
        let entry = entry.map_err(io("reading", &installed_bin))?;
        let to = bin.join(entry.file_name());
        fs::rename(entry.path(), &to).map_err(io("moving into", &to))?;
    }
    fs::remove_dir(&installed_bin).map_err(io("removing", &installed_bin))?;
    remove_any(&bin.join("pip"))?;
    remove_any(&bin.join("pip3"))?;
    let pip = bin.join(format!("pip{major_minor}"));
    let wheel = bin.join(format!("wheel{major_minor}"));
    fs::rename(bin.join("wheel"), &wheel).map_err(io("renaming to", &wheel))?;
    for (short, long) in [
        ("pip", &pip),
        ("pip3", &pip),
        ("wheel", &wheel),
        ("wheel3", &wheel),
    ] {
        let target = long.canonicalize().map_err(io("resolving", long))?;
        install_symlink(&target, &libexec.join("bin").join(short))?;
    }
    for tool in [&wheel, &pip] {
        install_symlink(
            tool,
            &ctx.prefix
                .join("bin")
                .join(tool.file_name().unwrap_or_default()),
        )?;
    }

    // Homebrew's make_cpython_venv_activation_scripts_writable: u+w.
    for entry in walkdir::WalkDir::new(lib_cellar.join("venv/scripts"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let mode = entry
            .metadata()
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644);
        chmod(entry.path(), mode | 0o200)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::context;
    use super::*;
    use tempfile::TempDir;

    fn script(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        chmod(path, 0o755).unwrap();
    }

    #[test]
    fn php_points_pear_at_the_prefix_and_its_own_opcache() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "php@8.3", "8.3.20");
        let keg = ctx.keg.clone();
        let log = tmp.path().join("pear.log");
        script(&keg.join("bin/php-config"), "echo /x/lib/php/20230831");
        script(
            &keg.join("bin/pear"),
            &format!("echo \"$@\" >> {}", log.display()),
        );
        fs::create_dir_all(keg.join("share/php@8.3/pear/.channels")).unwrap();
        fs::write(
            keg.join("share/php@8.3/pear/.channels/pear.php.net.reg"),
            "c",
        )
        .unwrap();

        configure_php(&ctx).unwrap();
        let pear_path = ctx.prefix.join("share/pear@8.3");
        assert!(pear_path.join(".channels/pear.php.net.reg").is_file());
        assert!(pear_path.join("htdocs").is_dir());
        assert!(ctx.prefix.join("lib/php/pecl/20230831").is_dir());
        assert_eq!(
            fs::read_link(keg.join("pecl")).unwrap(),
            ctx.prefix.join("lib/php/pecl")
        );
        let log = fs::read_to_string(log).unwrap();
        assert!(log.contains(&format!(
            "config-set php_dir {} system",
            pear_path.display()
        )));
        assert!(log.ends_with("update-channels\n"));
        let ini =
            fs::read_to_string(ctx.prefix.join("etc/php/8.3/conf.d/ext-opcache.ini")).unwrap();
        let opt = ctx.prefix.join("opt/php@8.3/lib/php/20230831/opcache.so");
        assert_eq!(
            ini,
            format!("[opcache]\nzend_extension=\"{}\"\n", opt.display())
        );
    }
}
