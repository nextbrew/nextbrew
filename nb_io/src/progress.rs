/// Progress events during installation
#[derive(Debug, Clone)]
pub enum InstallProgress {
    /// Starting to download a package (with total size if known)
    DownloadStarted {
        name: String,
        total_bytes: Option<u64>,
    },
    /// Download progress update
    DownloadProgress {
        name: String,
        downloaded: u64,
        total_bytes: Option<u64>,
    },
    /// Download completed for a package
    DownloadCompleted { name: String, total_bytes: u64 },
    /// Starting to unpack/materialize a package
    UnpackStarted { name: String },
    /// Unpacking completed for a package
    UnpackCompleted { name: String },
    /// Starting to link a package
    LinkStarted { name: String },
    /// Linking completed for a package
    LinkCompleted { name: String },
    /// Linking skipped (keg-only or conflict)
    LinkSkipped { name: String, reason: String },
    /// Installation completed for a package (final state)
    InstallCompleted { name: String },
    /// Running the package's post-install steps, after every package in
    /// the plan is installed and linked
    PostInstallStarted { name: String },
    /// Post-install steps completed
    PostInstallCompleted { name: String },
    /// A post-install step failed; the package stays installed
    PostInstallFailed { name: String, message: String },
}

/// Callback type for progress reporting
pub type ProgressCallback = Box<dyn Fn(InstallProgress) + Send + Sync>;
