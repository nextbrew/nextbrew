use std::process::Command;

const NEXTBREW_HOME: &str = "https://nextbrew.dev";
const NEXTBREW_REPO: &str = "https://github.com/nextbrew/nextbrew";

/// `nb home`: open the homepages of formulae or casks, or nextbrew's.
pub async fn home(installer: &nb_io::Installer, names: Vec<String>) -> Result<(), nb_core::Error> {
    if names.is_empty() {
        return open(NEXTBREW_HOME);
    }
    for name in names {
        let json = match name.strip_prefix("cask:") {
            Some(token) => installer.cask(token).await?,
            None => match installer.formula_json(&name).await {
                Ok(json) => json,
                // As brew does, a name that is no formula may be a cask.
                Err(e) => installer.cask(&name).await.map_err(|_| e)?,
            },
        };
        let homepage = json["homepage"]
            .as_str()
            .ok_or(nb_core::Error::InvalidArgument {
                message: format!("{name} has no homepage"),
            })?;
        open(homepage)?;
    }
    Ok(())
}

/// `nb source`: open a formula's source repository, or nextbrew's.
pub async fn source(
    installer: &nb_io::Installer,
    name: Option<String>,
) -> Result<(), nb_core::Error> {
    let Some(name) = name else {
        return open(NEXTBREW_REPO);
    };
    let json = installer.formula_json(&name).await?;
    let urls = [
        json["urls"]["head"]["url"].as_str(),
        json["urls"]["stable"]["url"].as_str(),
        json["homepage"].as_str(),
    ];
    let repo = nb_io::vulns::repo_url(&urls).ok_or(nb_core::Error::InvalidArgument {
        message: format!("{name}'s source repository is not on a known forge"),
    })?;
    open(&repo)
}

/// `nb desc`: formulae's descriptions, or the formulae whose name or
/// description matches.
pub async fn desc(
    installer: &nb_io::Installer,
    names: Vec<String>,
    search: bool,
    name: bool,
    description: bool,
) -> Result<(), nb_core::Error> {
    if search || name || description {
        let query = names.join(" ");
        let found = installer
            .describe(&query, search || name, search || description)
            .await?;
        if found.is_empty() {
            return Err(nb_core::Error::InvalidArgument {
                message: format!("No formulae found for {query:?}."),
            });
        }
        for (name, desc) in found {
            print_desc(&name, desc.as_deref());
        }
        return Ok(());
    }
    for name in names {
        let formula = installer.formula(&name).await?;
        print_desc(&formula.name, formula.desc.as_deref());
    }
    Ok(())
}

fn print_desc(name: &str, desc: Option<&str>) {
    println!(
        "{}: {}",
        console::style(name).bold(),
        desc.unwrap_or("(no description)")
    );
}

/// Open `url` in `$BROWSER`, or the system's default browser.
fn open(url: &str) -> Result<(), nb_core::Error> {
    let browser = std::env::var("BROWSER").ok().filter(|b| !b.is_empty());
    let opener = browser.as_deref().unwrap_or(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    match Command::new(opener).arg(url).status() {
        Ok(status) if status.success() => Ok(()),
        // No browser (over SSH, in a container): the URL is still useful.
        _ => {
            println!("{url}");
            Ok(())
        }
    }
}
