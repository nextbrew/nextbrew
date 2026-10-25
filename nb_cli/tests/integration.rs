use std::path::PathBuf;
use std::process::{Command, Output};

struct TestEnv {
    root: tempfile::TempDir,
    /// On macOS, Mach-O binary patching requires the prefix path to be no longer
    /// than the original Homebrew prefix (`/opt/homebrew` = 13 chars). The default
    /// OS temp directory on macOS (`/var/folders/…`) produces paths far too long,
    /// so we create a separate short temp dir in `/tmp` for the prefix.
    prefix_dir: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            root: tempfile::TempDir::new().expect("failed to create temp dir"),
            prefix_dir: tempfile::Builder::new()
                .prefix("nb")
                .rand_bytes(3)
                .tempdir_in("/tmp")
                .expect("failed to create short prefix temp dir"),
        }
    }

    fn prefix(&self) -> PathBuf {
        self.prefix_dir.path().to_path_buf()
    }

    fn nb(&self, args: &[&str]) -> Output {
        let nb = env!("CARGO_BIN_EXE_nb");
        Command::new(nb)
            .env("NEXTBREW_ROOT", self.root.path())
            // Use the short prefix so Mach-O patching stays within the 13-char limit,
            // and prevent a host-level NEXTBREW_PREFIX from leaking into the test.
            .env("NEXTBREW_PREFIX", self.prefix())
            .env("NEXTBREW_AUTO_INIT", "true")
            // Auto-init edits shell rc files; keep it off the real ~/.zshrc.
            .env("HOME", self.root.path())
            .args(args)
            .output()
            .unwrap_or_else(|_| panic!("failed to execute {nb} command"))
    }

    fn bin_dir(&self) -> PathBuf {
        self.prefix().join("bin")
    }

    fn count_store_entries(&self) -> usize {
        assert!(self.root.path().join("store").is_dir());
        std::fs::read_dir(self.root.path().join("store"))
            .map(|r| r.count())
            .expect("failed to read store directory")
    }

    fn run_binary(&self, name: &str, args: &[&str]) -> Output {
        let bin_path = self.bin_dir().join(name);
        Command::new(&bin_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin_dir().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute {}: {e}", bin_path.display()))
    }

    /// Find a binary inside the cellar (for keg-only formulas that aren't linked).
    fn cellar_binary(&self, formula: &str, binary: &str) -> PathBuf {
        let cellar = self.prefix().join("Cellar").join(formula);
        let versions: Vec<_> = std::fs::read_dir(&cellar)
            .unwrap_or_else(|e| panic!("no cellar entry for {formula}: {e}"))
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !versions.is_empty(),
            "no versions found in cellar for {formula}"
        );
        versions[0].path().join("bin").join(binary)
    }

    fn run_cellar_binary(&self, formula: &str, binary: &str, args: &[&str]) -> Output {
        let bin_path = self.cellar_binary(formula, binary);
        Command::new(&bin_path)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin_path.parent().unwrap().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("failed to execute {}: {e}", bin_path.display()))
    }
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{} failed:\nstdout: {}\nstderr: {}",
        context,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_stdout_contains(output: &Output, needle: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(needle),
        "expected stdout to contain {needle:?}, got: {stdout}"
    );
}

fn assert_no_installed_symlinks(dir: &std::path::Path) {
    if !dir.exists() {
        return;
    }
    let cellar = dir.join("Cellar");
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.expect("failed to read directory entry");
        if entry.path().starts_with(&cellar) {
            continue;
        }
        assert!(
            !entry.path_is_symlink(),
            "unexpected symlink: {}",
            entry.path().display()
        );
    }
}

#[test]
#[ignore = "integration test"]
#[cfg(target_os = "macos")] // GitHub Actions linux runner needs additional X11/XCB deps
fn test_ffmpeg_formula() {
    let t = TestEnv::new();

    assert_success(&t.nb(&["install", "ffmpeg"]), "nb install ffmpeg");

    // From the upstream formula test:
    // https://github.com/Homebrew/homebrew-core/blob/3076627c980d101ff02a720060c508433c44f293/Formula/f/ffmpeg.rb#L114
    let mp4out = t.root.path().join("video.mp4");
    assert_success(
        &t.run_binary(
            "ffmpeg",
            &[
                "-filter_complex",
                "testsrc=rate=1:duration=5",
                mp4out.to_str().unwrap(),
            ],
        ),
        "ffmpeg create test video",
    );
    assert!(mp4out.exists());
}

