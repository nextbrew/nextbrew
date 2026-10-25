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

/// A dependency of a build, as Homebrew's superenv sees it: one of the
/// formula's, or theirs at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDep {
    pub name: String,
    pub keg_only: bool,
    /// Needed at run time (`Build#install`'s `run_time_deps`), not only to
    /// build.
    pub run_time: bool,
    /// Its own dependencies, build ones too: `Formula#deps`, which
    /// `Formula["x"].python3` reads.
    pub dependencies: Vec<String>,
}

/// `Superenv#setup_build_environment`'s paths without its compiler
/// wrapper: the dependencies' tools first on `PATH`, only their
/// pkg-config files, keg-only ones' aclocal and CMake paths, and the
/// include and library paths the wrapper adds to each compiler call. Those
/// go in the variables the compiler itself reads, after the build's own
/// flags as the wrapper's do, where `configure` does not see them (it
/// turns an `-L` in `LDFLAGS` into an `-I` before the build's own).
/// Paths that do not exist are left out, as there.
fn superenv_paths(prefix: &Path, keg: &Path, deps: &[BuildDep]) -> Vec<(&'static str, String)> {
    let opt = |dep: &BuildDep| prefix.join("opt").join(&dep.name);
    let existing = |paths: Vec<PathBuf>| -> Vec<PathBuf> {
        let mut kept: Vec<PathBuf> = Vec::new();
        for path in paths {
            if path.is_dir() && !kept.contains(&path) {
                kept.push(path);
            }
        }
        kept
    };
    let join = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    };
    let keg_only: Vec<&BuildDep> = deps.iter().filter(|dep| dep.keg_only).collect();

    // `homebrew_extra_paths`: Pythons' unversioned commands, newest first.
    let mut pythons: Vec<&BuildDep> = deps
        .iter()
        .filter(|dep| dep.name == "python" || dep.name.starts_with("python@"))
        .collect();
    pythons.sort_by_key(|dep| {
        let version = dep.name.strip_prefix("python@").unwrap_or("0");
        std::cmp::Reverse(
            version
                .split('.')
                .map(|part| part.parse::<u32>().unwrap_or(0))
                .collect::<Vec<_>>(),
        )
    });
    let mut path: Vec<PathBuf> = deps.iter().map(|dep| opt(dep).join("bin")).collect();
    path.extend(pythons.iter().map(|dep| opt(dep).join("libexec/bin")));
    let mut path = join(&existing(path));
    if !path.is_empty() {
        path.push(':');
    }
    path.push_str("/usr/bin:/bin:/usr/sbin:/sbin");

    let pkg_config = existing(
        deps.iter()
            .map(|dep| opt(dep).join("lib/pkgconfig"))
            .chain(deps.iter().map(|dep| opt(dep).join("share/pkgconfig")))
            .collect(),
    );
    let aclocal = existing(
        keg_only
            .iter()
            .map(|dep| opt(dep).join("share/aclocal"))
            .chain([prefix.join("share/aclocal")])
            .collect(),
    );
    let cmake_prefix = existing(
        keg_only
            .iter()
            .map(|dep| opt(dep))
            .chain([prefix.to_path_buf()])
            .collect(),
    );
    let frameworks = existing(deps.iter().map(|dep| opt(dep).join("Frameworks")).collect());

    // The wrapper's `cppflags` (`-isystem` for the prefix, `-I` for
    // keg-only dependencies) and `ldflags`.
    let system_include = join(&existing(vec![prefix.join("include")]));
    let include = join(&existing(
        keg_only
            .iter()
            .map(|dep| opt(dep).join("include"))
            .collect(),
    ));
    let libraries = join(&existing(
        keg_only
            .iter()
            .filter(|dep| dep.name != "llvm" && !dep.name.starts_with("llvm@"))
            .map(|dep| opt(dep).join("lib"))
            .chain([prefix.join("lib")])
            .collect(),
    ));
    let ldflags = if cfg!(target_os = "macos") {
        "-Wl,-headerpad_max_install_names".to_string()
    } else {
        // The Linux wrapper's `-Wl,-rpath=` for `determine_rpath_paths`
        // (the keg's and gcc's even before they exist), and the brewed
        // glibc's loader if there is one.
        let mut rpath = vec![keg.join("lib"), prefix.join("opt/gcc/lib/gcc/current")];
        rpath.extend(existing(
            deps.iter()
                .filter(|dep| dep.run_time)
                .map(|dep| opt(dep).join("lib"))
                .collect(),
        ));
        rpath.push(prefix.join("lib"));
        let mut flags: Vec<String> = Vec::new();
        for path in rpath {
            let flag = format!("-Wl,-rpath={}", path.display());
            if !flags.contains(&flag) {
                flags.push(flag);
            }
        }
        let loader = prefix.join("lib/ld.so");
        if std::fs::File::open(&loader).is_ok() {
            flags.push(format!("-Wl,--dynamic-linker={}", loader.display()));
        }
        flags.join(" ")
    };
    // `HOMEBREW_OPTIMIZATION_LEVEL`, which the wrapper puts in place of the
    // build's own `-O` and `-g`.
    let optimization = if cfg!(target_os = "macos") {
        "-Os"
    } else {
        "-O2"
    };

    // `determine_pkg_config_libdir`: none of pkg-config's own, but on
    // macOS the system's and Homebrew's files for what macOS provides.
    let pkg_config_libdir = if cfg!(target_os = "macos") {
        let version = nb_core::macos_major_version()
            .map(|v| v.to_string())
            .unwrap_or_default();
        join(&existing(vec![
            PathBuf::from("/usr/lib/pkgconfig"),
            prefix
                .join("Library/Homebrew/os/mac/pkgconfig")
                .join(version),
        ]))
    } else {
        String::new()
    };

    vec![
        ("PATH", path),
        ("PKG_CONFIG_PATH", join(&pkg_config)),
        ("PKG_CONFIG_LIBDIR", pkg_config_libdir),
        ("ACLOCAL_PATH", join(&aclocal)),
        ("CMAKE_PREFIX_PATH", join(&cmake_prefix)),
        ("CMAKE_FRAMEWORK_PATH", join(&frameworks)),
        ("CPATH", include),
        ("C_INCLUDE_PATH", system_include.clone()),
        ("CPLUS_INCLUDE_PATH", system_include.clone()),
        ("OBJC_INCLUDE_PATH", system_include),
        ("LIBRARY_PATH", libraries),
        // cargo links with `cc` but not `LDFLAGS`, which Homebrew's
        // wrapper would add to it anyway.
        ("RUSTFLAGS", rustflags(&ldflags)),
        ("LDFLAGS", ldflags),
        ("CFLAGS", optimization.into()),
        ("CXXFLAGS", optimization.into()),
    ]
}

