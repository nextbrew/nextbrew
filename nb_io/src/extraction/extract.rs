use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

use nb_core::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressionFormat {
    Gzip,
    Xz,
    Zstd,
    Zip,
    Unknown,
}

pub fn is_archive(path: &Path) -> Result<bool, Error> {
    let mut file = File::open(path).map_err(Error::store("failed to open tarball"))?;
    let mut magic = [0u8; MAGIC_LEN];
    let bytes_read = file
        .read(&mut magic)
        .map_err(Error::store("failed to read magic bytes"))?;
    Ok(!matches!(
        detect_compression(&magic[..bytes_read]),
        CompressionFormat::Unknown
    ))
}

/// Bytes needed to tell the supported formats apart.
const MAGIC_LEN: usize = 6;

fn detect_compression(magic: &[u8]) -> CompressionFormat {
    if magic.len() < 2 {
        return CompressionFormat::Unknown;
    }

    // Gzip: 1f 8b
    if magic[0] == 0x1f && magic[1] == 0x8b {
        return CompressionFormat::Gzip;
    }

    // XZ: fd 37 7a 58 5a 00 (FD 7zXZ\0)
    if magic.len() >= 6 && magic[0..6] == [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00] {
        return CompressionFormat::Xz;
    }

    // Zstd: 28 b5 2f fd
    if magic.len() >= 4 && magic[0..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        return CompressionFormat::Zstd;
    }

    // ZIP: 50 4b 03 04
    if magic.len() >= 4 && magic[0..4] == [0x50, 0x4b, 0x03, 0x04] {
        return CompressionFormat::Zip;
    }

    CompressionFormat::Unknown
}

pub fn extract_tarball(tarball_path: &Path, dest_dir: &Path) -> Result<(), Error> {
    extract_archive(tarball_path, dest_dir)
}

pub fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<(), Error> {
    let file = File::open(archive_path).map_err(Error::store("failed to open archive"))?;
    match extract_archive_from_reader(BufReader::new(file), dest_dir)? {
        Extracted::Done => Ok(()),
        // Zip archives keep their directory at the end, so they are read
        // from the file, not the stream.
        Extracted::NeedsSeek => extract_zip_archive(archive_path, dest_dir),
    }
}

/// What [`extract_archive_from_reader`] did with the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Extracted {
    Done,
    /// A format that cannot be unpacked from a stream; the caller has to
    /// go back to the file.
    NeedsSeek,
}

/// Unpack a compressed tarball as it is read, detecting the compression
/// from its first bytes. Used to unpack a bottle while it is still
/// downloading.
pub fn extract_archive_from_reader<R: Read>(
    mut reader: R,
    dest_dir: &Path,
) -> Result<Extracted, Error> {
    let mut magic = [0u8; MAGIC_LEN];
    let mut filled = 0;
    while filled < MAGIC_LEN {
        let n = reader
            .read(&mut magic[filled..])
            .map_err(Error::store("failed to read magic bytes"))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let format = detect_compression(&magic[..filled]);
    let reader = std::io::Cursor::new(magic[..filled].to_vec()).chain(reader);

    match format {
        CompressionFormat::Gzip => extract_tar_archive(GzDecoder::new(reader), dest_dir)?,
        CompressionFormat::Xz => extract_tar_archive(XzDecoder::new(reader), dest_dir)?,
        CompressionFormat::Zstd => {
            let decoder =
                ZstdDecoder::new(reader).map_err(Error::store("failed to create zstd decoder"))?;
            extract_tar_archive(decoder, dest_dir)?
        }
        CompressionFormat::Zip => return Ok(Extracted::NeedsSeek),
        CompressionFormat::Unknown => {
            // Try gzip as fallback
            extract_tar_archive(GzDecoder::new(reader), dest_dir)?
        }
    }
    Ok(Extracted::Done)
}

fn extract_tar_archive<R: Read>(reader: R, dest_dir: &Path) -> Result<(), Error> {
    let mut archive = Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(true);
    let entries = archive
        .entries()
        .map_err(Error::store("failed to read archive entries"))?;

    // Regular files are written by a few threads while the archive is read:
    // a bottle like ruby's has 20,000 of them, and writing them one by one
    // took longer than the download.
    let writers = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let (jobs, queue) = std::sync::mpsc::sync_channel::<FileJob>(64);
    let queue = std::sync::Mutex::new(queue);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|_| {
                scope.spawn(|| {
                    let mut result = Ok(());
                    loop {
                        let Ok(job) = queue.lock().unwrap().recv() else {
                            break;
                        };
                        // Keep draining after a failure so the reader never blocks.
                        if result.is_ok() {
                            result = job.write();
                        }
                    }
                    result
                })
            })
            .collect();
        let read = read_entries(entries, dest_dir, &jobs);
        drop(jobs);
        let written = handles
            .into_iter()
            .map(|handle| handle.join().expect("file writer panicked"))
            .collect::<Result<Vec<_>, _>>();
        let finish = read?;
        written?;
        finish.run(dest_dir)
    })
}

