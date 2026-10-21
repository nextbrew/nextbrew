use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nb_core::BuildPlan;

/// What of the user's environment a build keeps, as `bin/brew` keeps it
/// (besides every `HOMEBREW_*`, and nextbrew's own `NEXTBREW_*`).
const KEPT_VARIABLES: &[&str] = &[
    "BAT_CONFIG_PATH",
    "BAT_THEME",
    "SHELL",
    "TERMINFO",
    "TERMINFO_DIRS",
    "COLUMNS",
    "DISPLAY",
    "LOGNAME",
    "USER",
    "CI",
    "SSH_AUTH_SOCK",
    "SUDO_ASKPASS",
    "http_proxy",
    "https_proxy",
    "ftp_proxy",
    "no_proxy",
    "all_proxy",
    "HTTPS_PROXY",
    "FTP_PROXY",
    "ALL_PROXY",
];

/// The package managers' caches Homebrew points a build at
/// (`Homebrew::PackageManagerCache.env`), under nextbrew's cache.
const PACKAGE_MANAGER_CACHES: &[(&str, &str)] = &[
    ("BUNDLE_USER_CACHE", "bundler_cache"),
    ("CABAL_DIR", "cabal_cache"),
    ("CARGO_HOME", "cargo_cache"),
    ("COMPOSER_CACHE_DIR", "composer_cache"),
    ("GEM_SPEC_CACHE", "gem_cache"),
    ("GOCACHE", "go_cache"),
    ("GOPATH", "go_mod_cache"),
    ("HEX_HOME", "hex_cache"),
    ("NPM_CONFIG_CACHE", "npm_cache"),
    ("NPM_CONFIG_STORE_DIR", "pnpm_cache"),
    ("NUGET_PACKAGES", "nuget_cache"),
    ("PIP_CACHE_DIR", "pip_cache"),
    ("UV_CACHE_DIR", "uv_cache"),
    ("YARN_CACHE_FOLDER", "yarn_cache"),
    ("ZIG_GLOBAL_CACHE_DIR", "zig_cache"),
];

/// The settings Homebrew's build environment fixes for some build tools
/// (vendored libraries, Bundler, opam, `ar` dates).
const BUILD_SETTINGS: &[(&str, &str)] = &[
    ("BUNDLER_NO_OLD_RUBYGEMS_WARNING", "1"),
    ("BUNDLE_COOLDOWN", "1"),
    ("BUNDLE_FORCE_RUBY_PLATFORM", "true"),
    ("BUNDLE_VERSION", "system"),
    ("BUNDLE_WITHOUT", "development:test"),
    ("HIDAPI_SYSTEM_HIDAPI", "1"),
    ("MATURIN_NO_INSTALL_RUST", "1"),
    ("OPAMNODEPEXTS", "1"),
    ("OPAMYES", "1"),
    ("OPENSSL_NO_VENDOR", "1"),
    ("PYZMQ_NO_BUNDLE", "1"),
    ("SODIUM_INSTALL", "system"),
    ("ZERO_AR_DATE", "1"),
];

/// `EnvSensitive#sensitive?`: what Homebrew clears before a build.
fn sensitive(key: &str) -> bool {
    let key = key.to_lowercase();
    ["cookie", "key", "token", "password", "passphrase", "auth"]
        .iter()
        .any(|word| key.contains(word))
}

/// What a build keeps of the user's environment `vars`.
fn kept_user_env(vars: impl Iterator<Item = (String, String)>) -> HashMap<String, String> {
    vars.filter(|(key, value)| {
        !value.is_empty()
            && (KEPT_VARIABLES.contains(&key.as_str())
                || key.starts_with("HOMEBREW_")
                || key.starts_with("NEXTBREW_"))
            && !sensitive(key)
    })
    .collect()
}

/// `HOMEBREW_TEMP`: where builds keep their temporary files.
pub fn temp_dir() -> PathBuf {
    match std::env::var_os("HOMEBREW_TEMP").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None if cfg!(target_os = "macos") => PathBuf::from("/private/tmp"),
        None => PathBuf::from("/tmp"),
    }
}