#[test]
#[ignore = "integration test"]
fn test_curl_keg_only() {
    let t = TestEnv::new();

    assert_success(&t.nb(&["install", "curl"]), "nb install curl");

    // curl is keg-only with reason :provided_by_macos.
    // On macOS the reason applies, so it should NOT be linked.
    // On Linux the reason is irrelevant, so it SHOULD be linked.
    if cfg!(target_os = "macos") {
        assert!(
            !t.bin_dir().join("curl").exists(),
            "curl should not be linked (keg-only on macOS)"
        );
    } else {
        assert!(
            t.bin_dir().join("curl").exists(),
            "curl should be linked on Linux (provided_by_macos is not applicable)"
        );
    }

    // the binary should exist in the cellar and work either way
    let output = if t.bin_dir().join("curl").exists() {
        t.run_binary("curl", &["https://www.githubstatus.com"])
    } else {
        t.run_cellar_binary("curl", "curl", &["https://www.githubstatus.com"])
    };
    assert_success(&output, "curl https://www.githubstatus.com");
    assert_stdout_contains(&output, "GitHub");
}

#[test]
#[ignore = "integration test"]
fn test_install_uninstall_and_reinstall() {
    let t = TestEnv::new();

    assert_success(&t.nb(&["install", "jq"]), "nb install jq");

    let test_json = t.root.path().join("test.json");
    std::fs::write(&test_json, r#"{"foo":1, "bar":2}"#).expect("failed to write test.json");

    let output = t.run_binary("jq", &[".bar", test_json.to_str().unwrap()]);
    assert_success(&output, "jq .bar test.json");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "2\n");

    assert_success(&t.nb(&["uninstall", "jq"]), "nb uninstall jq");
    assert!(!t.bin_dir().join("jq").exists());
    assert_no_installed_symlinks(&t.prefix());

    assert_success(&t.nb(&["install", "jq"]), "nb install jq (reinstall)");
    assert_success(
        &t.run_binary("jq", &["--version"]),
        "jq --version after reinstall",
    );
}

#[test]
#[ignore = "integration test"]
fn test_list_installed_formulas() {
    let t = TestEnv::new();

    let output = t.nb(&["list"]);
    assert_success(&output, "nb list (empty)");
    assert_stdout_contains(&output, "No formulas installed");

    assert_success(&t.nb(&["install", "jq"]), "nb install jq");

    let output = t.nb(&["list"]);
    assert_success(&output, "nb list");
    assert_stdout_contains(&output, "jq");

    assert_success(&t.nb(&["uninstall", "jq"]), "nb uninstall jq");

    let output = t.nb(&["list"]);
    assert_success(&output, "nb list (empty)");
    assert_stdout_contains(&output, "No formulas installed");
}

#[test]
#[ignore = "integration test"]
fn test_info_finds_installed_formula() {
    let t = TestEnv::new();

    let output = t.nb(&["info", "jq"]);
    assert_success(&output, "nb info jq (not installed)");
    assert_stdout_contains(&output, "Not installed");

    assert_success(&t.nb(&["install", "jq"]), "nb install jq");

    let output = t.nb(&["info", "jq"]);
    assert_success(&output, "nb info jq");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("==> Installed Versions") && !stdout.contains("Not installed"),
        "stdout: {stdout}"
    );
}

#[test]
#[ignore = "integration test"]
fn test_gc_removes_unused_store_entries() {
    let t = TestEnv::new();

    assert_success(&t.nb(&["gc"]), "nb gc (empty)");
    assert_eq!(t.count_store_entries(), 0);

    assert_success(&t.nb(&["install", "jq"]), "nb install jq");
    let entries_before = t.count_store_entries();
    assert!(entries_before > 0);

    assert_success(&t.nb(&["uninstall", "jq"]), "nb uninstall jq");
    assert_eq!(t.count_store_entries(), entries_before);

    assert_success(&t.nb(&["gc"]), "nb gc");
    assert_eq!(t.count_store_entries(), 0);
}