/// Files at most this big go to the writer threads; bigger ones are written
/// as they are read, which keeps the memory in flight small.
const QUEUED_FILE_LIMIT: u64 = 1 << 20;

struct FileJob {
    path: PathBuf,
    contents: Vec<u8>,
    mode: u32,
    mtime: Option<std::time::SystemTime>,
}

impl FileJob {
    fn write(&self) -> Result<(), Error> {
        write_file(&self.path, &self.contents[..], self.mode, self.mtime)
    }
}

/// Write a file as tar does: anew, replacing whatever was there without
/// following it, with the archive's times and then its mode.
fn write_file(
    path: &Path,
    mut contents: impl Read,
    mode: u32,
    mtime: Option<std::time::SystemTime>,
) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let ctx = || format!("failed to unpack entry {}", path.display());
    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    };
    let mut file = match create() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(path).map_err(Error::store(&ctx()))?;
            create()
        }
        other => other,
    }
    .map_err(Error::store(&ctx()))?;
    std::io::copy(&mut contents, &mut file).map_err(Error::store(&ctx()))?;
    if let Some(mtime) = mtime {
        let times = std::fs::FileTimes::new()
            .set_accessed(mtime)
            .set_modified(mtime);
        file.set_times(times).map_err(Error::store(&ctx()))?;
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(Error::store(&ctx()))
}

/// What is left once every file is written: hard links, which need their
/// targets, directory modes, which could stop the writes, and on macOS the
/// renames to decomposed names.
#[derive(Default)]
struct Finish {
    hard_links: Vec<(PathBuf, PathBuf)>,
    dir_modes: Vec<(PathBuf, u32)>,
    #[cfg(target_os = "macos")]
    renames: Vec<(PathBuf, String)>,
}

impl Finish {
    fn run(self, dest_dir: &Path) -> Result<(), Error> {
        use std::os::unix::fs::PermissionsExt;
        for (link, target) in self.hard_links {
            let _ = std::fs::remove_file(&link);
            std::fs::hard_link(&target, &link).map_err(Error::store(&format!(
                "failed to hard link {}",
                link.display()
            )))?;
        }
        for (dir, mode) in self.dir_modes.into_iter().rev() {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
                .map_err(Error::store("failed to set directory mode"))?;
        }
        #[cfg(target_os = "macos")]
        for (path, name) in self.renames {
            let from = dest_dir.join(&path);
            let to = from.with_file_name(name);
            // The same file to APFS, which only stores the name anew.
            std::fs::rename(&from, &to).map_err(Error::store("failed to rename entry"))?;
        }
        let _ = dest_dir;
        Ok(())
    }
}