fn rustflags(ldflags: &str) -> String {
    if cfg!(target_os = "macos") {
        return String::new();
    }
    ldflags
        .split_whitespace()
        .map(|flag| format!("-C link-arg={flag}"))
        .collect::<Vec<_>>()
        .join(" ")
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
    deps: &[BuildDep],
) -> HashMap<String, String> {
    let mut env = kept_user_env(std::env::vars());

    #[cfg(target_os = "macos")]
    super::mac_pkgconfig::install(prefix);
    for (key, value) in superenv_paths(prefix, &plan.cellar_path, deps) {
        env.insert(key.into(), value);
    }
    // Linux's superenv: the brewed m4, for libtool's and bison's tools.
    if cfg!(target_os = "linux")
        && deps
            .iter()
            .any(|dep| dep.name == "libtool" || dep.name == "bison")
    {
        env.insert(
            "M4".into(),
            prefix.join("opt/m4/bin/m4").display().to_string(),
        );
    }
    #[cfg(target_os = "macos")]
    {
        // The compilers and SDK as superenv's `xcrun` shim finds them, but
        // found once: called by name, each of a `configure`'s hundreds of
        // checks would ask `xcrun` again.
        let toolchain = macos_toolchain();
        env.insert("CC".into(), toolchain.cc.clone());
        env.insert("CXX".into(), toolchain.cxx.clone());
        if let Some(sdk) = &toolchain.sdk {
            env.insert("SDKROOT".into(), sdk.clone());
        }
        // `configure` runs itself again under `CONFIG_SHELL`, and its
        // checks start many shells: macOS's dash starts them faster than
        // its bash does.
        if Path::new("/bin/dash").exists() {
            env.insert("CONFIG_SHELL".into(), "/bin/dash".into());
        }
    }

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
    env.insert(
        "NEXTBREW_FORMULA_STABLE_VERSION".into(),
        plan.stable_version.clone(),
    );
    env.insert("NEXTBREW_FORMULA_TAP".into(), plan.tap.clone());
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

#[cfg(target_os = "macos")]
struct Toolchain {
    cc: String,
    cxx: String,
    sdk: Option<String>,
}

/// `xcrun --find` for the compilers and `xcrun --show-sdk-path`, asked
/// once; their names when `xcrun` cannot say.
#[cfg(target_os = "macos")]
fn macos_toolchain() -> &'static Toolchain {
    static TOOLCHAIN: std::sync::OnceLock<Toolchain> = std::sync::OnceLock::new();
    TOOLCHAIN.get_or_init(|| {
        let xcrun = |args: &[&str]| {
            std::process::Command::new("/usr/bin/xcrun")
                .args(args)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|path| !path.is_empty() && Path::new(path).exists())
        };
        Toolchain {
            cc: xcrun(&["--find", "clang"]).unwrap_or_else(|| "clang".into()),
            cxx: xcrun(&["--find", "clang++"]).unwrap_or_else(|| "clang++".into()),
            sdk: xcrun(&["--show-sdk-path"]),
        }
    })
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
            stable_version: "1.0.0".to_string(),
            tap: "homebrew/core".to_string(),
            source_git: None,
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
            &[],
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
            &[],
        );
        assert_eq!(env["HOME"], "/tmp/build/src/.brew_home");
        assert_eq!(env["PATH"], "/usr/bin:/bin:/usr/sbin:/sbin");
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
            &[],
        );
        assert!(env.contains_key("NEXTBREW_PREFIX"));
        assert!(env.contains_key("NEXTBREW_FORMULA_NAME"));
        assert!(env.contains_key("MAKEFLAGS"));
    }

    #[test]
    fn dependencies_are_reached_as_superenv_reaches_them() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path();
        for dir in [
            "include",
            "lib",
            "opt/ncurses/bin",
            "opt/ncurses/include",
            "opt/ncurses/lib/pkgconfig",
            "opt/ncurses/share/aclocal",
            "opt/xz/bin",
            "opt/xz/include",
            "opt/xz/lib/pkgconfig",
            "opt/python@3.13/libexec/bin",
            "opt/python@3.14/libexec/bin",
        ] {
            std::fs::create_dir_all(prefix.join(dir)).unwrap();
        }
        let dep = |name: &str, keg_only, run_time| BuildDep {
            name: name.into(),
            keg_only,
            run_time,
            dependencies: Vec::new(),
        };
        let deps = [
            dep("ncurses", true, false),
            dep("xz", false, true),
            dep("python@3.13", false, true),
            dep("python@3.14", false, false),
        ];
        let keg = prefix.join("Cellar/f/1");
        let env: HashMap<_, _> = superenv_paths(prefix, &keg, &deps).into_iter().collect();
        let p = |sub: &str| prefix.join(sub).display().to_string();
        assert_eq!(
            env["PATH"],
            format!(
                "{}:{}:{}:{}:/usr/bin:/bin:/usr/sbin:/sbin",
                p("opt/ncurses/bin"),
                p("opt/xz/bin"),
                p("opt/python@3.14/libexec/bin"),
                p("opt/python@3.13/libexec/bin"),
            )
        );
        assert_eq!(
            env["PKG_CONFIG_PATH"],
            format!(
                "{}:{}",
                p("opt/ncurses/lib/pkgconfig"),
                p("opt/xz/lib/pkgconfig")
            )
        );
        if !cfg!(target_os = "macos") {
            assert_eq!(env["PKG_CONFIG_LIBDIR"], "");
        }
        assert_eq!(env["ACLOCAL_PATH"], p("opt/ncurses/share/aclocal"));
        assert_eq!(
            env["CMAKE_PREFIX_PATH"],
            format!("{}:{}", p("opt/ncurses"), prefix.display())
        );
        // Only keg-only dependencies need their own paths.
        assert_eq!(env["CPATH"], p("opt/ncurses/include"));
        assert_eq!(env["C_INCLUDE_PATH"], p("include"));
        assert_eq!(
            env["LIBRARY_PATH"],
            format!("{}:{}", p("opt/ncurses/lib"), p("lib"))
        );
        if cfg!(target_os = "macos") {
            assert_eq!(env["CFLAGS"], "-Os");
        } else {
            assert_eq!(env["CFLAGS"], "-O2");
            // Only run-time dependencies' libraries that exist.
            assert_eq!(
                env["LDFLAGS"],
                format!(
                    "-Wl,-rpath={} -Wl,-rpath={} -Wl,-rpath={} -Wl,-rpath={}",
                    p("Cellar/f/1/lib"),
                    p("opt/gcc/lib/gcc/current"),
                    p("opt/xz/lib"),
                    p("lib")
                )
            );
            assert_eq!(
                env["RUSTFLAGS"],
                format!(
                    "-C link-arg=-Wl,-rpath={} -C link-arg=-Wl,-rpath={} -C link-arg=-Wl,-rpath={} -C link-arg=-Wl,-rpath={}",
                    p("Cellar/f/1/lib"),
                    p("opt/gcc/lib/gcc/current"),
                    p("opt/xz/lib"),
                    p("lib")
                )
            );
        }
    }
}
