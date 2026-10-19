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
    let cli = Cli::parse();
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
        } => {
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
        Commands::Uninstall { formulas, all, zap } => {
            let result = commands::uninstall::execute(&mut installer, formulas, all, zap, &mut ui);
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
        Commands::Doctor { repair } => commands::doctor::execute(&mut installer, repair, &mut ui),
        Commands::List { pinned } => commands::list::execute(&mut installer, pinned),
        Commands::Info { formula } => commands::info::execute(&mut installer, formula),
        Commands::Gc => commands::gc::execute(&mut installer),
        Commands::Update => commands::update::execute(&mut installer).await,
        Commands::Outdated { json } => {
            commands::outdated::execute(&mut installer, cli.quiet, cli.verbose > 0, json).await
        }
        Commands::Upgrade {
            formulas,
            build_from_source,
            no_link,
        } => {
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
        Commands::Link { formulas, force } => {
            commands::link::link(&mut installer, formulas, force).await
        }
        Commands::Unlink { formulas } => commands::link::unlink(&mut installer, formulas),
        Commands::Pin { formulas } => commands::pin::execute(&installer, formulas, true),
        Commands::Unpin { formulas } => commands::pin::execute(&installer, formulas, false),
        Commands::Services { command } => {
            commands::services::execute(&installer, command, cli.verbose > 0).await
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