fn read_entries<R: Read>(
    entries: tar::Entries<'_, R>,
    dest_dir: &Path,
    jobs: &std::sync::mpsc::SyncSender<FileJob>,
) -> Result<Finish, Error> {
    let mut finish = Finish::default();
    let mut dirs = Dirs::default();
    for entry in entries {
        let mut entry = entry.map_err(Error::store("failed to read archive entry"))?;
        let entry_path = entry
            .path()
            .map_err(Error::store("failed to read entry path"))?
            .into_owned();
        let path_display = entry_path.display().to_string();

        // Security check: validate path doesn't escape destination
        validate_path(&entry_path, dest_dir)?;

        let rel: PathBuf = entry_path
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        #[cfg(target_os = "macos")]
        finish.renames.extend(decomposed_names(&entry_path));

        let ctx = format!("failed to unpack entry {path_display}");
        let kind = entry.header().entry_type();
        let plain = kind.is_dir() || kind.is_file() || kind.is_symlink() || kind.is_hard_link();
        if !plain || has_xattrs(&mut entry) {
            entry.unpack_in(dest_dir).map_err(Error::store(&ctx))?;
            dirs.forget(&rel);
            continue;
        }

        if let Some(parent) = rel.parent() {
            dirs.ensure(dest_dir, parent)?;
        }
        let target = dest_dir.join(&rel);
        let mode = entry.header().mode().map_err(Error::store(&ctx))?;
        if kind.is_dir() {
            dirs.ensure(dest_dir, &rel)?;
            finish.dir_modes.push((target, mode));
        } else if kind.is_symlink() || kind.is_hard_link() {
            let link = entry
                .link_name()
                .map_err(Error::store(&ctx))?
                .ok_or_else(|| Error::StoreCorruption {
                    message: format!("link without a target: {path_display}"),
                })?
                .into_owned();
            if kind.is_hard_link() {
                validate_path(&link, dest_dir)?;
                finish.hard_links.push((target, dest_dir.join(&link)));
            } else {
                dirs.forget(&rel);
                let _ = std::fs::remove_file(&target);
                std::os::unix::fs::symlink(&link, &target).map_err(Error::store(&ctx))?;
            }
        } else {
            // tar never writes a file with mtime 0.
            let mtime =
                entry.header().mtime().ok().map(|secs| {
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(1))
                });
            if entry.size() > QUEUED_FILE_LIMIT {
                write_file(&target, &mut entry, mode, mtime)?;
            } else {
                let mut contents = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut contents)
                    .map_err(Error::store(&ctx))?;
                let job = FileJob {
                    path: target,
                    contents,
                    mode,
                    mtime,
                };
                if jobs.send(job).is_err() {
                    break;
                }
            }
        }
    }
    Ok(finish)
}

/// Whether the entry carries extended attributes, which tar sets by path.
fn has_xattrs<R: Read>(entry: &mut tar::Entry<'_, R>) -> bool {
    let Ok(Some(extensions)) = entry.pax_extensions() else {
        return false;
    };
    extensions
        .flatten()
        .any(|extension| extension.key_bytes().starts_with(b"SCHILY.xattr."))
}

/// The directories under the destination known to be real ones, so each is
/// checked once rather than for every file in it.
#[derive(Default)]
struct Dirs(std::collections::HashSet<PathBuf>);

impl Dirs {
    /// Make `rel` and its parents, refusing to go through a symlink that
    /// leads out of the destination.
    fn ensure(&mut self, dest_dir: &Path, rel: &Path) -> Result<(), Error> {
        let mut prefix = PathBuf::new();
        for component in rel.components() {
            prefix.push(component);
            if self.0.contains(&prefix) {
                continue;
            }
            let dir = dest_dir.join(&prefix);
            match std::fs::symlink_metadata(&dir) {
                Ok(meta) if meta.is_dir() => {}
                Ok(meta) if meta.file_type().is_symlink() => {
                    let inside = dir.canonicalize().ok().zip(dest_dir.canonicalize().ok());
                    if !inside.is_some_and(|(dir, dest)| dir.starts_with(dest) && dir.is_dir()) {
                        return Err(Error::StoreCorruption {
                            message: format!(
                                "archive entry written through a symlink: {}",
                                rel.display()
                            ),
                        });
                    }
                }
                Ok(_) => {
                    return Err(Error::StoreCorruption {
                        message: format!("not a directory: {}", prefix.display()),
                    });
                }
                Err(_) => match std::fs::create_dir(&dir) {
                    Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                        return Err(Error::store("failed to create directory")(e));
                    }
                    _ => {}
                },
            }
            self.0.insert(prefix.clone());
        }
        Ok(())
    }

    /// `rel` may no longer be a directory.
    fn forget(&mut self, rel: &Path) {
        if self.0.contains(rel) {
            self.0.retain(|dir| !dir.starts_with(rel));
        }
    }
}

