use std::fs;

use nb_core::{Error, Formula};

use super::Installer;
use crate::installer::post_install::{PostInstallContext, run_steps};

impl Installer {
    fn post_install_context(&self, formula: &Formula) -> PostInstallContext {
        PostInstallContext {
            prefix: self.prefix.clone(),
            cellar: self.cellar.dir().to_path_buf(),
            name: formula.name.clone(),
            version: formula.versions.stable.clone(),
            keg: self
                .cellar
                .keg_path(&formula.name, &formula.effective_version()),
            temp: self.prefix.join("tmp"),
            cache: self.cache_dir.join("post-install"),
            cask: None,
        }
    }

    /// Run a formula's post-install steps against its installed keg.
    /// Returns the messages of its `warn` steps.
    pub(super) async fn run_post_install(&self, formula: &Formula) -> Result<Vec<String>, Error> {
        if formula.post_install_steps.is_empty() {
            return Ok(Vec::new());
        }
        let ctx = self.post_install_context(formula);
        fs::create_dir_all(&ctx.temp).map_err(Error::file("failed to create tmp directory"))?;
        let steps = formula.post_install_steps.clone();
        let name = formula.name.clone();
        tokio::task::spawn_blocking(move || {
            run_steps(&ctx, &steps).map_err(|failure| Error::PostInstallFailed {
                name,
                step: failure.kind,
                message: failure.error.to_string(),
            })
        })
        .await
        .map_err(Error::exec("post-install task failed"))?
    }

    /// Run the post-install steps of an installed formula again, as
    /// `brew postinstall` does. The steps come from the current formula
    /// definition and run against the installed version.
    pub async fn post_install(&mut self, name: &str) -> Result<Vec<String>, Error> {
        let installed = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;
        let mut formula = self.api_client.get_formula(name).await?;
        if formula.effective_version() != installed.version {
            // Point the steps at the keg that is actually there.
            let (stable, revision) = match installed.version.rsplit_once('_') {
                Some((stable, revision)) if revision.chars().all(|c| c.is_ascii_digit()) => {
                    (stable.to_string(), revision.parse().unwrap_or(0))
                }
                _ => (installed.version.clone(), 0),
            };
            formula.versions.stable = stable;
            formula.revision = revision;
        }
        self.run_post_install(&formula).await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::test_support::formula_json_with;

    #[tokio::test]
    async fn post_install_reruns_steps_for_an_installed_formula() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;

        let steps = r#"[{"type": "mkdir_p", "path": {"path": "{{HOMEBREW_PREFIX}}/var/made-by/{{formula_name}}-{{version}}"}}]"#;
        Mock::given(method("GET"))
            .and(path("/formula/thing.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json_with(
                    &mock_server.uri(),
                    "thing",
                    "2.0.0",
                    "deadbeef",
                    &format!(r#""post_install_steps": {steps}"#),
                )),
            )
            .mount(&mock_server)
            .await;
        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("thing", "2.0.0", "deadbeef").unwrap();
            tx.commit().unwrap();
        }
        fs::create_dir_all(installer.cellar.keg_path("thing", "2.0.0")).unwrap();

        let notices = installer.post_install("thing").await.unwrap();
        assert!(notices.is_empty());
        assert!(prefix.join("var/made-by/thing-2.0.0").is_dir());
    }

    #[tokio::test]
    async fn post_install_uses_the_installed_version_when_the_formula_moved_on() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;

        let steps =
            r#"[{"type": "touch", "path": {"base": "prefix", "path": "touched-{{version}}"}}]"#;
        Mock::given(method("GET"))
            .and(path("/formula/thing.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(formula_json_with(
                    &mock_server.uri(),
                    "thing",
                    "3.0.0",
                    "cafe",
                    &format!(r#""post_install_steps": {steps}"#),
                )),
            )
            .mount(&mock_server)
            .await;
        {
            let tx = installer.db.transaction().unwrap();
            tx.record_install("thing", "2.0.0_1", "deadbeef").unwrap();
            tx.commit().unwrap();
        }
        let keg = installer.cellar.keg_path("thing", "2.0.0_1");
        fs::create_dir_all(&keg).unwrap();

        installer.post_install("thing").await.unwrap();
        assert!(keg.join("touched-2.0.0").is_file());
        assert!(!prefix.join("Cellar/thing/3.0.0").exists());
    }

    #[tokio::test]
    async fn post_install_requires_an_installed_formula() {
        let mock_server = MockServer::start().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let (mut installer, _root, _prefix) =
            super::super::tests::bare_installer(&mock_server, &tmp).await;
        let err = installer.post_install("ghost").await.unwrap_err();
        assert!(matches!(err, nb_core::Error::NotInstalled { .. }));
    }
}
