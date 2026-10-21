use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use nb_core::{BuildPlan, Error};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::environment::{build_env, logs_dir};
use super::source::download_and_extract_source;

const SHIM_RUBY: &str = include_str!("shim.rb");

pub struct BuildExecutor {
    prefix: PathBuf,
    work_root: PathBuf,
}

impl BuildExecutor {
    pub fn new(prefix: PathBuf) -> Self {
        let work_root = prefix.join("tmp").join("build");
        Self { prefix, work_root }
    }

    pub async fn execute(
        &self,
        plan: &BuildPlan,
        formula_rb_path: &Path,
        installed_deps: &HashMap<String, DepInfo>,
    ) -> Result<(), Error> {
        let work_dir = self.work_root.join(&plan.formula_name);
        self.prepare_work_dir(&work_dir).await?;

        let source_root = download_and_extract_source(
            &plan.source_url,
            plan.source_checksum.as_deref(),
            &work_dir,
        )
        .await?;

        let shim_path = work_dir.join("nextbrew_shim.rb");
        fs::write(&shim_path, SHIM_RUBY)
            .await
            .map_err(Error::file("failed to write ruby shim"))?;

        fs::create_dir_all(&plan.cellar_path)
            .await
            .map_err(Error::file("failed to create cellar directory"))?;

        // A home of the build's own, as `Formula#stage` makes one.
        let home = source_root.join(".brew_home");
        fs::create_dir_all(&home)
            .await
            .map_err(Error::file("failed to create the build's home"))?;
        let _ = fs::write(home.join(".gitignore"), "*").await;
        let _ = fs::write(
            home.join(".bazelrc"),
            format!("startup --output_user_root={}/_bazel", home.display()),
        )
        .await;

        // The formula's logs, each build's alone.
        let logs = logs_dir(&plan.formula_name);
        let _ = fs::remove_dir_all(&logs).await;
        let failed = work_dir.join("build-failed");

        let mut env = build_env(plan, &self.prefix, &source_root, &logs);
        env.insert("NEXTBREW_BUILD_FAILED".into(), failed.display().to_string());
        env.insert(
            "NEXTBREW_FORMULA_FILE".into(),
            formula_rb_path.display().to_string(),
        );

        let deps_json = serde_json::to_string(installed_deps).unwrap_or_else(|_| "{}".into());
        env.insert("NEXTBREW_INSTALLED_DEPS".into(), deps_json);

        let ruby = find_ruby().await?;
        if run_build(&ruby, &shim_path, &source_root, &env)
            .await?
            .is_some()
        {
            // A failed command has shown its log; anything else, its error.
            self.cleanup_work_dir(&work_dir).await;
            return Err(Error::BuildFailed {
                name: plan.formula_name.clone(),
                version: plan.version.clone(),
                report: None,
            });
        }

        self.cleanup_work_dir(&work_dir).await;
        Ok(())
    }

    async fn prepare_work_dir(&self, work_dir: &Path) -> Result<(), Error> {
        if work_dir.exists() {
            let _ = fs::remove_dir_all(work_dir).await;
        }
        fs::create_dir_all(work_dir)
            .await
            .map_err(Error::file("failed to create work directory"))
    }

    async fn cleanup_work_dir(&self, work_dir: &Path) {
        let _ = fs::remove_dir_all(work_dir).await;
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DepInfo {
    pub cellar_path: String,
}

/// The `ruby` on the user's `PATH`, else the system's, by its full path as
/// the build's `PATH` is not the user's.
async fn find_ruby() -> Result<PathBuf, Error> {
    let on_path = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("ruby"))
                .find(|ruby| ruby.is_file())
        })
        .unwrap_or_default();
    for candidate in on_path.into_iter().chain([PathBuf::from("/usr/bin/ruby")]) {
        let result = Command::new(&candidate).arg("--version").output().await;

        if let Ok(output) = result
            && output.status.success()
        {
            return Ok(candidate);
        }
    }

    Err(Error::ExecutionError {
        message: "ruby not found — required for building from source".into(),
    })
}