/// The names in `path` that macOS's tar (libarchive), which Homebrew
/// pours bottles with, writes decomposed (NFD): each path, from the
/// first, whose last name is not, with that name as it writes it. APFS
/// finds a file under either form, but lists it as written.
#[cfg(target_os = "macos")]
fn decomposed_names(path: &Path) -> Vec<(PathBuf, String)> {
    use unicode_normalization::UnicodeNormalization;
    let mut names = Vec::new();
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        let Some(name) = component.as_os_str().to_str() else {
            continue;
        };
        if name.is_ascii() {
            continue;
        }
        let decomposed: String = name.nfd().collect();
        if decomposed != name {
            names.push((prefix.clone(), decomposed));
        }
    }
    names
}

fn extract_zip_archive(path: &Path, dest_dir: &Path) -> Result<(), Error> {
    let file = File::open(path).map_err(Error::store("failed to open zip archive"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(Error::store("failed to open zip archive"))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(Error::store("failed to read zip entry"))?;
        let Some(raw_path) = entry.enclosed_name().map(|p| p.to_path_buf()) else {
            return Err(Error::StoreCorruption {
                message: "zip entry with invalid path".to_string(),
            });
        };

        validate_path(&raw_path, dest_dir)?;

        let out_path = dest_dir.join(&raw_path);
        refuse_symlinked_parent(&raw_path, dest_dir)?;

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(Error::store("failed to create output directory"))?;
            continue;
        }

        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(Error::store("failed to create output parent directory"))?;
        }

        if entry.is_symlink() {
            let mut link_target = String::new();
            entry
                .read_to_string(&mut link_target)
                .map_err(Error::store("failed to read zip symlink"))?;
            let _ = std::fs::remove_file(&out_path);
            std::os::unix::fs::symlink(&link_target, &out_path)
                .map_err(Error::store("failed to create zip symlink"))?;
            continue;
        }

        let mut output =
            File::create(&out_path).map_err(Error::store("failed to create extracted file"))?;
        std::io::copy(&mut entry, &mut output)
            .map_err(Error::store("failed to extract zip entry"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Only the permission bits, as unzip does without -K: a
            // download must not install setuid or setgid files.
            if let Some(mode) = entry.unix_mode() {
                let perms = std::fs::Permissions::from_mode(mode & 0o777);
                std::fs::set_permissions(&out_path, perms)
                    .map_err(Error::store("failed to set zip file permissions"))?;
            }
        }
    }

    Ok(())
}

/// Refuse an entry whose directory is a symlink made by an earlier entry,
/// so nothing is written outside the destination through it.
fn refuse_symlinked_parent(path: &Path, dest_dir: &Path) -> Result<(), Error> {
    let mut current = dest_dir.to_path_buf();
    let components: Vec<_> = path.components().collect();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component);
        if current
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(Error::StoreCorruption {
                message: format!("zip entry written through a symlink: {}", path.display()),
            });
        }
    }
    Ok(())
}

/// Validate that a path from a tar entry is safe to extract.
///
/// This function ensures:
/// 1. The path is not absolute
/// 2. The path contains no `..` components  
/// 3. When joined with dest_dir, the normalized path stays within dest_dir
///
/// The normalization is done without filesystem access, so it works for
/// files that don't exist yet.
fn validate_path(path: &Path, dest_dir: &Path) -> Result<(), Error> {
    // Reject absolute paths
    if path.is_absolute() {
        return Err(Error::StoreCorruption {
            message: format!("absolute path in archive: {}", path.display()),
        });
    }

    // Reject paths with .. components
    for component in path.components() {
        if let std::path::Component::ParentDir = component {
            return Err(Error::StoreCorruption {
                message: format!("path traversal in archive: {}", path.display()),
            });
        }
    }

    // Here we normalize the full path and verify it stays within dest_dir.
    // This catches edge cases where multiple normal components could somehow escape
    // (though the .. check above should prevent this in practice, so this shouldn't fire).
    let full_path = dest_dir.join(path);
    let normalized = normalize_path(&full_path);

    // Normalize dest_dir for comparison
    let normalized_dest = normalize_path(dest_dir);

    if !normalized.starts_with(&normalized_dest) {
        return Err(Error::StoreCorruption {
            message: format!(
                "path escapes destination directory: {} (normalized: {}) not within {}",
                path.display(),
                normalized.display(),
                normalized_dest.display()
            ),
        });
    }

    Ok(())
}

