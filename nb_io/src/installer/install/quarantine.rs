//! Gatekeeper quarantine for cask downloads, as Homebrew does it
//! (`cask/quarantine.rb`): the download is quarantined through
//! LaunchServices, and what is unpacked from it gets the same attribute with
//! the no-translocation bit set, so macOS checks an app on its first launch.
//! Only macOS has quarantine; elsewhere these do nothing.

use std::path::Path;

use nb_core::Error;

#[cfg(target_os = "macos")]
const ATTRIBUTE: &std::ffi::CStr = c"com.apple.quarantine";

/// Quarantine `download` unless it already is, recording `url` and
/// `homepage` as where it came from.
pub(super) fn quarantine_download(download: &Path, url: &str, homepage: &str) -> Result<(), Error> {
    #[cfg(target_os = "macos")]
    {
        const SCRIPT: &str = r#"ObjC.import("Foundation");
function run(argv) {
  var props = $.NSDictionary.dictionaryWithObjectsForKeys(
    ["NextBrew", "LSQuarantineTypeWebDownload",
     $.NSURL.URLWithString(argv[1]), $.NSURL.URLWithString(argv[2])],
    ["LSQuarantineAgentName", "LSQuarantineType",
     "LSQuarantineDataURL", "LSQuarantineOriginURL"]);
  var ok = $.NSURL.fileURLWithPath(argv[0])
    .setResourceValueForKeyError(props, $.NSURLQuarantinePropertiesKey, null);
  return ok ? "ok" : "failed";
}"#;
        if read(download)?.is_some() {
            return Ok(());
        }
        let output = std::process::Command::new("osascript")
            .args(["-l", "JavaScript", "-e", SCRIPT])
            .arg(download)
            .args([url, homepage])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| failed(download, &e.to_string()))?;
        if String::from_utf8_lossy(&output.stdout).trim() != "ok" {
            return Err(failed(
                download,
                String::from_utf8_lossy(&output.stderr).trim(),
            ));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (download, url, homepage);
    Ok(())
}

/// Give everything under `to` (or `to` itself, if a file) the quarantine of
/// `from`, symlinks excepted, making each file writable for the owner first
/// as Homebrew's `propagate` does.
pub(super) fn propagate(from: &Path, to: &Path) -> Result<(), Error> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;

        let Some(value) = read(from)? else {
            return Ok(());
        };
        let value = no_translocation(&value);
        let min_depth = usize::from(to.is_dir());
        for entry in walkdir::WalkDir::new(to).min_depth(min_depth) {
            let entry = entry.map_err(|e| failed(to, &e.to_string()))?;
            if entry.path_is_symlink() {
                continue;
            }
            let path = entry.path();
            let mode = entry
                .metadata()
                .map_err(|e| failed(path, &e.to_string()))?
                .permissions()
                .mode();
            if mode & 0o200 == 0 {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200))
                    .map_err(|e| failed(path, &e.to_string()))?;
            }
            write(path, &value)?;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (from, to);
    Ok(())
}

/// Set the "do not translocate" flag in a quarantine value's flags field.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn no_translocation(value: &str) -> String {
    let (flags, rest) = value.split_once(';').unwrap_or((value, ""));
    let flags = u32::from_str_radix(flags, 16).unwrap_or(0) | 0x0100;
    if rest.is_empty() {
        format!("{flags:04x}")
    } else {
        format!("{flags:04x};{rest}")
    }
}

#[cfg(target_os = "macos")]
fn read(path: &Path) -> Result<Option<String>, Error> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| failed(path, &e.to_string()))?;
    let mut buf = [0u8; 512];
    // SAFETY: both strings are NUL-terminated and `buf` outlives the call.
    let len = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            ATTRIBUTE.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if len < 0 {
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::ENOATTR) => Ok(None),
            _ => Err(failed(path, &err.to_string())),
        };
    }
    Ok(Some(
        String::from_utf8_lossy(&buf[..len as usize]).into_owned(),
    ))
}

#[cfg(target_os = "macos")]
fn write(path: &Path, value: &str) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| failed(path, &e.to_string()))?;
    // SAFETY: both strings are NUL-terminated and `value` outlives the call.
    let rc = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            ATTRIBUTE.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(failed(path, &std::io::Error::last_os_error().to_string()));
    }
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn failed(path: &Path, detail: &str) -> Error {
    Error::ExecutionError {
        message: format!("failed to quarantine {}: {detail}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_translocation_sets_the_flag_and_keeps_the_rest() {
        assert_eq!(
            no_translocation("0081;6ad03a47;NextBrew;A6B8C266"),
            "0181;6ad03a47;NextBrew;A6B8C266"
        );
        assert_eq!(no_translocation("0181;x"), "0181;x");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn propagate_marks_files_and_directories_but_not_symlinks() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let download = tmp.path().join("download");
        fs::write(&download, b"x").unwrap();
        write(&download, "0081;6ad03a47;NextBrew;UUID").unwrap();

        let staged = tmp.path().join("staged");
        fs::create_dir_all(staged.join("Foo.app/Contents")).unwrap();
        let binary = staged.join("Foo.app/Contents/foo");
        fs::write(&binary, b"bin").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o555)).unwrap();
        std::os::unix::fs::symlink("Contents/foo", staged.join("Foo.app/link")).unwrap();

        propagate(&download, &staged).unwrap();

        let marked = Some("0181;6ad03a47;NextBrew;UUID".to_string());
        assert_eq!(read(&staged.join("Foo.app")).unwrap(), marked);
        assert_eq!(read(&binary).unwrap(), marked);
        assert_eq!(read(&staged.join("Foo.app/link")).unwrap(), None);
        assert_eq!(read(&staged).unwrap(), None);
        assert_ne!(
            fs::metadata(&binary).unwrap().permissions().mode() & 0o200,
            0
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn propagate_without_quarantine_on_the_download_does_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let download = tmp.path().join("download");
        let file = tmp.path().join("file");
        std::fs::write(&download, b"x").unwrap();
        std::fs::write(&file, b"y").unwrap();

        propagate(&download, &file).unwrap();

        assert_eq!(read(&file).unwrap(), None);
    }
}
