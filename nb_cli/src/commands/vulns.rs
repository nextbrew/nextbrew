use console::style;

/// `nb vulns`: known vulnerabilities in OSV.dev for the installed formulae
/// (or the ones named, at their installed or current version). Fails when
/// any are found, as `brew vulns` does.
pub async fn execute(
    installer: &nb_io::Installer,
    names: Vec<String>,
    deps: bool,
) -> Result<(), nb_core::Error> {
    let mut wanted: Vec<String> = if names.is_empty() {
        installer
            .list_installed()?
            .into_iter()
            .map(|keg| keg.name)
            .filter(|name| !name.starts_with("cask:"))
            .collect()
    } else {
        names
    };
    let mut targets = Vec::new();
    let mut skipped = 0;
    let mut i = 0;
    while i < wanted.len() {
        let json = installer.formula_json(&wanted[i]).await?;
        let name = json["name"].as_str().unwrap_or(&wanted[i]).to_string();
        i += 1;
        if deps {
            for dep in installer.formula(&name).await?.runtime_dependencies() {
                if !wanted.contains(&dep) {
                    wanted.push(dep);
                }
            }
        }
        let version = installer
            .get_installed(&name)
            .map(|keg| keg.version)
            .or_else(|| json["versions"]["stable"].as_str().map(String::from))
            .unwrap_or_default();
        match nb_io::vulns::target(&json, &version) {
            Some(target) => targets.push(target),
            None => skipped += 1,
        }
    }
    println!(
        "{} Checking {} formulae for vulnerabilities...",
        style("==>").cyan().bold(),
        targets.len()
    );
    let found = nb_io::vulns::scan(installer.http(), &targets).await?;
    let mut total = 0;
    for (target, advisories) in targets.iter().zip(&found) {
        if advisories.is_empty() {
            continue;
        }
        total += advisories.len();
        println!(
            "{} {}: {} vulnerabilit{}",
            style(&target.name).bold(),
            target.version,
            advisories.len(),
            if advisories.len() == 1 { "y" } else { "ies" }
        );
        for advisory in advisories {
            println!("  {}: {}", style(&advisory.id).red(), advisory.summary);
        }
    }
    if skipped > 0 {
        println!("Skipped {skipped} formulae whose source is on no forge OSV.dev knows.");
    }
    if total > 0 {
        return Err(nb_core::Error::InvalidArgument {
            message: format!("found {total} known vulnerabilities"),
        });
    }
    println!("No known vulnerabilities found.");
    Ok(())
}
