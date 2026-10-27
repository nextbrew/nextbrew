use crate::ui::StdUi;

/// `nb version-install`: `brew extract` then `brew install`. The formula
/// as homebrew/core had it at `version` goes into the local tap
/// `<user>/versions` as `<name>@<version>`, which is built from source.
pub async fn execute(
    installer: &mut nb_io::Installer,
    name: String,
    version: String,
    ui: &mut StdUi,
) -> Result<(), nb_core::Error> {
    let json = installer.formula_json(&name).await?;
    let name = json["name"].as_str().unwrap_or(&name).to_string();
    let path = json["ruby_source_path"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("Formula/{}/{name}.rb", &name[..1]));
    let versioned = format!("{name}@{version}");
    let (sha, source) =
        nb_io::extract::formula_at(installer.http(), &name, &path, &version, &versioned).await?;

    let user = std::env::var("USER")
        .ok()
        .map(|u| {
            u.to_lowercase()
                .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "")
        })
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "local".to_string());
    let tap = nb_io::tap::TapName::parse(&format!("{user}/versions"))?;
    let dir = tap.path(installer.prefix()).join("Formula");
    let file = dir.join(format!("{versioned}.rb"));
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&file, source))
        .map_err(|e| nb_core::Error::FileError {
            message: format!("failed to write {}: {e}", file.display()),
        })?;
    eprintln!(
        "Extracted {versioned} from homebrew/core@{} to {}",
        &sha[..10],
        file.display()
    );

    let full = vec![format!("{tap}/{versioned}")];
    super::tap::prepare_full_names(installer, &full).await?;
    super::install::execute(installer, full, false, true, ui).await
}
