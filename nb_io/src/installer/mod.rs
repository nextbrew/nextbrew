pub(crate) mod cask;
pub mod cask_config;
pub mod homebrew;
pub mod install;
pub mod post_install;
pub(crate) mod sudo;

pub use cask::{DeprecateDisable, deprecate_disable, for_current_platform, resolve_cask};
pub use homebrew::{
    HomebrewMigrationPackages, HomebrewPackage, categorize_packages, get_homebrew_packages,
};
pub use install::diagnostic::{Finding, SystemEnv};
pub use install::doctor::{DiagnosticReport, RepairSummary};
pub use install::{
    ExecuteResult, Greedy, InstallPlan, Installer, LinkDryRun, LinkOutcome, OutdatedPackage,
    PlanFailure, PostInstallNotice, ServiceFormula, Uses, UsesScope, create_installer,
};
pub use post_install::{PostInstallContext, StepError, StepFailure, run_steps};
