use std::path::{Path, PathBuf};

use nb_core::Error;
use tokio::fs;

use crate::checksum::verify_sha256_bytes;
use crate::extraction::extract_tarball;

/// Fetch the source from `url`, else each of `mirrors` in turn (as
/// Homebrew's download strategies do), check it, and unpack it.
pub async fn download_and_extract_source(
    url: &str,
    mirrors: &[String],
    expected_checksum: Option<&str>,
    work_dir: &Path,
) -> Result<PathBuf, Error> {
    let tarball_path = work_dir.join("source.tar.gz");
    let mut result = download_source(url, &tarball_path).await;
    for mirror in mirrors {
        if result.is_ok() {
            break;
        }
        result = download_source(mirror, &tarball_path).await;
    }
    result?;

    verify_checksum(&tarball_path, expected_checksum, url).await?;

    let src_dir = work_dir.join("src");
    fs::create_dir_all(&src_dir)
        .await
        .map_err(Error::file("failed to create source directory"))?;

    unpack(&tarball_path, &src_dir).await?;

    find_source_root(&src_dir).await
}

/// `GitDownloadStrategy`'s checkout, made where the build runs: cloned
/// at the tag (or whole, for a commit), checked out, the tag checked to
/// be the commit the formula says, and the submodules brought in. The
/// checkout keeps its `.git`, as Homebrew copies it into the build.
pub async fn clone_git_source(
    url: &str,
    git: &nb_core::GitRef,
    work_dir: &Path,
) -> Result<PathBuf, Error> {
    let src_dir = work_dir.join("src");
    let mut clone: Vec<&str> = vec!["clone"];
    if let Some(tag) = &git.tag {
        clone.extend(["--branch", tag]);
    }
    let src = src_dir.display().to_string();
    clone.extend([
        "--config",
        "advice.detachedHead=false",
        "--config",
        "core.fsmonitor=false",
        "--end-of-options",
        url,
        &src,
    ]);
    git_command(work_dir, &clone).await?;
    let Some(reference) = git.tag.as_ref().or(git.revision.as_ref()) else {
        return Err(Error::ExecutionError {
            message: format!("{url} names no tag or revision to check out"),
        });
    };
    git_command(&src_dir, &["checkout", "-f", reference, "--"]).await?;
    if let (Some(tag), Some(revision)) = (&git.tag, &git.revision) {
        let head = git_command(&src_dir, &["rev-parse", "-q", "--verify", "HEAD"]).await?;
        if head.trim() != revision {
            return Err(Error::ExecutionError {
                message: format!(
                    "{tag} tag should be {revision}\nbut is actually {}",
                    head.trim()
                ),
            });
        }
    }
    if src_dir.join(".gitmodules").exists() {
        git_command(
            &src_dir,
            &["submodule", "foreach", "--recursive", "git submodule sync"],
        )
        .await?;
        git_command(&src_dir, &["submodule", "update", "--init", "--recursive"]).await?;
    }
    Ok(src_dir)
}

async fn git_command(dir: &Path, args: &[&str]) -> Result<String, Error> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(Error::file("failed to run git"))?;
    if !output.status.success() {
        return Err(Error::NetworkFailure {
            message: format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `source_modified_time`, which Homebrew gives the build as
/// `SOURCE_DATE_EPOCH`: a git checkout's commit time, else the newest file
/// as unpacked.
pub async fn source_modified_time(root: &Path) -> Option<i64> {
    if root.join(".git").exists() {
        let time = git_command(root, &["show", "-s", "--format=%ct", "HEAD"])
            .await
            .ok()?;
        return time.trim().parse().ok();
    }
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || newest_file_time(&root))
        .await
        .ok()
        .flatten()
}

fn newest_file_time(dir: &Path) -> Option<i64> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        // As `Pathname#find` and `#file?` see it: links followed to files,
        // not into directories.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let time = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            newest_file_time(&path)
        } else if meta.is_file() {
            meta.modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|time| time.as_secs() as i64)
        } else {
            None
        };
        newest = newest.max(time);
    }
    newest
}

