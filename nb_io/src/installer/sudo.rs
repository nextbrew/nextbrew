//! Root for one command at a time, as Homebrew does it: nextbrew itself
//! runs as the user, and only the command that needs root goes through
//! `sudo`, which asks for the password on the terminal (or through
//! `SUDO_ASKPASS`).

use std::ffi::OsStr;
use std::process::Command;

/// `sudo [-A] -E -- program`, announced so the user knows what a password
/// prompt is for.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    eprintln!(
        "==> Running {} with sudo; the password may be necessary.",
        program.to_string_lossy()
    );
    let mut cmd = Command::new("/usr/bin/sudo");
    if std::env::var_os("SUDO_ASKPASS").is_some() {
        cmd.arg("-A");
    }
    cmd.args(["-E", "--"]).arg(program);
    cmd
}

#[cfg(test)]
mod tests {
    #[test]
    fn wraps_the_program() {
        let cmd = super::command("/usr/sbin/installer");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(cmd.get_program(), "/usr/bin/sudo");
        assert_eq!(args[args.len() - 3..], ["-E", "--", "/usr/sbin/installer"]);
    }
}