/// Run the build, showing what it says. If it failed, which it has said
/// why, the last lines it wrote to standard error.
async fn run_build(
    ruby: &Path,
    shim_path: &Path,
    source_root: &Path,
    env: &HashMap<String, String>,
) -> Result<Option<Vec<String>>, Error> {
    let mut child = Command::new(ruby)
        .arg(shim_path)
        .current_dir(source_root)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::exec("failed to execute ruby shim"))?;

    let stdout = child.stdout.take().ok_or_else(|| Error::ExecutionError {
        message: "failed to capture ruby shim stdout".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| Error::ExecutionError {
        message: "failed to capture ruby shim stderr".to_string(),
    })?;

    let stdout_task = tokio::spawn(stream_output(stdout, false));
    let stderr_task = tokio::spawn(stream_output(stderr, true));

    let status = child
        .wait()
        .await
        .map_err(Error::exec("failed waiting for ruby shim"))?;

    stdout_task
        .await
        .map_err(Error::exec("failed to join stdout task"))?
        .map_err(Error::exec("failed reading stdout"))?;
    let stderr_tail = stderr_task
        .await
        .map_err(Error::exec("failed to join stderr task"))?
        .map_err(Error::exec("failed reading stderr"))?;

    Ok((!status.success()).then_some(stderr_tail))
}