/// `UnpackStrategy`'s as Homebrew picks it for a source: a zip with our
/// own unzip, anything else with the system's `tar`, which reads every
/// compression a source comes in (`.tar.bz2`, `.tar.lz` and the like) and
/// names the files as Homebrew's do.
async fn unpack(archive: &Path, dir: &Path) -> Result<(), Error> {
    let zip = fs::read(archive)
        .await
        .map(|bytes| bytes.starts_with(b"PK\x03\x04"))
        .unwrap_or(false);
    if zip {
        return extract_tarball(archive, dir);
    }
    let output = tokio::process::Command::new("tar")
        .args(["--extract", "--no-same-owner", "--file"])
        .arg(archive)
        .arg("--directory")
        .arg(dir)
        .output()
        .await
        .map_err(Error::exec("failed to run tar"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::ExecutionError {
        message: format!(
            "failed to unpack the source: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    })
}

/// The formula's own `mirror`s for its source, from its file: those at
/// the top of the class or of its `stable do` block, which a resource's or
/// a patch's are not.
pub fn source_mirrors(formula_rb: &str) -> Vec<String> {
    let mut in_stable = false;
    let mut mirrors = Vec::new();
    for line in formula_rb.lines() {
        match line {
            "  stable do" => in_stable = true,
            "  end" => in_stable = false,
            _ => {}
        }
        let indent = if in_stable { "    " } else { "  " };
        if let Some(url) = line
            .strip_prefix(indent)
            .and_then(|rest| rest.strip_prefix("mirror \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .filter(|url| !url.contains("#{"))
        {
            mirrors.push(url.to_string());
        }
    }
    mirrors
}

async fn download_source(url: &str, dest: &Path) -> Result<(), Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .use_preconfigured_tls((*crate::network::tls::shared_tls_config()).clone())
        .build()
        .map_err(Error::network("failed to create HTTP client"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(Error::network("failed to download source"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::NetworkFailure {
            message: format!("source download returned HTTP {status}"),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(Error::network("failed to read source response"))?;

    fs::write(dest, &bytes)
        .await
        .map_err(Error::file("failed to write source tarball"))
}

async fn verify_checksum(path: &Path, expected: Option<&str>, url: &str) -> Result<(), Error> {
    let bytes = fs::read(path)
        .await
        .map_err(Error::file("failed to read tarball for checksum"))?;

    verify_sha256_bytes(&bytes, expected).map_err(|e| match e {
        Error::ChecksumMismatch { .. } => e,
        Error::InvalidArgument { message } => Error::InvalidArgument {
            message: format!("invalid source checksum for '{url}': {message}"),
        },
        other => other,
    })
}

async fn find_source_root(src_dir: &Path) -> Result<PathBuf, Error> {
    let mut entries = fs::read_dir(src_dir)
        .await
        .map_err(Error::file("failed to read source directory"))?;

    let mut subdirs = Vec::new();
    let mut has_files = false;

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(Error::file("failed to read directory entry"))?
    {
        let ft = entry
            .file_type()
            .await
            .map_err(Error::file("failed to get file type"))?;
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else {
            has_files = true;
        }
    }

    if subdirs.len() == 1 && !has_files {
        return Ok(subdirs.into_iter().next().unwrap());
    }

    Ok(src_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_COMMITTER_DATE", "2020-01-02T03:04:05Z")
            .env("GIT_AUTHOR_DATE", "2020-01-02T03:04:05Z")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn a_git_source_is_checked_out_at_its_tag_and_checked_against_its_commit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a"), "1").unwrap();
        git(&repo, &["add", "a"]);
        git(&repo, &["commit", "-q", "-m", "one"]);
        git(&repo, &["tag", "v1"]);
        let v1 = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("a"), "2").unwrap();
        git(&repo, &["commit", "-q", "-am", "two"]);
        let url = format!("file://{}", repo.display());

        let tagged = nb_core::GitRef {
            tag: Some("v1".into()),
            revision: Some(v1.clone()),
        };
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let src = clone_git_source(&url, &tagged, &work).await.unwrap();
        assert_eq!(std::fs::read_to_string(src.join("a")).unwrap(), "1");
        assert!(src.join(".git").is_dir());
        assert_eq!(source_modified_time(&src).await, Some(1577934245));

        let wrong = nb_core::GitRef {
            tag: Some("v1".into()),
            revision: Some("0".repeat(40)),
        };
        let work = tmp.path().join("work2");
        std::fs::create_dir(&work).unwrap();
        let err = clone_git_source(&url, &wrong, &work).await.unwrap_err();
        assert!(err.to_string().contains("v1 tag should be"), "{err}");
    }

    #[tokio::test]
    async fn an_archives_time_is_its_newest_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("dir/new");
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::write(&old, "").unwrap();
        std::fs::write(&new, "").unwrap();
        let at = |secs| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(at(100))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&new)
            .unwrap()
            .set_modified(at(200))
            .unwrap();
        assert_eq!(source_modified_time(tmp.path()).await, Some(200));
    }

    #[test]
    fn mirrors_are_the_sources_own() {
        let rb = r#"class Ncurses < Formula
  url "https://invisible-mirror.net/archives/ncurses/ncurses-6.6.tar.gz"
  mirror "https://ftpmirror.gnu.org/ncurses/ncurses-6.6.tar.gz"
  mirror "https://ftp.gnu.org/gnu/ncurses/ncurses-#{version}.tar.gz"

  resource "x" do
    url "https://example.com/x.tar.gz"
    mirror "https://example.org/x.tar.gz"
  end
end
"#;
        assert_eq!(
            source_mirrors(rb),
            ["https://ftpmirror.gnu.org/ncurses/ncurses-6.6.tar.gz"]
        );
    }

    #[test]
    fn a_stable_blocks_mirrors_count() {
        let rb = r#"class Wget < Formula
  stable do
    url "https://ftpmirror.gnu.org/wget/wget-1.25.0.tar.gz"
    mirror "https://ftp.gnu.org/gnu/wget/wget-1.25.0.tar.gz"

    patch do
      url "https://example.com/x.diff"
    end
  end
end
"#;
        assert_eq!(
            source_mirrors(rb),
            ["https://ftp.gnu.org/gnu/wget/wget-1.25.0.tar.gz"]
        );
    }
}
