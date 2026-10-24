//! A cache for the CA bundle ca-certificates builds in post-install.
//!
//! The formula's step runs `libexec/post-install SOURCE DESTINATION`, a
//! shell script that exports the system keychain, checks every certificate
//! with `openssl` and `security` one process at a time and merges in the
//! Mozilla bundle. That takes about five seconds, and its output depends
//! only on the Mozilla bundle it is given and the system trust store. The
//! result is kept keyed on both, so reinstalling ca-certificates, or
//! anything that depends on it, copies the bundle instead of rebuilding
//! it.
//!
//! On a miss, a script nextbrew knows (by its SHA-256, with the list of
//! public keys it excludes taken out, so a new list is still known) is
//! followed step by step: what it asks `openssl` is read from each certificate as LibreSSL
//! would answer, `security verify-cert` still decides trust, the checks run
//! concurrently and their results are taken in the script's order, so the
//! bundle is the script's own bytes in a fraction of the time. Any other
//! script, or anything unexpected, runs the script unchanged.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use super::steps::write_atomically;
use super::{PostInstallContext, StepError};

/// The system trust stores the script reads, in the order it reads them.
#[cfg(target_os = "macos")]
const TRUST_SOURCES: &[&str] = &[
    "/Library/Keychains/System.keychain",
    "/System/Library/Keychains/SystemRootCertificates.keychain",
];

#[cfg(not(target_os = "macos"))]
const TRUST_SOURCES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/ca-bundle.pem",
];

/// SHA-256 of the `libexec/post-install` scripts whose macOS branch
/// [`CaBundle::build_in_parallel`] follows, after [`excluded_keys`] puts
/// [`EXCLUDED`] in place of the excluded public keys.
const KNOWN_SCRIPTS: &[&str] = &[
    // ca-certificates 2026-09-25
    "0d382c7231f5f0378947729c5815f7fb8f6c12c16396172513109b13bd59b900",
    // ca-certificates 2026-09-25_1 and 2026-10-20
    "043a5039c58b98710b07840982ee04c3abe9732c15ce66cba863b47d897eedeb",
];

/// What the formula writes into the script in place of the excluded keys.
const EXCLUDED: &str = "@EXCLUDED_SPKI_HASHES@";

pub(super) struct CaBundle {
    source: PathBuf,
    destination: PathBuf,
    cached: PathBuf,
}

impl CaBundle {
    /// Recognise ca-certificates' bundle step and work out its cache key.
    /// `None` for any other command.
    pub(super) fn detect(
        ctx: &PostInstallContext,
        program: &OsStr,
        args: &[String],
    ) -> Option<Self> {
        if ctx.name != "ca-certificates" || args.len() != 2 {
            return None;
        }
        if Path::new(program).file_name() != Some(OsStr::new("post-install")) {
            return None;
        }
        let mozilla = fs::read(&args[0]).ok()?;

        let mut hasher = Sha256::new();
        hasher.update(&mozilla);
        // A new script (say, more excluded keys) builds a new bundle.
        hasher.update(fs::read(program).unwrap_or_default());
        for source in TRUST_SOURCES {
            hasher.update(source.as_bytes());
            match fs::metadata(source) {
                Ok(metadata) => {
                    hasher.update(metadata.len().to_le_bytes());
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|age| age.as_nanos())
                        .unwrap_or(0);
                    hasher.update(modified.to_le_bytes());
                }
                Err(_) => hasher.update(b"missing"),
            }
        }
        #[cfg(target_os = "macos")]
        hasher.update(nb_core::macos_major_version().unwrap_or(0).to_le_bytes());
        let key = hex(&hasher.finalize());

