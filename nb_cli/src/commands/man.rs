use std::fmt::Write as _;
use std::io::IsTerminal;

use clap::CommandFactory;

use crate::cli::Cli;

/// `nb man`: the manual page, made from the command line's own help. To a
/// terminal it opens in `man`; otherwise the roff is printed.
pub fn execute() -> Result<(), nb_core::Error> {
    let page = roff();
    if !std::io::stdout().is_terminal() {
        print!("{page}");
        return Ok(());
    }
    let path = std::env::temp_dir().join("nb.1");
    std::fs::write(&path, &page).map_err(|e| nb_core::Error::FileError {
        message: format!("failed to write {}: {e}", path.display()),
    })?;
    let status = std::process::Command::new("man").arg(&path).status();
    if !status.is_ok_and(|s| s.success()) {
        print!("{page}");
    }
    Ok(())
}

/// The page, in roff.
pub fn roff() -> String {
    let cli = Cli::command();
    let mut page = String::new();
    let _ = writeln!(
        page,
        ".TH NB 1 \"\" \"nextbrew {}\" \"User Commands\"",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(page, ".SH NAME\nnb \\- {}", escape(&about(&cli)));
    let _ = writeln!(
        page,
        ".SH SYNOPSIS\n\\fBnb\\fR [\\fIOPTIONS\\fR] \\fICOMMAND\\fR"
    );
    page.push_str(".SH OPTIONS\n");
    options(&mut page, &cli);
    page.push_str(".SH COMMANDS\n");
    for command in cli.get_subcommands().filter(|c| !c.is_hide_set()) {
        let _ = writeln!(
            page,
            ".SS \"{}\"\n{}",
            command.get_name(),
            escape(&about(command))
        );
        options(&mut page, command);
    }
    page.push_str(".SH SEE ALSO\nhttps://nextbrew.dev/en/docs\n");
    page
}

fn options(page: &mut String, command: &clap::Command) {
    for arg in command.get_arguments().filter(|a| !a.is_hide_set()) {
        let mut names: Vec<String> = Vec::new();
        if let Some(short) = arg.get_short() {
            names.push(format!("\\-{short}"));
        }
        if let Some(long) = arg.get_long() {
            names.push(format!("\\-\\-{long}"));
        }
        if names.is_empty() {
            names.push(format!("<{}>", arg.get_id()));
        }
        let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
        let _ = writeln!(page, ".TP\n\\fB{}\\fR\n{}", names.join(", "), escape(&help));
    }
}

fn about(command: &clap::Command) -> String {
    command
        .get_about()
        .map(|a| a.to_string())
        .unwrap_or_default()
}

/// Backslashes, and a leading `.` or `'`, which roff would read as markup.
fn escape(text: &str) -> String {
    let text = text.replace('\\', "\\e").replace('-', "\\-");
    text.lines()
        .map(|line| {
            if line.starts_with(['.', '\'']) {
                format!("\\&{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_page_lists_the_commands() {
        let page = super::roff();
        assert!(page.starts_with(".TH NB 1"));
        assert!(page.contains(".SS \"install\""));
        assert!(!page.contains("__path"));
    }
}
