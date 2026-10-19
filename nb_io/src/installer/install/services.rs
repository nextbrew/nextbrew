//! The formulae `nb services` manages, as Homebrew's
//! `Services::FormulaWrapper` sees them.

use std::path::PathBuf;

use nb_core::{Error, Formula, formula_token};

use super::Installer;
use crate::services::{Places, ServiceSpec};

/// A formula and what its `service` block amounts to here.
#[derive(Debug, Clone)]
pub struct ServiceFormula {
    pub name: String,
    /// The installed keg, if the formula is installed.
    pub keg: Option<PathBuf>,
    /// Whether the formula has a `service` block at all.
    pub has_service: bool,
    /// The block's command on this OS, from which the plist is generated.
    pub spec: Option<ServiceSpec>,
    /// The service file in the keg under Homebrew's plist name, which a
    /// package ships when its block only names it.
    pub keg_file: Option<PathBuf>,
    /// That file through `opt/`, as listed for a service not registered
    /// (Homebrew's `source_service_file`).
    pub source_file: PathBuf,
}

impl ServiceFormula {
    pub fn installed(&self) -> bool {
        self.keg.is_some()
    }
}

impl Installer {
    /// One formula, installed or not, for `nb services <subcommand> <name>`.
    pub async fn service_formula(&self, name: &str) -> Result<ServiceFormula, Error> {
        let formula = self.api_client.get_formula(name).await?;
        self.wrap_service(formula)
    }

    /// The installed formulae with a `service` block, by name
    /// (`Services::Formulae.available_services`).
    pub async fn service_formulae(&self) -> Result<Vec<ServiceFormula>, Error> {
        let mut services = Vec::new();
        for keg in self.db.list_installed()? {
            if keg.name.starts_with("cask:") {
                continue;
            }
            // A keg whose formula has left the API has no service to manage.
            let Ok(formula) = self.api_client.get_formula(&keg.name).await else {
                continue;
            };
            let service = self.wrap_service(formula)?;
            if service.has_service {
                services.push(service);
            }
        }
        services.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(services)
    }

    fn wrap_service(&self, formula: Formula) -> Result<ServiceFormula, Error> {
        let home =
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| Error::InvalidArgument {
                    message: "HOME is not set; services are registered in the home folder"
                        .to_string(),
                })?;
        let places = Places {
            prefix: self.prefix.clone(),
            home,
        };
        let keg = self
            .db
            .get_installed(&formula.name)
            .map(|keg| {
                self.cellar
                    .keg_path(formula_token(&formula.name), &keg.version)
            })
            .filter(|keg| keg.is_dir());
        let spec = match &formula.service {
            Some(service) => ServiceSpec::from_api(service, &places)?,
            None => None,
        };
        let plist_name = formula
            .service
            .as_ref()
            .and_then(|service| service["name"]["macos"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("sh.brew.{}", formula.name));
        // Older bottles ship the plist under Homebrew's legacy name.
        let names = [plist_name, format!("homebrew.mxcl.{}", formula.name)];
        let found = keg.as_ref().and_then(|keg| {
            names
                .iter()
                .map(|name| format!("{name}.plist"))
                .find(|file| keg.join(file).is_file())
                .map(|file| (keg.join(&file), file))
        });
        let opt = self.prefix.join("opt").join(formula_token(&formula.name));
        let source_file = match &found {
            Some((_, file)) => opt.join(file),
            None => opt.join(format!("{}.plist", names[0])),
        };
        let keg_file = found.map(|(path, _)| path);
        Ok(ServiceFormula {
            name: formula.name,
            keg,
            has_service: formula.service.is_some(),
            spec,
            keg_file,
            source_file,
        })
    }
}
