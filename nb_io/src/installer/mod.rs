mod cask;
pub mod homebrew;
pub mod install;
pub mod post_install;
pub(crate) mod sudo;

pub use homebrew::{
    HomebrewMigrationPackages, HomebrewPackage, categorize_packages, get_homebrew_packages,
    parse_casks_from_plain_text, parse_formulas_from_json,
};
pub use install::doctor::{DiagnosticReport, RepairSummary};
pub use install::{
    ExecuteResult, InstallPlan, Installer, LinkOutcome, OutdatedPackage, PlanFailure,
    PostInstallNotice, Uses, UsesScope, create_installer,
};
pub use post_install::{PostInstallContext, StepError, StepFailure, run_steps};