/// Where a formula's build logs go (`Formula#logs`), as Homebrew's
/// `HOMEBREW_LOGS` but nextbrew's: `~/Library/Logs/nextbrew` on macOS,
/// `~/.cache/nextbrew/Logs` elsewhere.
pub fn logs_dir(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let root = if cfg!(target_os = "macos") {
        home.join("Library/Logs/nextbrew")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("nextbrew/Logs")
    };
    root.join(name)
}

/// A build's whole environment, as Homebrew gives one: only the variables
/// `bin/brew` keeps from the user's (no `CFLAGS`, `GOPATH`, CA settings
/// and the like), less anything that looks like a secret, with a home of
/// its own in `buildpath` (`Formula#stage`'s `common_sandbox_env`), the
/// package managers' caches, and the prefix's tools and libraries.
pub fn build_env(
    plan: &BuildPlan,
    prefix: &Path,
    buildpath: &Path,
    logs: &Path,
) -> HashMap<String, String> {
    let mut env = kept_user_env(std::env::vars());

    let bin_dir = prefix.join("bin");
    let lib_dir = prefix.join("lib");
    let include_dir = prefix.join("include");
    let pkgconfig_dir = lib_dir.join("pkgconfig");

    env.insert(
        "PATH".into(),
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin_dir.display()),
    );
    env.insert(
        "PKG_CONFIG_PATH".into(),
        pkgconfig_dir.display().to_string(),
    );
    env.insert("CFLAGS".into(), format!("-I{}", include_dir.display()));
    env.insert("CPPFLAGS".into(), format!("-I{}", include_dir.display()));
    env.insert("LDFLAGS".into(), format!("-L{}", lib_dir.display()));

    // `common_sandbox_env`. The user's curl configuration stays, as
    // nextbrew fetches a formula's resources and patches in the build.
    let home = buildpath.join(".brew_home");
    let temp = temp_dir();
    let user_home = std::env::var("HOME").unwrap_or_default();
    let cache = prefix.join("cache");
    for (key, dir) in PACKAGE_MANAGER_CACHES {
        env.insert((*key).into(), cache.join(dir).display().to_string());
    }
    env.insert("BUNDLE_GLOBAL_GEM_CACHE".into(), "true".into());
    env.insert(
        "_JAVA_OPTIONS".into(),
        format!(
            "-Duser.home={} -Djava.io.tmpdir={}",
            cache.join("java_cache").display(),
            temp.display()
        ),
    );
    env.insert("HOME".into(), home.display().to_string());
    for key in ["TMPDIR", "TEMP", "TMP"] {
        env.insert(key.into(), temp.display().to_string());
    }
    env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    env.insert("GOENV".into(), "off".into());
    env.insert("GOTOOLCHAIN".into(), "local".into());
    env.insert("PIP_CONFIG_FILE".into(), "/dev/null".into());
    env.insert("NPM_CONFIG_USERCONFIG".into(), "/dev/null".into());
    env.insert(
        "CURL_HOME".into(),
        std::env::var("CURL_HOME").unwrap_or(user_home),
    );
    env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    env.insert(
        "XDG_CONFIG_HOME".into(),
        home.join(".config").display().to_string(),
    );
    for (key, value) in BUILD_SETTINGS {
        env.insert((*key).into(), (*value).into());
    }
    env.insert("TERM".into(), "dumb".into());
    env.insert("TZ".into(), "UTC0".into());

    env.insert("HOMEBREW_PREFIX".into(), prefix.display().to_string());
    env.insert(
        "HOMEBREW_CELLAR".into(),
        prefix.join("Cellar").display().to_string(),
    );
    env.insert("HOMEBREW_TEMP".into(), temp.display().to_string());
    env.insert(
        "HOMEBREW_FORMULA_PREFIX".into(),
        plan.cellar_path.display().to_string(),
    );
    env.insert(
        "HOMEBREW_FORMULA_BUILDPATH".into(),
        buildpath.display().to_string(),
    );

    env.insert("NEXTBREW_PREFIX".into(), prefix.display().to_string());
    env.insert(
        "NEXTBREW_CELLAR".into(),
        prefix.join("Cellar").display().to_string(),
    );
    env.insert("NEXTBREW_FORMULA_NAME".into(), plan.formula_name.clone());
    env.insert("NEXTBREW_FORMULA_VERSION".into(), plan.version.clone());
    env.insert("NEXTBREW_LOGS".into(), logs.display().to_string());

    let jobs = num_cpus();
    env.insert("MAKEFLAGS".into(), format!("-j{jobs}"));
    env.insert("HOMEBREW_MAKE_JOBS".into(), jobs.to_string());

    #[cfg(target_os = "macos")]
    {
        let target = std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| {
            nb_core::macos_major_version()
                .map(|v| format!("{v}.0"))
                .unwrap_or_else(|| "15.0".to_string())
        });
        env.insert("MACOSX_DEPLOYMENT_TARGET".into(), target);
    }

    env
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nb_core::{BuildPlan, BuildSystem};
    use std::path::PathBuf;

    fn test_plan() -> BuildPlan {
        BuildPlan {
            formula_name: "test".to_string(),
            version: "1.0.0".to_string(),
            source_url: "https://example.com/test.tar.gz".to_string(),
            source_checksum: None,
            ruby_source_path: None,
            build_dependencies: Vec::new(),
            runtime_dependencies: Vec::new(),
            detected_system: BuildSystem::Autoconf,
            prefix: PathBuf::from("/opt/nextbrew/prefix"),
            cellar_path: PathBuf::from("/opt/nextbrew/cellar/test/1.0.0"),
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn build_env_includes_macosx_deployment_target() {
        let plan = test_plan();
        let env = build_env(
            &plan,
            &PathBuf::from("/opt/nextbrew/prefix"),
            Path::new("/tmp/build/src"),
            Path::new("/tmp/logs/test"),
        );
        assert!(env.contains_key("MACOSX_DEPLOYMENT_TARGET"));
        let target = &env["MACOSX_DEPLOYMENT_TARGET"];
        assert!(
            target.contains('.'),
            "expected version format like '15.0', got '{target}'"
        );
    }

    #[test]
    fn keeps_only_what_homebrew_keeps_of_the_users_environment() {
        let vars = [
            ("HOME", "/Users/me"),
            ("https_proxy", "http://proxy:3128"),
            ("HTTP_PROXY", "http://proxy:3128"),
            ("GOPATH", "/Users/me/go"),
            ("SSL_CERT_FILE", "/Users/me/ca.pem"),
            ("CFLAGS", "-O0"),
            ("HOMEBREW_VERBOSE", "1"),
            ("HOMEBREW_GITHUB_API_TOKEN", "secret"),
            ("NEXTBREW_AUTO_INIT", "true"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("USER", ""),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let mut kept: Vec<String> = kept_user_env(vars.into_iter()).into_keys().collect();
        kept.sort();
        assert_eq!(
            kept,
            ["HOMEBREW_VERBOSE", "NEXTBREW_AUTO_INIT", "https_proxy"]
        );
    }

    #[test]
    fn a_build_gets_a_home_of_its_own_and_the_prefix() {
        let env = build_env(
            &test_plan(),
            &PathBuf::from("/opt/nextbrew/prefix"),
            Path::new("/tmp/build/src"),
            Path::new("/tmp/logs/test"),
        );
        assert_eq!(env["HOME"], "/tmp/build/src/.brew_home");
        assert_eq!(
            env["PATH"],
            "/opt/nextbrew/prefix/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert_eq!(env["GOPATH"], "/opt/nextbrew/prefix/cache/go_mod_cache");
        assert_eq!(env["NEXTBREW_LOGS"], "/tmp/logs/test");
        assert_eq!(env["GIT_CONFIG_GLOBAL"], "/dev/null");
    }

    #[test]
    fn build_env_includes_standard_vars() {
        let plan = test_plan();
        let env = build_env(
            &plan,
            &PathBuf::from("/opt/nextbrew/prefix"),
            Path::new("/tmp/build/src"),
            Path::new("/tmp/logs/test"),
        );
        assert!(env.contains_key("NEXTBREW_PREFIX"));
        assert!(env.contains_key("NEXTBREW_FORMULA_NAME"));
        assert!(env.contains_key("MAKEFLAGS"));
    }
}
