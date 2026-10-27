use std::time::Duration;

use console::style;

use super::info::disk_usage_readable;

/// `nb cleanup`: cached downloads older than `--prune` days (120 by
/// default) and unreferenced store entries, as `brew cleanup` reports them.
pub fn cleanup(
    installer: &mut nb_io::Installer,
    prune: Option<String>,
    dry_run: bool,
    scrub: bool,
) -> Result<(), nb_core::Error> {
    let older_than = match prune.as_deref() {
        _ if scrub => None,
        Some("all") => None,
        Some(days) => Some(
            days.parse::<u64>()
                .map_err(|_| nb_core::Error::InvalidArgument {
                    message: format!("--prune takes a number of days or `all`, not `{days}`"),
                })?,
        ),
        None => Some(120),
    }
    .map(|days| Duration::from_secs(days * 24 * 60 * 60));

    let cleanup = installer.cleanup(older_than, dry_run)?;
    let verb = if dry_run { "Would remove" } else { "Removing" };
    for (path, size) in &cleanup.files {
        println!(
            "{verb}: {} ({})",
            path.display(),
            disk_usage_readable(*size)
        );
    }
    for key in &cleanup.store_entries {
        println!("{verb}: store entry {}", &key[..key.len().min(12)]);
    }
    let freed: u64 = cleanup.files.iter().map(|(_, size)| size).sum();
    if freed > 0 {
        let would = if dry_run { "would free" } else { "has freed" };
        println!(
            "{} This operation {would} approximately {} of disk space.",
            style("==>").cyan().bold(),
            disk_usage_readable(freed)
        );
    }
    Ok(())
}

/// `nb fetch`: download bottles or casks into the cache and print where.
pub async fn fetch(
    installer: &nb_io::Installer,
    names: Vec<String>,
    deps: bool,
    cask: bool,
) -> Result<(), nb_core::Error> {
    let names: Vec<String> = names
        .into_iter()
        .map(|name| {
            if cask && !name.starts_with("cask:") {
                format!("cask:{name}")
            } else {
                name
            }
        })
        .collect();
    for (name, path) in installer.fetch(&names, deps).await? {
        match path {
            Some(path) => println!("Downloaded {name}: {}", path.display()),
            None => eprintln!(
                "{} {name} has no bottle for this system; it is built from source on install.",
                style("Warning:").yellow().bold()
            ),
        }
    }
    Ok(())
}
