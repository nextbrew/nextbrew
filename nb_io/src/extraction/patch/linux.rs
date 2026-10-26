use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use nb_core::Error;
use rayon::prelude::*;
use tracing::warn;

use super::{RelocationFiles, in_keg, text};

const LINUX_HOMEBREW_PREFIX: &str = "/home/linuxbrew/.linuxbrew";

/// Patch @@HOMEBREW_CELLAR@@ and @@HOMEBREW_PREFIX@@ placeholders in both ELF binaries and text files.
#[cfg(target_os = "linux")]
pub fn patch_placeholders(
    keg_path: &Path,
    prefix_dir: &Path,
    _pkg_name: &str,
    _pkg_version: &str,
    files: Option<&RelocationFiles>,
) -> Result<(), Error> {
    // The interpreter of every ELF file is set, placeholder or not, so that
    // walk stays; the tab's lists spare the other two.
    patch_elf_placeholders(keg_path, prefix_dir)?;
    let (texts, binaries) = match files {
        Some(files) => (
            in_keg(keg_path, &files.text),
            in_keg(keg_path, &files.binary),
        ),
        None => {
            let all = regular_files(keg_path);
            (all.clone(), all)
        }
    };
    patch_text_placeholders(&texts, prefix_dir);
    patch_binary_prefixes(&binaries, prefix_dir);
    Ok(())
}

fn regular_files(keg_path: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect()
}

fn rewrite_homebrew_prefixes(input: &str, prefix_dir: &Path) -> String {
    let prefix_str = prefix_dir.to_string_lossy().into_owned();
    input
        .replace("@@HOMEBREW_PREFIX@@", &prefix_str)
        .replace("@@HOMEBREW_REPOSITORY@@", &prefix_str)
        .replace("@@HOMEBREW_LIBRARY@@", &format!("{}/Library", prefix_str))
        .replace(LINUX_HOMEBREW_PREFIX, &prefix_str)
}

/// Detect if nextbrew has installed its own glibc and return the path to its ld.so interpreter.
/// Returns None if nextbrew's glibc is not found, indicating we should use the system ld.so.
fn detect_nextbrew_glibc(prefix_dir: &Path) -> Option<PathBuf> {
    let cellar = prefix_dir.join("Cellar").join("glibc");

    if !cellar.exists() {
        return None;
    }

    // Look for glibc installations in the Cellar
    let glibc_entries = match fs::read_dir(&cellar) {
        Ok(entries) => entries,
        Err(_) => return None,
    };

    // Find the most recent glibc version directory
    let mut glibc_versions: Vec<PathBuf> = glibc_entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    if glibc_versions.is_empty() {
        return None;
    }

    // Sort to get the newest version (simple lexicographic sort should work for version numbers)
    glibc_versions.sort();
    glibc_versions.reverse();

    // Look for the ld.so interpreter in the glibc lib directory
    // Common names: ld-linux-x86-64.so.2, ld-linux-aarch64.so.1, ld-linux.so.2, etc.
    for glibc_dir in glibc_versions {
        let lib_dir = glibc_dir.join("lib");
        if !lib_dir.exists() {
            continue;
        }

        let entries = match fs::read_dir(&lib_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let filename = match path.file_name() {
                Some(name) => name.to_string_lossy(),
                None => continue,
            };

            // Match ld-linux*.so* patterns
            if filename.starts_with("ld-linux") && filename.contains(".so") {
                return Some(path);
            }
            // Also check for ld64.so.2 (ppc64)
            if filename == "ld64.so.2" || filename.starts_with("ld64.so.") {
                return Some(path);
            }
            // And ld-linux.so.* variants
            if filename.starts_with("ld-linux.so.") {
                return Some(path);
            }
        }
    }

    None
}