#[test]
#[ignore = "integration test"]
fn test_build_from_source() {
    let t = TestEnv::new();

    assert_success(
        &t.nb(&["install", "--build-from-source", "jq"]),
        "nb install --build-from-source jq",
    );

    // Runs from the keg with oniguruma from the prefix, which the build's
    // RPATH (Linux) or install names (macOS) must find.
    let output = t.run_binary("jq", &["-n", "[1,2] | map(. + 1) | add"]);
    assert_success(&output, "jq built from source");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "5\n");
}

#[test]
#[ignore = "integration test"]
fn test_formula_commands() {
    let t = TestEnv::new();
    assert_success(&t.nb(&["install", "jq"]), "nb install jq");

    let output = t.nb(&["deps", "jq"]);
    assert_success(&output, "nb deps jq");
    assert_stdout_contains(&output, "oniguruma");

    let output = t.nb(&["uses", "--installed", "oniguruma"]);
    assert_success(&output, "nb uses --installed oniguruma");
    assert_stdout_contains(&output, "jq");

    let output = t.nb(&["search", "/^jq$/"]);
    assert_success(&output, "nb search /^jq$/");
    assert_stdout_contains(&output, "jq");

    assert_success(&t.nb(&["unlink", "jq"]), "nb unlink jq");
    assert!(!t.bin_dir().join("jq").exists());
    assert_success(&t.nb(&["link", "jq"]), "nb link jq");
    assert_success(&t.run_binary("jq", &["--version"]), "jq after relink");

    assert_success(&t.nb(&["pin", "jq"]), "nb pin jq");
    let output = t.nb(&["list", "--pinned"]);
    assert_success(&output, "nb list --pinned");
    assert_stdout_contains(&output, "jq");
    // Homebrew's `onoe`: refused with an error, yet not a failure.
    t.nb(&["uninstall", "jq"]);
    assert!(t.bin_dir().join("jq").exists(), "pinned jq uninstalled");
    assert_success(&t.nb(&["unpin", "jq"]), "nb unpin jq");

    assert_success(&t.nb(&["uninstall", "jq"]), "nb uninstall jq");
    let output = t.nb(&["autoremove"]);
    assert_success(&output, "nb autoremove");
    let output = t.nb(&["list", "-1"]);
    assert_success(&output, "nb list after autoremove");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("oniguruma"),
        "oniguruma left after autoremove"
    );
    assert_no_installed_symlinks(&t.prefix());
}

#[test]
#[ignore = "integration test"]
fn test_bundle() {
    let t = TestEnv::new();
    let brewfile = t.root.path().join("Brewfile");
    std::fs::write(&brewfile, "brew \"jq\"\n").expect("failed to write Brewfile");
    let file = brewfile.to_str().unwrap();

    assert_success(
        &t.nb(&["bundle", "install", "--file", file]),
        "nb bundle install",
    );
    assert_success(&t.run_binary("jq", &["--version"]), "jq from the Brewfile");
    assert_success(
        &t.nb(&["bundle", "check", "--file", file]),
        "nb bundle check",
    );
    let output = t.nb(&["bundle", "list", "--file", file]);
    assert_success(&output, "nb bundle list");
    assert_stdout_contains(&output, "jq");

    assert_success(&t.nb(&["install", "tree"]), "nb install tree");
    assert_success(
        &t.nb(&["bundle", "cleanup", "--file", file, "--force"]),
        "nb bundle cleanup",
    );
    assert!(
        !t.bin_dir().join("tree").exists(),
        "tree left after cleanup"
    );
    assert!(t.bin_dir().join("jq").exists(), "jq removed by cleanup");

    let dump = t.root.path().join("Dumped");
    assert_success(
        &t.nb(&["bundle", "dump", "--file", dump.to_str().unwrap()]),
        "nb bundle dump",
    );
    let dumped = std::fs::read_to_string(&dump).expect("failed to read dump");
    assert!(dumped.contains("brew \"jq\""), "dump: {dumped}");
}