        Some(Self {
            source: PathBuf::from(&args[0]),
            destination: PathBuf::from(&args[1]),
            cached: ctx.cache.join("ca-certificates").join(format!("{key}.pem")),
        })
    }

    /// Put the cached bundle in place. `false` when there is none.
    pub(super) fn restore(&self) -> Result<bool, StepError> {
        let Ok(content) = fs::read(&self.cached) else {
            return Ok(false);
        };
        if let Some(parent) = self.destination.parent() {
            fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
        }
        write_atomically(&self.destination, &content)?;
        fs::set_permissions(&self.destination, fs::Permissions::from_mode(0o644))
            .map_err(StepError::io("setting bundle mode"))?;
        Ok(true)
    }

    /// Build the bundle as the known `script` would, its per-certificate
    /// checks run concurrently. `false` when the script is not a known one or
    /// something unexpected happens; the caller then runs the script.
    pub(super) fn build_in_parallel(&self, script: &Path) -> Result<bool, StepError> {
        let Some(excluded) = known_script(script) else {
            return Ok(false);
        };
        let Some(bundle) = build(&self.source, &excluded) else {
            return Ok(false);
        };
        if let Some(parent) = self.destination.parent() {
            fs::create_dir_all(parent).map_err(StepError::io("creating directory"))?;
        }
        write_atomically(&self.destination, &bundle)?;
        fs::set_permissions(&self.destination, fs::Permissions::from_mode(0o644))
            .map_err(StepError::io("setting bundle mode"))?;
        Ok(true)
    }

    /// Build the bundle into the cache only, ahead of the step, for a known
    /// script; the step then restores it. Done while the formula's
    /// dependencies are still downloading, so its time is hidden. Failures
    /// are left for the step to meet.
    pub(super) fn prebuild(&self, script: &Path) {
        if self.cached.exists() {
            return;
        }
        let Some(excluded) = known_script(script) else {
            return;
        };
        if let Some(bundle) = build(&self.source, &excluded) {
            if let Some(parent) = self.cached.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = write_atomically(&self.cached, &bundle);
        }
    }

    /// Remember the bundle the script just wrote.
    pub(super) fn store(&self) -> Result<(), StepError> {
        let Ok(content) = fs::read(&self.destination) else {
            return Ok(());
        };
        if let Some(parent) = self.cached.parent() {
            fs::create_dir_all(parent).map_err(StepError::io("creating cache directory"))?;
        }
        write_atomically(&self.cached, &content)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The public keys a known script excludes; `None` for any other script.
fn known_script(script: &Path) -> Option<Vec<String>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let (template, excluded) = excluded_keys(&fs::read_to_string(script).ok()?);
    KNOWN_SCRIPTS
        .contains(&hex(&Sha256::digest(template)).as_str())
        .then_some(excluded)
}