/// Find the system's dynamic linker (ld.so).
/// Returns the path to the system ld.so if found, None otherwise.
fn find_system_ld_so() -> Option<PathBuf> {
    // Common paths for system dynamic linkers on Linux
    let candidates = [
        "/lib64/ld-linux-x86-64.so.2",     // x86_64
        "/usr/lib64/ld-linux-x86-64.so.2", // x86_64
        "/lib/ld-linux-aarch64.so.1",      // aarch64/ARM64
        "/usr/lib/ld-linux-aarch64.so.1",  // aarch64/ARM64
        "/lib/ld-linux-armhf.so.3",        // ARM hard float
        "/usr/lib/ld-linux-armhf.so.3",    // ARM hard float
        "/lib/ld-linux.so.3",              // ARM
        "/lib/ld-linux.so.2",              // old ARM
        "/lib64/ld64.so.2",                // ppc64
        "/lib64/ld64.so.1",                // s390x
    ];

    for candidate in &candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Patch @@HOMEBREW_CELLAR@@ and @@HOMEBREW_PREFIX@@ placeholders in ELF binaries.
/// Uses `arwen` crate to natively update RPATH, RUNPATH, and optionally the ELF interpreter.
fn patch_elf_placeholders(keg_path: &Path, prefix_dir: &Path) -> Result<(), Error> {
    let lib_path = prefix_dir.join("lib").to_string_lossy().to_string();

    // Detect if nextbrew has installed its own glibc
    let nextbrew_interpreter = detect_nextbrew_glibc(prefix_dir);

    // Determine which interpreter to use:
    // - If nextbrew has glibc, use nextbrew's ld.so
    // - Otherwise, use the system ld.so (fallback)
    let target_interpreter = if let Some(ref nb_ld) = nextbrew_interpreter {
        Some(nb_ld.clone())
    } else {
        // Find system ld.so - common paths for Linux
        find_system_ld_so()
    };

    // Collect all ELF files
    let elf_files: Vec<PathBuf> = walkdir::WalkDir::new(keg_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            // Read only first 4 bytes to check magic
            let mut file = match fs::File::open(e.path()) {
                Ok(f) => f,
                Err(_) => return false,
            };
            let mut magic = [0u8; 4];
            if file.read_exact(&mut magic).is_ok() {
                return magic == *b"\x7fELF";
            }
            false
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    // Every path, hard links too: a patched file is a new one renamed into
    // place, so its links (perl's `bin/perl`) keep the unpatched one.
    let patch_failures = AtomicUsize::new(0);

    // Clone for use in parallel closure
    let target_interpreter = target_interpreter.clone();
    let new_prefix = prefix_dir.to_string_lossy().to_string();

    elf_files.par_iter().for_each(|path| {
        // Get permissions and make writable if needed
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let original_mode = metadata.permissions().mode();
        let is_readonly = original_mode & 0o200 == 0;

        if is_readonly {
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode | 0o200);
            if let Err(e) = fs::set_permissions(path, perms) {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to make ELF writable for patching"
                );
                patch_failures.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        let result = (|| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let content = fs::read(path)?;
            let mut elf = arwen::elf::ElfContainer::parse(&content)?;

            // Check if it is a dynamic ELF
            let has_dynamic_segment = elf
                .inner
                .builder()
                .segments
                .iter()
                .any(|s| s.p_type == object::elf::PT_DYNAMIC);
            if !has_dynamic_segment {
                return Ok(());
            }

            // Set page size for alignment
            let page_size = elf.get_page_size();
            let _ = elf.set_page_size(page_size);

            // RPATH, only where there is one, as Homebrew's `change_rpath!`:
            // adding one to a static-pie executable (glibc's ldconfig) breaks
            // its self-relocation.
            let mut changed = false;
            let old_rpaths = elf.get_rpath();
            if !old_rpaths.is_empty() {
                let mut new_rpaths: Vec<String> = old_rpaths
                    .iter()
                    .map(|r| rewrite_homebrew_prefixes(r, prefix_dir))
                    .filter(|r| r.starts_with(&new_prefix) || r.starts_with("$ORIGIN"))
                    .collect();
                if !new_rpaths.contains(&lib_path) {
                    new_rpaths.push(lib_path.clone());
                }
                if new_rpaths != old_rpaths {
                    elf.set_runpath(new_rpaths.join(":"))?;
                    changed = true;
                }
            }

            // Interpreter
            let is_executable = elf.inner.builder().header.e_type == object::elf::ET_EXEC
                || (elf.inner.builder().header.e_type == object::elf::ET_DYN
                    && elf.inner.elf_interpreter().is_some());

            if is_executable && let Some(current_interp_bytes) = elf.inner.elf_interpreter() {
                let current_interp_str = String::from_utf8_lossy(current_interp_bytes);

                let target_interp_path = if current_interp_str.contains("@@HOMEBREW_PREFIX@@")
                    || current_interp_str.contains(LINUX_HOMEBREW_PREFIX)
                {
                    let expanded = rewrite_homebrew_prefixes(&current_interp_str, prefix_dir);
                    let expanded_path = PathBuf::from(&expanded);
                    if expanded_path.exists() {
                        Some(expanded_path)
                    } else {
                        find_system_ld_so()
                    }
                } else {
                    target_interpreter.clone()
                };

                if let Some(target_path) = target_interp_path {
                    let target_str = target_path.to_string_lossy();
                    if target_str != current_interp_str {
                        elf.set_interpreter(&target_str)?;
                        changed = true;
                    }
                }
            }
            if !changed {
                return Ok(());
            }

            // Atomic write
            let temp_path = path.with_extension("tmp_patch");
            {
                let mut temp_file = fs::File::create(&temp_path)?;
                elf.write(&mut temp_file)?;
            }
            fs::rename(temp_path, path)?;

            // Restore original permissions (including execute bit) after atomic write
            let mut perms = metadata.permissions();
            perms.set_mode(original_mode);
            fs::set_permissions(path, perms)?;

            Ok(())
        })();

        if let Err(e) = result {
            warn!(path = %path.display(), error = %e, "failed to patch ELF");
            patch_failures.fetch_add(1, Ordering::Relaxed);
        }
    });

    let failures = patch_failures.load(Ordering::Relaxed);
    if failures > 0 {
        warn!(
            failures,
            "failed to patch ELF files; packages may not work correctly until manually patched"
        );
    }

    Ok(())
}

/// Patch text files containing @@HOMEBREW_...@@ placeholders
fn patch_text_placeholders(files: &[PathBuf], prefix_dir: &Path) {
    let prefix_str = prefix_dir.to_string_lossy().into_owned();
    let cellar_str = prefix_dir.join("Cellar").to_string_lossy().into_owned();
    let library_str = format!("{prefix_str}/Library");
    let replacements = [
        ("@@HOMEBREW_PREFIX@@", prefix_str.as_str()),
        ("@@HOMEBREW_REPOSITORY@@", prefix_str.as_str()),
        ("@@HOMEBREW_LIBRARY@@", library_str.as_str()),
        (LINUX_HOMEBREW_PREFIX, prefix_str.as_str()),
        ("@@HOMEBREW_CELLAR@@", cellar_str.as_str()),
        ("@@HOMEBREW_PERL@@", "/usr/bin/perl"),
        ("@@HOMEBREW_JAVA@@", "/usr/bin/java"),
    ];

    files.par_iter().for_each(|path| {
        if let Err(e) = text::rewrite_text_file(path, &replacements) {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to patch text file"
            );
        }
    });
}