/// One cask per artifact and container: binaries from a zip (ngrok) and a
/// tarball (rq), apps from a dmg (rectangle) and a zip (keycastr), and a
/// suite moved into place by preflight steps (quakespasm).
#[cfg(target_os = "macos")]
#[test]
#[ignore = "integration test"]
fn test_casks() {
    let t = TestEnv::new();
    let appdir = t.root.path().join("Applications");
    let casks = ["ngrok", "rq", "rectangle", "keycastr", "quakespasm"];
    let mut args = vec!["install", "--cask", "--appdir", appdir.to_str().unwrap()];
    args.extend(casks);
    assert_success(&t.nb(&args), "nb install --cask");

    assert_success(&t.run_binary("ngrok", &["--version"]), "ngrok");
    assert_success(&t.run_binary("rq", &["--help"]), "rq");
    for placed in ["Rectangle.app", "KeyCastr.app", "QuakeSpasm"] {
        assert!(appdir.join(placed).is_dir(), "{placed} not placed");
    }
    let output = t.nb(&["list", "--cask"]);
    assert_success(&output, "nb list --cask");
    for cask in casks {
        assert_stdout_contains(&output, cask);
    }

    let mut args = vec!["uninstall", "--cask", "--zap"];
    args.extend(casks);
    assert_success(&t.nb(&args), "nb uninstall --cask --zap");
    assert!(!t.bin_dir().join("ngrok").exists() && !t.bin_dir().join("rq").exists());
    assert_eq!(
        std::fs::read_dir(&appdir).map(|d| d.count()).unwrap_or(0),
        0,
        "apps left in the appdir"
    );
    assert_no_installed_symlinks(&t.prefix());
}

/// A pkg installs system-wide through `sudo installer`, so only on CI.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "integration test"]
fn test_pkg_cask() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let t = TestEnv::new();
    let receipt = || {
        Command::new("pkgutil")
            .args(["--pkg-info", "com.scriptingosx.desktoppr"])
            .output()
            .expect("failed to run pkgutil")
            .status
            .success()
    };
    assert_success(
        &t.nb(&["install", "--cask", "desktoppr"]),
        "nb install desktoppr",
    );
    assert!(receipt(), "desktoppr's pkg not installed");
    assert_success(
        &t.nb(&["uninstall", "--cask", "desktoppr"]),
        "nb uninstall desktoppr",
    );
    assert!(!receipt(), "desktoppr's pkg left installed");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "integration test"]
fn test_services() {
    let t = TestEnv::new();
    assert_success(&t.nb(&["install", "memcached"]), "nb install memcached");
    assert_success(
        &t.nb(&["services", "start", "memcached"]),
        "nb services start",
    );
    let output = t.nb(&["services", "list"]);
    assert_success(&output, "nb services list");
    assert_stdout_contains(&output, "memcached");
    assert_stdout_contains(&output, "started");
    assert_success(
        &t.nb(&["services", "stop", "memcached"]),
        "nb services stop",
    );
    let output = t.nb(&["services", "list"]);
    assert_success(&output, "nb services list after stop");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("started"),
        "memcached still started"
    );
}

/// systemd only finds user units under the real home, so only on CI,
/// where the workflow starts a user manager for the runner.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "integration test"]
fn test_services() {
    if std::env::var_os("CI").is_none() {
        return;
    }
    let t = TestEnv::new();
    let home = std::env::var("HOME").expect("HOME");
    let nb = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_nb"))
            .env("NEXTBREW_ROOT", t.root.path())
            .env("NEXTBREW_PREFIX", t.prefix())
            .env("NEXTBREW_AUTO_INIT", "true")
            .env("HOME", &home)
            .args(args)
            .output()
            .expect("failed to run nb")
    };
    assert_success(&nb(&["install", "memcached"]), "nb install memcached");
    assert_success(
        &nb(&["services", "start", "memcached"]),
        "nb services start",
    );
    let output = nb(&["services", "list"]);
    assert_success(&output, "nb services list");
    assert_stdout_contains(&output, "memcached");
    assert_stdout_contains(&output, "started");
    assert_success(&nb(&["services", "stop", "memcached"]), "nb services stop");
    let output = nb(&["services", "list"]);
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("started"),
        "memcached still started"
    );
}
