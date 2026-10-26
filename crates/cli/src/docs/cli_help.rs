//! The command line's own help, every subcommand's, as one document. It is
//! the authoritative source for the CLI page: rendered from the same
//! definitions that parse the commands, so its flags, positionals,
//! defaults and environment variables are exactly the real ones.

use clap::CommandFactory;

/// The long help of `workforce` and every subcommand under it, each under a
/// `## workforce …` heading, in definition order. Hidden commands are left
/// out, as they are from `--help`.
pub fn cli_help_reference() -> String {
    let mut out = String::new();
    let mut root = crate::Cli::command();
    root.build();
    render(&mut root, "workforce", &mut out);
    out
}

fn render(command: &mut clap::Command, path: &str, out: &mut String) {
    out.push_str(&format!("## {path}\n\n"));
    out.push_str(command.render_long_help().to_string().trim_end());
    out.push_str("\n\n");
    let names: Vec<String> = command
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        if let Some(sub) = command.find_subcommand_mut(&name) {
            render(sub, &format!("{path} {name}"), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subcommand_is_rendered_with_its_real_arguments() {
        let help = cli_help_reference();
        assert!(help.starts_with("## workforce\n"));
        // The forms the curator got wrong in PR #21, as the help states them.
        let shadow = section(&help, "workforce shadow promote");
        assert!(shadow.contains("<STORE>"), "{shadow}");
        assert!(
            !help.contains("## workforce decide"),
            "hidden commands stay off the user-facing page"
        );
        assert!(section(&help, "workforce permissions import-roles").contains("--from-csv"));
        assert!(
            !help.contains("## workforce help"),
            "the help subcommand is noise"
        );
    }

    fn section<'a>(help: &'a str, path: &str) -> &'a str {
        let start = help
            .find(&format!("## {path}\n"))
            .unwrap_or_else(|| panic!("no section for {path}"));
        let rest = &help[start + 3..];
        &rest[..rest.find("\n## ").unwrap_or(rest.len())]
    }
}