/// Normalize a path by resolving . and .. components without filesystem access.
///
/// This is safer than `canonicalize()` because:
/// - It works for paths that don't exist yet
/// - It doesn't follow symlinks (which could be malicious in a tarball)
/// - It's purely lexical, making behavior predictable
///
/// For absolute paths, .. components cannot escape above the root.
/// For relative paths, leading .. components are preserved.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = Vec::new();
    let mut is_absolute = false;

    for component in path.components() {
        match component {
            Component::RootDir => {
                // This is an absolute path
                is_absolute = true;
                components.push(component);
            }
            Component::CurDir => {
                // Skip . components
            }
            Component::ParentDir => {
                // Try to pop the last component
                if !components.is_empty() {
                    let last = components.last();
                    // Only pop if the last component is a normal component
                    if matches!(last, Some(Component::Normal(_))) {
                        components.pop();
                    } else if matches!(last, Some(Component::RootDir)) {
                        // For absolute paths, cannot go above root - silently ignore
                        // e.g., /foo/../../bar -> /bar (the extra .. is dropped)
                    } else {
                        // For relative paths, keep the .. if we hit prefix or another ..
                        components.push(component);
                    }
                } else if !is_absolute {
                    // Keep leading .. for relative paths
                    components.push(component);
                }
                // For absolute paths with nothing to pop, drop the .. (can't go above root)
            }
            _ => {
                components.push(component);
            }
        }
    }

    // Reconstruct the path from components
    components.iter().collect()
}

/// Extract a tarball from a reader (assumes gzip compression).
/// For file-based extraction with auto-detection, use `extract_tarball` instead.
pub fn extract_tarball_from_reader<R: Read>(reader: R, dest_dir: &Path) -> Result<(), Error> {
    let decoder = GzDecoder::new(reader);
    extract_tar_archive(decoder, dest_dir)
}