/// Show what `reader` says, keeping its last lines.
async fn stream_output<R>(reader: R, stderr: bool) -> Result<Vec<String>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    const TAIL_LINES: usize = 40;
    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
    Ok(tail.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_build_supports_mv_in_formula_install() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(source_root.join("themes")).unwrap();
        std::fs::write(source_root.join("themes/default.omp.json"), "{}").unwrap();

        let shim_path = tmp.path().join("shim.rb");
        std::fs::write(&shim_path, SHIM_RUBY).unwrap();

        let formula_path = tmp.path().join("foo.rb");
        std::fs::write(
            &formula_path,
            r#"
class Foo < Formula
  def install
    mv "themes", prefix
  end
end
"#,
        )
        .unwrap();

        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        std::fs::create_dir_all(&cellar).unwrap();

        let env = shim_env(&prefix, &cellar, &formula_path);

        assert_eq!(
            run_build(&ruby, &shim_path, &source_root, &env)
                .await
                .unwrap(),
            None
        );

        assert!(
            prefix
                .join("Cellar")
                .join("foo")
                .join("1.0.0")
                .join("themes")
                .join("default.omp.json")
                .exists()
        );
    }

    /// What the executor gives the shim, for a build of foo 1.0.0.
    fn shim_env(prefix: &Path, cellar: &Path, formula_path: &Path) -> HashMap<String, String> {
        let tmp = formula_path.parent().unwrap();
        let mut env = HashMap::new();
        env.insert(
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        );
        env.insert("NEXTBREW_PREFIX".to_string(), prefix.display().to_string());
        env.insert("NEXTBREW_CELLAR".to_string(), cellar.display().to_string());
        env.insert("NEXTBREW_FORMULA_NAME".to_string(), "foo".to_string());
        env.insert("NEXTBREW_FORMULA_VERSION".to_string(), "1.0.0".to_string());
        env.insert(
            "NEXTBREW_FORMULA_FILE".to_string(),
            formula_path.display().to_string(),
        );
        env.insert("NEXTBREW_INSTALLED_DEPS".to_string(), "{}".to_string());
        env.insert(
            "NEXTBREW_LOGS".to_string(),
            tmp.join("logs").display().to_string(),
        );
        env.insert(
            "NEXTBREW_BUILD_FAILED".to_string(),
            tmp.join("build-failed").display().to_string(),
        );
        env
    }

    #[tokio::test]
    async fn a_failed_command_shows_its_log_and_stops_the_build() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let shim_path = tmp.path().join("shim.rb");
        std::fs::write(&shim_path, SHIM_RUBY).unwrap();

        let formula_path = tmp.path().join("foo.rb");
        std::fs::write(
            &formula_path,
            r#"
class Foo < Formula
  def install
    system "sh", "-c", "echo boom-from-stderr 1>&2; exit 7"
  end
end
"#,
        )
        .unwrap();

        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        std::fs::create_dir_all(&cellar).unwrap();

        let env = shim_env(&prefix, &cellar, &formula_path);

        let failed = run_build(&ruby, &shim_path, &source_root, &env)
            .await
            .unwrap();
        // The command's output went to its log, not to the terminal.
        assert_eq!(failed, Some(Vec::new()));
        assert!(tmp.path().join("build-failed").exists());
        let log = std::fs::read_to_string(tmp.path().join("logs/01.sh.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines[1..],
            [
                "",
                "sh",
                "-c",
                "echo boom-from-stderr 1>&2; exit 7",
                "",
                "boom-from-stderr"
            ]
        );
    }

    fn sha256_hex_file(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            })
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.canonicalize().unwrap().display())
    }

    async fn run_shim_formula(
        ruby: &Path,
        source_root: &Path,
        prefix: &Path,
        formula_rb: &str,
    ) -> Result<(), Error> {
        let tmp = source_root.parent().unwrap();
        let shim_path = tmp.join("shim.rb");
        std::fs::write(&shim_path, SHIM_RUBY).unwrap();

        let formula_path = tmp.join("formula.rb");
        std::fs::write(&formula_path, formula_rb).unwrap();

        let cellar = prefix.join("Cellar");
        std::fs::create_dir_all(&cellar).unwrap();

        let env = shim_env(prefix, &cellar, &formula_path);

        match run_build(ruby, &shim_path, source_root, &env).await? {
            None => Ok(()),
            Some(tail) => Err(Error::ExecutionError {
                message: tail.join("\n"),
            }),
        }
    }

    /// A keg built in `source` by `install`, as `(file, contents)` sorted.
    async fn build_keg(source: &[(&str, &str)], install: &str) -> Option<Vec<(String, String)>> {
        let ruby = find_ruby().await.ok()?;
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();
        for (file, contents) in source {
            std::fs::write(source_root.join(file), contents).unwrap();
        }
        let prefix = tmp.path().join("prefix");
        let formula = format!("class Foo < Formula\n  def install\n{install}\n  end\nend\n");
        run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap();
        let keg = prefix.join("Cellar/foo/1.0.0");
        let mut files: Vec<(String, String)> = walkdir::WalkDir::new(&keg)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let name = entry.path().strip_prefix(&keg).unwrap();
                let contents = std::fs::read_to_string(entry.path()).unwrap_or_default();
                (name.display().to_string(), contents)
            })
            .collect();
        files.sort();
        Some(files)
    }

    /// As Homebrew's build: licences and readmes left in the source go into
    /// the keg; other files, empty ones and those installed already don't.
    #[tokio::test]
    async fn run_build_installs_metafiles() {
        let Some(files) = build_keg(
            &[
                ("LICENSE.txt", "licence"),
                ("README.md", "readme"),
                ("NOTICE", ""),
                ("CHANGELOG", "changes"),
                ("Makefile", "all:"),
                ("foo", "binary"),
            ],
            r#"    bin.install "foo"
    doc.install "CHANGELOG""#,
        )
        .await
        else {
            return;
        };
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "LICENSE.txt",
                "README.md",
                "bin/foo",
                "share/doc/foo/CHANGELOG"
            ]
        );
    }

    /// Homebrew's `std_go_args`, `ENV.deparallelize` and
    /// `generate_completions_from_executable`.
    #[tokio::test]
    async fn run_build_has_homebrew_build_helpers() {
        let Some(files) = build_keg(
            &[("tool", "#!/bin/sh\necho \"complete $*\"\n")],
            r#"    ENV["MAKEFLAGS"] = "-j8"
    ENV.deparallelize
    (prefix/"env").write ENV.fetch("MAKEFLAGS", "unset")
    (prefix/"args").write std_go_args(ldflags: "-X main.v=1").join("|")
    (prefix/"args2").write std_go_args(output: bin/"x", tags: %w[a b], ldflags: %w[-X y]).join("|")
    chmod 0755, "tool"
    generate_completions_from_executable("./tool", "completion")
    generate_completions_from_executable("./tool", base_name: "other", shell_parameter_format: :flag, shells: [:zsh])"#,
        )
        .await
        else {
            return;
        };
        let get = |name: &str| {
            files
                .iter()
                .find(|(file, _)| file == name)
                .map(|(_, contents)| contents.replace('\n', ""))
        };
        assert_eq!(get("env").as_deref(), Some("unset"));
        let args = get("args").unwrap();
        assert!(
            args.starts_with("-trimpath|-o=")
                && args.ends_with("/bin/foo|-ldflags=-s -w -X main.v=1"),
            "{args}"
        );
        let args2 = get("args2").unwrap();
        assert!(
            args2.ends_with("/bin/x|-tags=a,b|-ldflags=-s -w -X y"),
            "{args2}"
        );
        assert_eq!(
            get("etc/bash_completion.d/foo").as_deref(),
            Some("complete completion bash")
        );
        assert_eq!(
            get("share/zsh/site-functions/_foo").as_deref(),
            Some("complete completion zsh")
        );
        assert_eq!(
            get("share/fish/vendor_completions.d/foo.fish").as_deref(),
            Some("complete completion fish")
        );
        assert_eq!(
            get("share/zsh/site-functions/_other").as_deref(),
            Some("complete --zsh")
        );
    }

    #[tokio::test]
    async fn run_build_rejects_resource_checksum_mismatch() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let resource_dir = tmp.path().join("resource_content");
        std::fs::create_dir_all(&resource_dir).unwrap();
        std::fs::write(resource_dir.join("file.txt"), "resource-bytes").unwrap();

        let archive = tmp.path().join("resource.tar.gz");
        std::process::Command::new("tar")
            .args([
                "czf",
                archive.to_str().unwrap(),
                "-C",
                tmp.path().to_str().unwrap(),
                "resource_content",
            ])
            .status()
            .unwrap()
            .success()
            .then_some(())
            .expect("tar failed to create resource archive");

        let resource_url = file_url(&archive);
        let formula = format!(
            r#"
class Foo < Formula
  resource "dep" do
    url "{resource_url}"
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  end

  def install
    resource("dep").stage do |_r|
      File.write(prefix + "done", "ok")
    end
  end
end
"#
        );

        let prefix = tmp.path().join("prefix");
        let err = run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[tokio::test]
    async fn run_build_accepts_matching_resource_checksum() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let resource_dir = tmp.path().join("resource_content");
        std::fs::create_dir_all(&resource_dir).unwrap();
        std::fs::write(resource_dir.join("file.txt"), "resource-bytes").unwrap();

        let archive = tmp.path().join("resource.tar.gz");
        std::process::Command::new("tar")
            .args([
                "czf",
                archive.to_str().unwrap(),
                "-C",
                tmp.path().to_str().unwrap(),
                "resource_content",
            ])
            .status()
            .unwrap()
            .success()
            .then_some(())
            .expect("tar failed to create resource archive");

        let resource_url = file_url(&archive);
        let resource_sha = sha256_hex_file(&archive);
        let formula = format!(
            r#"
class Foo < Formula
  resource "dep" do
    url "{resource_url}"
    sha256 "{resource_sha}"
  end

  def install
    resource("dep").stage do |_r|
      File.write(prefix + "done", "ok")
    end
  end
end
"#
        );

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap();

        assert!(
            prefix
                .join("Cellar")
                .join("foo")
                .join("1.0.0")
                .join("done")
                .exists()
        );
    }

    #[tokio::test]
    async fn run_build_rejects_patch_checksum_mismatch() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("hello.txt"), "hello\n").unwrap();

        let patch = tmp.path().join("change.patch");
        std::fs::write(
            &patch,
            "--- a/hello.txt\n+++ b/hello.txt\n@@ -1 +1 @@\n-hello\n+world\n",
        )
        .unwrap();

        let patch_url = file_url(&patch);
        let formula = format!(
            r#"
class Foo < Formula
  patch :p0 do
    url "{patch_url}"
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  end

  def install
    File.write(prefix + "done", File.read("hello.txt"))
  end
end
"#
        );

        let prefix = tmp.path().join("prefix");
        let err = run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"));
    }
}