/// Rewrite the Linux Homebrew prefix in place in binary files, static ELF
/// executables included: bottles pinned to its Cellar hardcode it. The
/// planner does not pour those into a longer prefix, so only a relocatable
/// bottle's stray reference can be left behind.
fn patch_binary_prefixes(files: &[PathBuf], prefix_dir: &Path) {
    let old = LINUX_HOMEBREW_PREFIX.as_bytes();
    let new = prefix_dir.as_os_str().as_encoded_bytes();
    if old == new {
        return;
    }
    files.par_iter().for_each(|path| {
        let result = (|| -> std::io::Result<()> {
            if !text::is_binary(path)? {
                return Ok(());
            }
            let mut content = fs::read(path)?;
            if new.len() > old.len() {
                if text::has_paths_under(&content, old) {
                    warn!(
                        path = %path.display(),
                        "binary has paths under {LINUX_HOMEBREW_PREFIX} that do not fit in {}",
                        prefix_dir.display()
                    );
                }
                return Ok(());
            }
            if !text::replace_prefix_in_place(&mut content, old, new).is_empty() {
                text::write_preserving_mode(path, &content)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            warn!(path = %path.display(), error = %e, "failed to patch binary file");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use std::process::Command;
    use tempfile::TempDir;

    fn compile_dummy_elf(dir: &Path, name: &str) -> Option<PathBuf> {
        let src_path = dir.join(format!("{}.c", name));
        if fs::write(&src_path, "int main() { return 0; }").is_err() {
            return None;
        }

        let out_path = dir.join(name);
        let status = Command::new("cc")
            .arg(&src_path)
            .arg("-o")
            .arg(&out_path)
            .arg("-Wl,-rpath,@@HOMEBREW_PREFIX@@/lib")
            .status()
            .ok()?;

        if status.success() {
            Some(out_path)
        } else {
            None
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn binaries_get_the_prefix_in_place_when_it_fits() {
        let tmp = TempDir::new().unwrap();
        let keg = tmp.path().join("keg");
        fs::create_dir_all(&keg).unwrap();
        let data = b"\x7fELF\0/home/linuxbrew/.linuxbrew/Cellar/glibc/2.39/share/i18n\0".to_vec();
        fs::write(keg.join("localedef"), &data).unwrap();

        patch_binary_prefixes(&regular_files(&keg), Path::new("/opt/nb"));
        let patched = fs::read(keg.join("localedef")).unwrap();
        assert_eq!(patched.len(), data.len());
        assert!(patched.starts_with(b"\x7fELF\0/opt/nb////////////////////Cellar/glibc/"));

        // A longer prefix is left alone.
        fs::write(keg.join("localedef"), &data).unwrap();
        patch_binary_prefixes(
            &regular_files(&keg),
            Path::new("/home/someone/.local/share/nextbrew/prefix"),
        );
        assert_eq!(fs::read(keg.join("localedef")).unwrap(), data);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn rewrites_linuxbrew_prefixes() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");

        assert_eq!(
            rewrite_homebrew_prefixes(
                "/home/linuxbrew/.linuxbrew/opt/expat/lib:@@HOMEBREW_PREFIX@@/lib",
                &prefix
            ),
            format!(
                "{}/opt/expat/lib:{}/lib",
                prefix.display(),
                prefix.display()
            )
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn patches_text_files() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        let pkg_dir = cellar.join("testpkg/1.0.0");
        let bin_dir = pkg_dir.join("bin");

        fs::create_dir_all(&bin_dir).unwrap();

        let script_path = bin_dir.join("script.sh");
        fs::write(
            &script_path,
            "#!/home/linuxbrew/.linuxbrew/opt/python@3.14/bin/python3.14\necho @@HOMEBREW_PREFIX@@\necho @@HOMEBREW_CELLAR@@\necho @@HOMEBREW_LIBRARY@@\necho @@HOMEBREW_PERL@@",
        )
        .unwrap();

        let result = patch_placeholders(&pkg_dir, &prefix, "testpkg", "1.0.0", None);
        assert!(result.is_ok());

        let content = fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(prefix.to_str().unwrap()));
        assert!(content.starts_with(&format!(
            "#!{}/opt/python@3.14/bin/python3.14",
            prefix.display()
        )));
        assert!(!content.contains(LINUX_HOMEBREW_PREFIX));
        assert!(content.contains(cellar.to_str().unwrap()));
        assert!(content.contains(&format!("{}/Library", prefix.to_str().unwrap())));
        assert!(content.contains("/usr/bin/perl"));
        assert!(!content.contains("@@HOMEBREW_"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn patches_elf_file() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        let pkg_dir = cellar.join("testpkg/1.0.0");
        let bin_dir = pkg_dir.join("bin");

        fs::create_dir_all(&bin_dir).unwrap();

        let elf_path = match compile_dummy_elf(&bin_dir, "testbin") {
            Some(p) => p,
            None => {
                eprintln!("Skipping ELF patch test: cc not found");
                return;
            }
        };

        // Record original permissions (should include execute bit from cc)
        let original_mode = fs::metadata(&elf_path).unwrap().permissions().mode();
        assert!(
            original_mode & 0o111 != 0,
            "compiled binary should be executable"
        );

        // A hard link to it, as perl's `bin/perl`, is patched too.
        let link = bin_dir.join("testbin-link");
        fs::hard_link(&elf_path, &link).unwrap();

        let result = patch_placeholders(&pkg_dir, &prefix, "testpkg", "1.0.0", None);
        assert!(result.is_ok());
        for path in [&elf_path, &link] {
            let contents = fs::read(path).unwrap();
            assert!(!contents.windows(19).any(|w| w == b"@@HOMEBREW_PREFIX@@"));
        }

        // Verify permissions are preserved after patching
        let new_mode = fs::metadata(&elf_path).unwrap().permissions().mode();
        assert_eq!(
            original_mode & 0o777,
            new_mode & 0o777,
            "permissions should be preserved after patching"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_glibc_detection() {
        let tmp = TempDir::new().unwrap();
        let prefix = tmp.path().join("prefix");

        // Test 1: No glibc installed - should return None
        assert!(detect_nextbrew_glibc(&prefix).is_none());

        // Test 2: Create a mock glibc installation
        let glibc_dir = prefix.join("Cellar/glibc/2.38");
        let lib_dir = glibc_dir.join("lib");
        fs::create_dir_all(&lib_dir).unwrap();

        // Create a mock ld-linux-x86-64.so.2
        let ld_so = lib_dir.join("ld-linux-x86-64.so.2");
        fs::write(&ld_so, "mock").unwrap();

        // Should now detect the glibc
        let detected = detect_nextbrew_glibc(&prefix);
        assert!(detected.is_some());
        assert_eq!(detected.unwrap(), ld_so);

        // Test 3: Multiple glibc versions - should pick the newest
        let glibc_dir_newer = prefix.join("Cellar/glibc/2.39");
        let lib_dir_newer = glibc_dir_newer.join("lib");
        fs::create_dir_all(&lib_dir_newer).unwrap();
        let ld_so_newer = lib_dir_newer.join("ld-linux-x86-64.so.2");
        fs::write(&ld_so_newer, "mock").unwrap();

        let detected = detect_nextbrew_glibc(&prefix);
        assert!(detected.is_some());
        assert_eq!(detected.unwrap(), ld_so_newer);
    }
}