#[cfg(test)]
mod reader_tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use tempfile::TempDir;

    fn gz_tarball() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("pkg/1.0/bin/tool").unwrap();
        header.set_size(5);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &b"hello"[..]).unwrap();
        let tar = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn unpacks_a_gzip_stream_without_seeking() {
        let tmp = TempDir::new().unwrap();
        // A reader that hands out one byte at a time, like a slow network.
        struct Trickle(std::io::Cursor<Vec<u8>>);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let len = buf.len().min(1);
                self.0.read(&mut buf[..len])
            }
        }

        let done =
            extract_archive_from_reader(Trickle(std::io::Cursor::new(gz_tarball())), tmp.path())
                .unwrap();

        assert_eq!(done, Extracted::Done);
        assert_eq!(
            fs::read_to_string(tmp.path().join("pkg/1.0/bin/tool")).unwrap(),
            "hello"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn names_are_written_decomposed_as_macos_tar_writes_them() {
        let tmp = TempDir::new().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind) in [
            ("aspell/1.0/lib/d\u{e9}j\u{e0}/", tar::EntryType::Directory),
            (
                "aspell/1.0/lib/d\u{e9}j\u{e0}/kasz\u{eb}bsczi.alias",
                tar::EntryType::Regular,
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_entry_type(kind);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append(&header, &b""[..]).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();

        extract_archive_from_reader(&encoder.finish().unwrap()[..], tmp.path()).unwrap();

        let names = |dir: &std::path::Path| -> Vec<String> {
            fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        };
        let lib = tmp.path().join("aspell/1.0/lib");
        assert_eq!(names(&lib), ["de\u{301}ja\u{300}"]);
        assert_eq!(
            names(&lib.join("de\u{301}ja\u{300}")),
            ["kasze\u{308}bsczi.alias"]
        );
    }

    #[test]
    fn zip_streams_are_handed_back_to_the_file_path() {
        let tmp = TempDir::new().unwrap();
        let done = extract_archive_from_reader(&b"PK\x03\x04 not really"[..], tmp.path()).unwrap();
        assert_eq!(done, Extracted::NeedsSeek);
        assert!(fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn garbage_streams_fail_cleanly() {
        let tmp = TempDir::new().unwrap();
        assert!(
            extract_archive_from_reader(&b"this is not an archive at all"[..], tmp.path()).is_err()
        );
        assert!(extract_archive_from_reader(&b""[..], tmp.path()).is_err());
    }

    use std::fs;
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tar::Builder;
    use tempfile::TempDir;

    fn create_test_tarball(entries: Vec<(&str, &[u8], Option<u32>)>) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        for (path, content, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(mode.unwrap_or(0o644));
            header.set_cksum();
            builder.append(&header, content).unwrap();
        }

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn create_tarball_with_symlink(name: &str, target: &str) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());

        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_path(name).unwrap();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();

        builder.append_link(&mut header, name, target).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    fn create_test_zip(entries: Vec<(&str, &[u8])>) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let cursor = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);

        for (path, content) in entries {
            zip.start_file(path, SimpleFileOptions::default()).unwrap();
            zip.write_all(content).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    fn gz(builder: Builder<Vec<u8>>) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&builder.into_inner().unwrap()).unwrap();
        encoder.finish().unwrap()
    }

    fn header(path: &str, kind: tar::EntryType, size: u64, mode: u32) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_entry_type(kind);
        header.set_size(size);
        header.set_mode(mode);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        header
    }

    #[test]
    fn writes_many_files_with_their_modes_and_times() {
        let tmp = TempDir::new().unwrap();
        let mut builder = Builder::new(Vec::new());
        let big = vec![7u8; (QUEUED_FILE_LIMIT + 1) as usize];
        builder
            .append(
                &header("pkg/big", tar::EntryType::Regular, big.len() as u64, 0o644),
                &big[..],
            )
            .unwrap();
        for i in 0..500 {
            let body = format!("file {i}");
            let mode = if i % 2 == 0 { 0o755 } else { 0o444 };
            let path = format!("pkg/d{}/f{i}", i % 7);
            builder
                .append(
                    &header(&path, tar::EntryType::Regular, body.len() as u64, mode),
                    body.as_bytes(),
                )
                .unwrap();
        }

        extract_archive_from_reader(&gz(builder)[..], tmp.path()).unwrap();

        assert_eq!(fs::read(tmp.path().join("pkg/big")).unwrap(), big);
        for i in 0..500 {
            let path = tmp.path().join(format!("pkg/d{}/f{i}", i % 7));
            assert_eq!(fs::read_to_string(&path).unwrap(), format!("file {i}"));
            let meta = fs::metadata(&path).unwrap();
            let mode = if i % 2 == 0 { 0o755 } else { 0o444 };
            assert_eq!(meta.permissions().mode() & 0o7777, mode);
            assert_eq!(
                meta.modified().unwrap(),
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
            );
        }
    }

    #[test]
    fn hard_links_and_read_only_directories_unpack() {
        let tmp = TempDir::new().unwrap();
        let mut builder = Builder::new(Vec::new());
        builder
            .append(
                &header("pkg/ro/", tar::EntryType::Directory, 0, 0o555),
                &b""[..],
            )
            .unwrap();
        builder
            .append(
                &header("pkg/ro/a", tar::EntryType::Regular, 2, 0o644),
                &b"hi"[..],
            )
            .unwrap();
        let mut link = header("pkg/ro/b", tar::EntryType::Link, 0, 0o644);
        builder
            .append_link(&mut link, "pkg/ro/b", "pkg/ro/a")
            .unwrap();

        extract_archive_from_reader(&gz(builder)[..], tmp.path()).unwrap();

        let ro = tmp.path().join("pkg/ro");
        assert_eq!(fs::read_to_string(ro.join("b")).unwrap(), "hi");
        assert_eq!(
            fs::metadata(&ro).unwrap().permissions().mode() & 0o777,
            0o555
        );
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn refuses_a_file_written_through_a_symlink_out_of_the_destination() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let mut builder = Builder::new(Vec::new());
        let mut link = header("pkg/escape", tar::EntryType::Symlink, 0, 0o777);
        builder
            .append_link(&mut link, "pkg/escape", outside.path())
            .unwrap();
        builder
            .append(
                &header("pkg/escape/evil", tar::EntryType::Regular, 4, 0o644),
                &b"evil"[..],
            )
            .unwrap();

        assert!(extract_archive_from_reader(&gz(builder)[..], tmp.path()).is_err());
        assert!(!outside.path().join("evil").exists());
    }

    #[test]
    fn extracts_file_with_content() {
        let tmp = TempDir::new().unwrap();
        let tarball = create_test_tarball(vec![("hello.txt", b"Hello, World!", None)]);

        let tarball_path = tmp.path().join("test.tar.gz");
        fs::write(&tarball_path, &tarball).unwrap();

        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        extract_tarball(&tarball_path, &dest).unwrap();

        let content = fs::read_to_string(dest.join("hello.txt")).unwrap();
        assert_eq!(content, "Hello, World!");
    }

    #[test]
    fn extracts_zip_file_with_content() {
        let tmp = TempDir::new().unwrap();
        let zip_data = create_test_zip(vec![("op", b"#!/bin/sh\necho op")]);

        let zip_path = tmp.path().join("test.zip");
        fs::write(&zip_path, &zip_data).unwrap();

        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        extract_archive(&zip_path, &dest).unwrap();

        let content = fs::read_to_string(dest.join("op")).unwrap();
        assert_eq!(content, "#!/bin/sh\necho op");
    }

    #[test]
    fn extracts_zip_symlink_as_link() {
        use zip::write::SimpleFileOptions;

        let tmp = TempDir::new().unwrap();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("sdk/bin/dart", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"#!/bin/sh").unwrap();
        zip.add_symlink("sdk/bin/latest", "dart", SimpleFileOptions::default())
            .unwrap();
        let zip_path = tmp.path().join("test.zip");
        fs::write(&zip_path, zip.finish().unwrap().into_inner()).unwrap();
        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        extract_archive(&zip_path, &dest).unwrap();

        let link = dest.join("sdk/bin/latest");
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("dart"));
        assert_eq!(fs::read_to_string(&link).unwrap(), "#!/bin/sh");
    }

    #[test]
    fn rejects_zip_entry_written_through_symlink() {
        use zip::write::SimpleFileOptions;

        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.add_symlink(
            "escape",
            outside.to_str().unwrap(),
            SimpleFileOptions::default(),
        )
        .unwrap();
        zip.start_file("escape/planted", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        let zip_path = tmp.path().join("test.zip");
        fs::write(&zip_path, zip.finish().unwrap().into_inner()).unwrap();
        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        assert!(extract_archive(&zip_path, &dest).is_err());
        assert!(!outside.join("planted").exists());
    }

    #[test]
    fn preserves_executable_bit() {
        let tmp = TempDir::new().unwrap();
        let tarball = create_test_tarball(vec![("script.sh", b"#!/bin/sh\necho hi", Some(0o755))]);

        let tarball_path = tmp.path().join("test.tar.gz");
        fs::write(&tarball_path, &tarball).unwrap();

        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        extract_tarball(&tarball_path, &dest).unwrap();

        let metadata = fs::metadata(dest.join("script.sh")).unwrap();
        let mode = metadata.permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "executable bit not preserved: {:o}",
            mode
        );
    }

    #[test]
    fn preserves_symlink() {
        let tmp = TempDir::new().unwrap();
        let tarball = create_tarball_with_symlink("link", "target.txt");

        let tarball_path = tmp.path().join("test.tar.gz");
        fs::write(&tarball_path, &tarball).unwrap();

        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        extract_tarball(&tarball_path, &dest).unwrap();

        let link_path = dest.join("link");
        assert!(
            link_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&link_path).unwrap(),
            PathBuf::from("target.txt")
        );
    }

    fn create_malicious_tarball(path: &[u8]) -> Vec<u8> {
        // Manually construct a tar header with unsafe path
        let mut tar_data = vec![0u8; 512 + 512]; // header + one block of data

        // Copy path into header (bytes 0-99)
        let path_len = path.len().min(100);
        tar_data[..path_len].copy_from_slice(&path[..path_len]);

        // Set mode (bytes 100-107) - "0000644\0"
        tar_data[100..108].copy_from_slice(b"0000644\0");

        // Set uid (bytes 108-115) - "0000000\0"
        tar_data[108..116].copy_from_slice(b"0000000\0");

        // Set gid (bytes 116-123) - "0000000\0"
        tar_data[116..124].copy_from_slice(b"0000000\0");

        // Set size (bytes 124-135) - "00000000004\0" for 4 bytes
        tar_data[124..136].copy_from_slice(b"00000000004\0");

        // Set mtime (bytes 136-147) - "00000000000\0"
        tar_data[136..148].copy_from_slice(b"00000000000\0");

        // Set typeflag (byte 156) - '0' for regular file
        tar_data[156] = b'0';

        // Calculate checksum (bytes 148-155)
        // First set checksum field to spaces
        tar_data[148..156].copy_from_slice(b"        ");

        let checksum: u32 = tar_data[..512].iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        tar_data[148..156].copy_from_slice(checksum_str.as_bytes());

        // Add content "evil" + padding to 512 bytes
        tar_data[512..516].copy_from_slice(b"evil");

        // Compress with gzip
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar_data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();

        let tarball = create_malicious_tarball(b"../evil.txt");

        let tarball_path = tmp.path().join("evil.tar.gz");
        fs::write(&tarball_path, &tarball).unwrap();

        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        let result = extract_tarball(&tarball_path, &dest);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("path traversal"));
    }

    #[test]
    fn rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();

        let tarball = create_malicious_tarball(b"/etc/passwd");

        let tarball_path = tmp.path().join("absolute.tar.gz");
        fs::write(&tarball_path, &tarball).unwrap();

        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        let result = extract_tarball(&tarball_path, &dest);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn normalize_path_removes_dot_components() {
        let path = PathBuf::from("/foo/./bar/./baz");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/foo/bar/baz"));
    }

    #[test]
    fn normalize_path_resolves_parent_dirs() {
        let path = PathBuf::from("/foo/bar/../baz");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/foo/baz"));
    }

    #[test]
    fn normalize_path_handles_multiple_parent_dirs() {
        let path = PathBuf::from("/foo/bar/qux/../../baz");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/foo/baz"));
    }

    #[test]
    fn normalize_path_preserves_leading_parent_dirs_in_relative_paths() {
        let path = PathBuf::from("../foo/bar");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("../foo/bar"));
    }

    #[test]
    fn normalize_path_handles_complex_relative_path() {
        let path = PathBuf::from("foo/./bar/../baz/./qux");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("foo/baz/qux"));
    }

    #[test]
    fn normalize_path_cannot_escape_root() {
        let path = PathBuf::from("/foo/../../etc/passwd");
        let normalized = normalize_path(&path);
        // For absolute paths, .. cannot go above root
        // /foo/../../etc/passwd -> /etc/passwd (the extra .. is dropped)
        assert_eq!(normalized, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn normalize_path_multiple_attempts_to_escape_root() {
        let path = PathBuf::from("/../../../../etc/passwd");
        let normalized = normalize_path(&path);
        // All the .. components above root are dropped
        assert_eq!(normalized, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn normalize_path_root_with_only_parent_dirs() {
        let path = PathBuf::from("/../..");
        let normalized = normalize_path(&path);
        // Should normalize to just root
        assert_eq!(normalized, PathBuf::from("/"));
    }

    #[test]
    fn validate_path_rejects_normalized_escape() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        // Even though this doesn't have explicit .., after normalization it could escape
        // (This is a defense-in-depth test - the .. check should catch real cases)
        let tricky_path = PathBuf::from("foo/../../etc/passwd");

        let result = validate_path(&tricky_path, &dest);
        // Should be rejected by the .. component check
        assert!(result.is_err());
    }

    #[test]
    fn validate_path_accepts_safe_nested_paths() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        let safe_path = PathBuf::from("foo/bar/baz.txt");
        let result = validate_path(&safe_path, &dest);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_accepts_paths_with_dots_in_names() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("extracted");
        fs::create_dir(&dest).unwrap();

        // Files with dots in names should be fine
        let safe_path = PathBuf::from("foo/file.tar.gz");
        let result = validate_path(&safe_path, &dest);
        assert!(result.is_ok());
    }

    #[test]
    fn is_archive_true_for_gzip() {
        let tmp = TempDir::new().unwrap();
        let tarball = create_test_tarball(vec![("a.txt", b"a", None)]);
        let path = tmp.path().join("test.tar.gz");
        fs::write(&path, &tarball).unwrap();
        assert!(is_archive(&path).unwrap());
    }

    #[test]
    fn is_archive_true_for_zip() {
        let tmp = TempDir::new().unwrap();
        let zip_data = create_test_zip(vec![("a.txt", b"a")]);
        let path = tmp.path().join("test.zip");
        fs::write(&path, &zip_data).unwrap();
        assert!(is_archive(&path).unwrap());
    }

    #[test]
    fn is_archive_false_for_raw_binary() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("raw-binary");
        fs::write(&path, b"\x7fELF raw executable bytes").unwrap();
        assert!(!is_archive(&path).unwrap());
    }
}
