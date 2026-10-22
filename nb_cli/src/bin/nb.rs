use clap::Parser;
use console::style;
use nb_cli::{
    cli::{Cli, Commands},
    commands,
    init::ensure_init,
    logging,
    ui::Ui,
    utils::{get_prefix_path, get_root_path},
};
use nb_io::create_installer;

#[tokio::main]
async fn main() {
    // An unknown command may be a tap's (`brew <name>`).
    let cli = Cli::try_parse().unwrap_or_else(|e| commands::external::dispatch(e));
    logging::init(cli.verbose, cli.quiet);

    if let Err(e) = run(cli).await {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), nb_core::Error> {
    let mut ui = Ui::new();

    if let Commands::Completion { shell } = cli.command {
        return commands::completion::execute(shell);
    }

    let root = get_root_path(cli.root);
    let prefix = get_prefix_path(cli.prefix, &root);

    if let Commands::Init { no_modify_path } = cli.command {
        return commands::init::execute(&root, &prefix, no_modify_path, &mut ui);
    }

    if !matches!(cli.command, Commands::Reset { .. }) {
        ensure_init(&root, &prefix, cli.auto_init, &mut ui)?;
    }

    let mut installer = create_installer(&root, &prefix, cli.concurrency)?;

    match cli.command {
        Commands::Init { .. } => unreachable!(),
        Commands::Completion { .. } => unreachable!(),
        Commands::Install {
            formulas,
            no_link,
            build_from_source,
            formula,
            cask,
            cask_options,
        } => {
            installer.set_cask_config(cask_config(&cask_options)?);
            // `--cask` names casks; `--formula` keeps bare names as formulae.
            let formulas: Vec<String> = formulas
                .into_iter()
                .map(|name| {
                    if cask && !name.starts_with("cask:") {
                        format!("cask:{name}")
                    } else if formula && !name.starts_with("cask:") && !name.contains('/') {
                        format!("formula:{name}")
                    } else {
                        name
                    }
                })
                .collect();
            commands::tap::prepare_full_names(&installer, &formulas).await?;
            commands::install::execute(
                &mut installer,
                formulas,
                no_link,
                build_from_source,
                &mut ui,
            )
            .await
        }
        Commands::Bundle { install, command } => {
            let command = command.unwrap_or(nb_cli::cli::BundleCommands::Install(install));
            commands::bundle::execute(&mut installer, command, cli.verbose > 0, &mut ui).await
        }
        Commands::Reinstall {
            formulas,
            build_from_source,
            formula,
            cask,
            zap,
            cask_options,
        } => {
            installer.set_cask_config(cask_config(&cask_options)?);
            let formulas: Vec<String> = formulas
                .into_iter()
                .map(|name| {
                    if cask && !name.starts_with("cask:") {
                        format!("cask:{name}")
                    } else if formula && !name.starts_with("cask:") && !name.contains('/') {
                        format!("formula:{name}")
                    } else {
                        name
                    }
                })
                .collect();
            commands::tap::trust_full_names(&installer, &formulas)?;
            commands::reinstall::execute(&mut installer, formulas, build_from_source, zap, &mut ui)
                .await
        }
        Commands::Uninstall {
            formulas,
            all,
            formula,
            cask,
            zap,
            ignore_dependencies,
            force,
        } => {
            // `--cask` names casks; `--formula` keeps names from meaning one.
            let formulas: Vec<String> = formulas
                .into_iter()
                .map(|name| {
                    if cask && !name.starts_with("cask:") {
                        format!("cask:{name}")
                    } else if formula && !name.starts_with("cask:") {
                        format!("formula:{name}")
                    } else {
                        name
                    }
                })
                .collect();
            let result = commands::uninstall::execute(
                &mut installer,
                formulas,
                all,
                zap,
                ignore_dependencies,
                force,
                &mut ui,
            )
            .await;
            // As brew uninstall does, unless turned off; nothing is left after --all.
            if !all && std::env::var_os("NEXTBREW_NO_AUTOREMOVE").is_none() {
                commands::autoremove::after_uninstall(&mut installer, &mut ui).await;
            }
            result
        }
        Commands::Autoremove { dry_run } => {
            commands::autoremove::execute(&mut installer, dry_run, &mut ui).await
        }
        Commands::Migrate { yes, force } => {
            commands::migrate::execute(&mut installer, yes, force, &mut ui).await
        }
        Commands::Doctor { repair } => {
            commands::doctor::execute(&mut installer, repair, &mut ui).await
        }
        Commands::List {
            pinned,
            formula,
            cask,
            versions,
            one,
        } => commands::list::execute(
            &mut installer,
            pinned,
            commands::list::Listing {
                formula,
                cask,
                versions,
                one,
            },
        ),
        Commands::Info {
            formula,
            formula_only,
            cask,
        } => commands::info::execute(&mut installer, formula, formula_only, cask).await,
        Commands::Gc => commands::gc::execute(&mut installer),
        Commands::Update => commands::update::execute(&mut installer).await,
        Commands::Outdated { json } => {
            commands::outdated::execute(&mut installer, cli.quiet, cli.verbose > 0, json).await
        }
        Commands::Upgrade {
            formulas,
            build_from_source,
            no_link,
            cask_options,
        } => {
            installer.set_cask_config(cask_config(&cask_options)?);
            commands::tap::trust_full_names(&installer, &formulas)?;
            commands::upgrade::execute(
                &mut installer,
                formulas,
                build_from_source,
                no_link,
                &mut ui,
            )
            .await
        }
        Commands::Search {
            query,
            formula,
            cask,
        } => commands::search::execute(&installer, query, formula, cask).await,
        Commands::Deps {
            formulas,
            direct,
            union,
        } => commands::deps::deps(&installer, formulas, direct, union).await,
        Commands::Uses {
            formulas,
            installed,
            recursive,
            formula,
            cask,
        } => {
            let scope = nb_io::UsesScope {
                installed,
                recursive,
                formulae: !cask,
                casks: !formula,
            };
            commands::deps::uses(&installer, formulas, scope).await
        }
        Commands::Link {
            formulas,
            force,
            overwrite,
            dry_run,
            formula,
            cask,
        } => {
            let options = commands::link::LinkOptions {
                force,
                overwrite,
                dry_run,
            };
            commands::link::link(&mut installer, formulas, options, formula, cask).await
        }
        Commands::Unlink {
            formulas,
            formula,
            cask,
        } => commands::link::unlink(&mut installer, formulas, formula, cask).await,
        Commands::Pin { formulas } => commands::pin::execute(&installer, formulas, true),
        Commands::Unpin { formulas } => commands::pin::execute(&installer, formulas, false),
        Commands::Tap {
            name,
            url,
            force,
            custom_remote,
        } => {
            if custom_remote && url.is_none() {
                Err(nb_core::Error::InvalidArgument {
                    message: format!(
                        "Tap {} with option `--custom-remote` but without a remote URL.",
                        name.as_deref().unwrap_or_default()
                    ),
                })
            } else {
                commands::tap::tap(&installer, name, url, force, custom_remote).await
            }
        }
        Commands::Untap { names, force } => {
            commands::tap::untap(&mut installer, names, force, &mut ui).await
        }
        Commands::Trust {
            names,
            tap,
            formula,
            cask,
            command,
        } => commands::tap::trust(
            &installer,
            names,
            commands::tap::trust_kind(tap, formula, cask, command),
            commands::tap::TrustAction::Trust,
        ),
        Commands::Untrust {
            names,
            tap,
            formula,
            cask,
            command,
        } => commands::tap::trust(
            &installer,
            names,
            commands::tap::trust_kind(tap, formula, cask, command),
            commands::tap::TrustAction::Untrust,
        ),
        Commands::Services {
            command,
            sudo_service_user,
        } => {
            commands::services::execute_as(&installer, command, cli.verbose > 0, sudo_service_user)
                .await
        }
        Commands::Postinstall { formulas } => {
            commands::postinstall::execute(&mut installer, formulas, &mut ui).await
        }
        Commands::Reset { yes } => commands::reset::execute(&root, &prefix, yes, &mut ui),
        Commands::Run { formula, args } => {
            commands::run::execute(&mut installer, formula, args).await
        }
    }
}

/// `$HOMEBREW_CASK_OPTS` with the options given on the command line on top.
fn cask_config(options: &nb_cli::cli::CaskOptions) -> Result<nb_io::CaskConfig, nb_core::Error> {
    let mut explicit = nb_io::CaskConfig::default();
    for option in options.options() {
        explicit.apply_option(&option, true)?;
    }
    Ok(nb_io::CaskConfig::from_env()?.overlay(&explicit))
}