/// The script with its `case` line of excluded key hashes
/// (`HASH|HASH) return 0 ;;`) turned back into the formula's [`EXCLUDED`]
/// placeholder, and those hashes.
fn excluded_keys(script: &str) -> (String, Vec<String>) {
    let mut excluded = Vec::new();
    let template = script
        .split_inclusive('\n')
        .map(|line| {
            let body = line.trim_start();
            let hashes = body
                .trim_end()
                .strip_suffix(") return 0 ;;")
                .map(|list| list.split('|').collect::<Vec<_>>())
                .filter(|list| {
                    list.iter().all(|hash| {
                        hash.len() == 64
                            && hash
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
                });
            match hashes {
                Some(list) => {
                    excluded.extend(list.iter().map(|hash| hash.to_string()));
                    let indent = &line[..line.len() - body.len()];
                    let end = &body[body.trim_end().len()..];
                    format!("{indent}{EXCLUDED}) return 0 ;;{end}")
                }
                None => line.to_string(),
            }
        })
        .collect();
    (template, excluded)
}

/// The script's macOS branch: trusted, unexpired server CAs from the two
/// system keychains, then the Mozilla bundle, each certificate once.
#[cfg(target_os = "macos")]
fn build(source: &Path, excluded: &[String]) -> Option<Vec<u8>> {
    use rayon::prelude::*;
    use std::collections::HashSet;

    let work = tempfile::tempdir().ok()?;
    let now = chrono::Utc::now();
    let mut seen = HashSet::new();
    let mut bundle = Vec::new();
    let mut append = |certificates: &[Vec<u8>], facts: Vec<Option<super::x509::Facts>>| {
        for (certificate, facts) in certificates.iter().zip(facts) {
            if let Some(facts) = facts
                && !excluded.contains(&facts.spki_sha256)
                && seen.insert(facts.fingerprint)
            {
                bundle.extend_from_slice(certificate);
                bundle.push(b'\n');
            }
        }
    };

    for (keychain, purpose) in [
        ("/Library/Keychains/System.keychain", "ssl"),
        (
            "/System/Library/Keychains/SystemRootCertificates.keychain",
            "basic",
        ),
    ] {
        // The script stops (`set -e`) when a keychain cannot be read; let it.
        let pem = std::process::Command::new("/usr/bin/security")
            .args(["find-certificate", "-a", "-p", keychain])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        let certificates = split_certificates(&pem.stdout);
        let facts = certificates
            .par_iter()
            .enumerate()
            .map(|(i, certificate)| {
                let file = work.path().join(format!("{purpose}-{i}.pem"));
                trusted(certificate, purpose, &file, now)
            })
            .collect();
        append(&certificates, facts);
    }

    let certificates = split_certificates(&fs::read(source).ok()?);
    let facts = certificates
        .par_iter()
        .map(|c| super::x509::inspect(c, now))
        .collect();
    append(&certificates, facts);
    Some(bundle)
}

#[cfg(not(target_os = "macos"))]
fn build(_source: &Path, _excluded: &[String]) -> Option<Vec<u8>> {
    None
}

/// `append_keychain`'s checks, then what is known of the certificate. What the script asks
/// `openssl x509` (expiry, purpose, fingerprint) is read from the
/// certificate here, the way LibreSSL answers it (see [`super::x509`]);
/// the trust decision is still `security verify-cert`'s.
#[cfg(target_os = "macos")]
fn trusted(
    certificate: &[u8],
    purpose: &str,
    file: &Path,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<super::x509::Facts> {
    let facts = super::x509::inspect(certificate, now)?;
    if !facts.unexpired || !facts.ssl_server_ca {
        return None;
    }
    fs::write(file, certificate).ok()?;
    std::process::Command::new("/usr/bin/security")
        .args(["verify-cert", "-l", "-L", "-c"])
        .arg(file)
        .args(["-p", purpose, "-R", "offline"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|status| status.success())?;
    Some(facts)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The certificates of a PEM bundle in the order the script reads them back:
/// its awk writes them to `1.pem`, `2.pem`, … and the `*.pem` glob returns
/// those names sorted as strings (`1`, `10`, `100`, `101`, …, `2`, …).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn split_certificates(pem: &[u8]) -> Vec<Vec<u8>> {
    let mut numbered: Vec<(String, Vec<u8>)> = read_certificates(pem)
        .into_iter()
        .enumerate()
        .map(|(i, certificate)| (format!("{}.pem", i + 1), certificate))
        .collect();
    numbered.sort_by(|(a, _), (b, _)| a.cmp(b));
    numbered
        .into_iter()
        .map(|(_, certificate)| certificate)
        .collect()
}

/// The script's `split_certificates` awk: every line from a BEGIN marker
/// through the next END marker, newline-terminated; a new BEGIN starts over.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) fn read_certificates(pem: &[u8]) -> Vec<Vec<u8>> {
    let pem = pem.strip_suffix(b"\n").unwrap_or(pem);
    let mut certificates = Vec::new();
    let mut certificate = Vec::new();
    let mut writing = false;
    for line in pem.split(|byte| *byte == b'\n') {
        if contains(line, b"-----BEGIN CERTIFICATE-----") {
            certificate.clear();
            writing = true;
        }
        if writing {
            certificate.extend_from_slice(line);
            certificate.push(b'\n');
        }
        if writing && contains(line, b"-----END CERTIFICATE-----") {
            certificates.push(std::mem::take(&mut certificate));
            writing = false;
        }
    }
    certificates
}

#[cfg(test)]
mod tests {
    use super::super::steps::run_step;
    use super::super::test_support::context;
    use super::*;
    use nb_core::{PathBase, PathSpec, StepAction, Sudo};
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// A stand-in for the real script: counts its runs and writes the
    /// source bundle to the destination with a marker.
    fn install_script(ctx: &PostInstallContext) -> PathBuf {
        let script = ctx.keg.join("libexec/post-install");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(
            &script,
            "#!/bin/sh\necho run >> \"$HOMEBREW_PREFIX/runs\"\nmkdir -p \"$(dirname \"$2\")\"\n(cat \"$1\"; echo built) > \"$2\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn bundle_step() -> StepAction {
        StepAction::Run {
            command: PathSpec::based(PathBase::Libexec, "post-install"),
            args: vec![
                "{{pkgshare}}/cacert.pem".into(),
                "{{pkgetc}}/cert.pem".into(),
            ],
            env: BTreeMap::new(),
            stdin_path: None,
            stdout_path: None,
            chdir: None,
            allow_failure: false,
            sudo: Sudo::Never,
        }
    }

    fn runs(ctx: &PostInstallContext) -> usize {
        fs::read_to_string(ctx.prefix.join("runs"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn the_bundle_is_built_once_and_restored_after() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        install_script(&ctx);
        let source = ctx.keg.join("share/ca-certificates/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "MOZILLA\n").unwrap();
        let destination = ctx.prefix.join("etc/ca-certificates/cert.pem");

        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 1);
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "MOZILLA\nbuilt\n"
        );

        fs::remove_file(&destination).unwrap();
        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 1, "the second run comes from the cache");
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "MOZILLA\nbuilt\n"
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // A new Mozilla bundle is a new key.
        fs::write(&source, "MOZILLA v2\n").unwrap();
        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 2);
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "MOZILLA v2\nbuilt\n"
        );
    }

    #[test]
    fn only_ca_certificates_is_cached() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "other-certs", "1");
        install_script(&ctx);
        let source = ctx.keg.join("share/other-certs/cacert.pem");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "X\n").unwrap();

        run_step(&ctx, &bundle_step()).unwrap();
        run_step(&ctx, &bundle_step()).unwrap();
        assert_eq!(runs(&ctx), 2);
        assert!(!ctx.cache.exists());
    }

    #[test]
    fn split_certificates_orders_like_the_scripts_glob() {
        let pem: String = (1..=12)
            .map(|n| format!("-----BEGIN CERTIFICATE-----\n{n}\n-----END CERTIFICATE-----\n"))
            .collect();
        let order: Vec<String> = split_certificates(pem.as_bytes())
            .iter()
            .map(|c| {
                String::from_utf8_lossy(c)
                    .lines()
                    .nth(1)
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            order,
            [
                "1", "10", "11", "12", "2", "3", "4", "5", "6", "7", "8", "9"
            ]
        );
    }

    #[test]
    fn split_certificates_reads_pem_like_the_scripts_awk() {
        let pem = b"junk\n-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n\
            between\n-----BEGIN CERTIFICATE-----\nlost\n-----BEGIN CERTIFICATE-----\nBBB\n\
            -----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nunterminated\n";
        assert_eq!(
            read_certificates(pem),
            [
                b"-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".to_vec(),
                b"-----BEGIN CERTIFICATE-----\nBBB\n-----END CERTIFICATE-----\n".to_vec(),
            ]
        );
    }

    /// The parallel build must give the script's bytes. It runs the real
    /// script from an installed ca-certificates (or `NB_TEST_CA_SCRIPT`)
    /// against the system keychain, so it is skipped where there is none.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_parallel_build_matches_the_script() {
        let script = std::env::var_os("NB_TEST_CA_SCRIPT")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/opt/homebrew/opt/ca-certificates/libexec/post-install".into());
        if known_script(&script).is_none() {
            eprintln!(
                "skipping: no known ca-certificates script at {}",
                script.display()
            );
            return;
        }
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        // Any PEM bundle stands in for the Mozilla one.
        let source = "/etc/ssl/cert.pem";
        let by_script = tmp.path().join("script/cert.pem");
        let status = std::process::Command::new(&script)
            .arg(source)
            .arg(&by_script)
            .status()
            .unwrap();
        assert!(status.success());

        let args = [
            source.to_string(),
            tmp.path().join("ours/cert.pem").display().to_string(),
        ];
        let bundle = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        assert!(bundle.build_in_parallel(&script).unwrap());

        let ours = fs::read(tmp.path().join("ours/cert.pem")).unwrap();
        assert_eq!(ours, fs::read(&by_script).unwrap());
        assert!(ours.len() > 100_000, "a real bundle: {} bytes", ours.len());
    }

    /// Building ahead fills the cache only; the step then restores from it.
    #[cfg(target_os = "macos")]
    #[test]
    fn prebuilding_fills_the_cache_the_step_restores_from() {
        let script = std::env::var_os("NB_TEST_CA_SCRIPT")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/opt/homebrew/opt/ca-certificates/libexec/post-install".into());
        if known_script(&script).is_none() {
            eprintln!(
                "skipping: no known ca-certificates script at {}",
                script.display()
            );
            return;
        }
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "2026-09-25");
        let destination = tmp.path().join("etc/cert.pem");
        let args = [
            "/etc/ssl/cert.pem".to_string(),
            destination.display().to_string(),
        ];
        let bundle = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();

        bundle.prebuild(&script);

        assert!(!destination.exists(), "only the cache is written ahead");
        assert!(bundle.restore().unwrap());
        assert!(fs::read(&destination).unwrap().len() > 100_000);
    }

    #[test]
    fn prebuilding_with_an_unknown_script_does_nothing() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "1");
        let script = install_script(&ctx);
        let source = ctx.keg.join("cacert.pem");
        fs::write(&source, "M").unwrap();
        let args = [
            source.display().to_string(),
            ctx.prefix.join("cert.pem").display().to_string(),
        ];
        let bundle = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        bundle.prebuild(&script);
        assert!(!bundle.restore().unwrap());
    }

    #[test]
    fn an_unknown_script_is_not_rebuilt_in_parallel() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "1");
        let script = install_script(&ctx);
        let source = ctx.keg.join("cacert.pem");
        fs::write(&source, "M").unwrap();
        let args = [
            source.display().to_string(),
            ctx.prefix.join("cert.pem").display().to_string(),
        ];
        let bundle = CaBundle::detect(&ctx, script.as_os_str(), &args).unwrap();
        assert!(!bundle.build_in_parallel(&script).unwrap());
        assert!(!ctx.prefix.join("cert.pem").exists());
    }

    #[test]
    fn excluded_keys_are_read_out_of_the_case_line() {
        let a = "a".repeat(64);
        let b = "0".repeat(64);
        let script = format!("x\n  case \"$h\" in\n    {a}|{b}) return 0 ;;\n  esac\n");
        let (template, excluded) = excluded_keys(&script);
        assert_eq!(
            template,
            "x\n  case \"$h\" in\n    @EXCLUDED_SPKI_HASHES@) return 0 ;;\n  esac\n"
        );
        assert_eq!(excluded, [a, b]);
        assert_eq!(excluded_keys("x\n").0, "x\n");
    }

    /// `NB_TEST_CA_SCRIPT` names a real script to check against [`KNOWN_SCRIPTS`].
    #[test]
    fn a_real_script_is_known() {
        let Some(script) = std::env::var_os("NB_TEST_CA_SCRIPT") else {
            return;
        };
        let (template, excluded) = excluded_keys(&fs::read_to_string(script).unwrap());
        assert!(KNOWN_SCRIPTS.contains(&hex(&Sha256::digest(template)).as_str()));
        assert_eq!(excluded.len(), 3);
    }

    #[test]
    fn detect_ignores_other_commands_and_missing_sources() {
        let tmp = TempDir::new().unwrap();
        let ctx = context(tmp.path(), "ca-certificates", "1");
        let args = ["a".to_string(), "b".to_string()];
        assert!(CaBundle::detect(&ctx, OsStr::new("/bin/sh"), &args).is_none());
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args).is_none());
        let source = ctx.keg.join("cacert.pem");
        fs::write(&source, "M").unwrap();
        let args = [source.display().to_string(), "b".to_string()];
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args).is_some());
        assert!(CaBundle::detect(&ctx, OsStr::new("post-install"), &args[..1]).is_none());
    }
}
