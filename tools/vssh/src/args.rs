use std::ffi::OsString;

use anyhow::{bail, Result};

#[derive(Debug)]
pub struct Cli {
    pub verbose: bool,
    pub no_update: bool,
    pub force_update: bool,
    pub overwrite_system: bool,
    pub fix_path: bool,
    pub ssh_args: Vec<OsString>,
}

pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Cli> {
    let mut iter = args.into_iter();
    let _argv0 = iter.next();

    // `fix_path` defaults to true so vssh-launched sessions get
    // `~/.local/bin` on PATH transparently, since most remote shells
    // don't include it by default. Opt out with `--no-fix-path`
    // (e.g. when running a remote command and the wrapper would
    // collide with the user's intent).
    let mut cli = Cli {
        verbose: false,
        no_update: false,
        force_update: false,
        overwrite_system: false,
        fix_path: true,
        ssh_args: Vec::new(),
    };

    // vssh's own flags only consume the leading portion of argv. As
    // soon as we see something that isn't one of ours (a positional
    // host arg, or any flag we don't recognise), everything from that
    // point on is forwarded verbatim to ssh. This mirrors how `kitten
    // ssh` and similar wrappers split their own options from the
    // wrapped tool's: no `--` separator needed in normal usage.
    let mut passthrough = false;
    for arg in iter {
        if passthrough {
            cli.ssh_args.push(arg);
            continue;
        }
        match arg.to_str() {
            Some("--help") | Some("-h") => {
                print_help();
                std::process::exit(0);
            }
            Some("--version") => {
                println!("vssh {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            Some("--vssh-verbose") => cli.verbose = true,
            Some("--no-update") => cli.no_update = true,
            Some("--force-update") => cli.force_update = true,
            Some("--overwrite-system") => cli.overwrite_system = true,
            Some("--no-fix-path") => cli.fix_path = false,
            _ => {
                cli.ssh_args.push(arg);
                passthrough = true;
            }
        }
    }

    if cli.ssh_args.is_empty() {
        bail!(
            "usage: vssh [vssh-flags] [ssh-args] [user@]host\n\
             Try 'vssh --help' for the full flag list."
        );
    }

    Ok(cli)
}

fn print_help() {
    let help = "\
vssh — ssh wrapper that keeps veter-tools fresh on remote hosts

USAGE:
    vssh [vssh-flags] [ssh-args] [user@]host [command ...]

vssh flags (must come before ssh args):
    --no-update           Skip the version probe / install step.
    --force-update        Reinstall veter-tools even if remote is current.
    --overwrite-system    Permit overwriting vmux outside ~/.local/bin.
    --no-fix-path         Do NOT prepend ~/.local/bin to PATH for the
                          session (vssh injects it by default so the
                          installed tools are immediately usable).
    --vssh-verbose        Log vssh's actions to stderr.
    -h, --help            Show this help.
    --version             Show vssh version.

All other arguments pass through to ssh(1) unchanged.
";
    print!("{help}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Cli> {
        parse(std::iter::once(OsString::from("vssh")).chain(args.iter().map(OsString::from)))
    }

    fn ssh_args_str(cli: &Cli) -> Vec<&str> {
        cli.ssh_args
            .iter()
            .map(|s| s.to_str().unwrap())
            .collect()
    }

    #[test]
    fn plain_host_passes_through() {
        let cli = parse_strs(&["user@host"]).unwrap();
        assert_eq!(ssh_args_str(&cli), vec!["user@host"]);
        assert!(!cli.verbose);
    }

    #[test]
    fn ssh_flags_pass_through_unmolested() {
        let cli = parse_strs(&["-p", "2222", "-J", "jump", "user@host"]).unwrap();
        assert_eq!(
            ssh_args_str(&cli),
            vec!["-p", "2222", "-J", "jump", "user@host"]
        );
    }

    #[test]
    fn vssh_flags_consumed_before_passthrough() {
        let cli = parse_strs(&["--no-update", "--vssh-verbose", "-p", "22", "host"]).unwrap();
        assert!(cli.no_update);
        assert!(cli.verbose);
        assert_eq!(ssh_args_str(&cli), vec!["-p", "22", "host"]);
    }

    #[test]
    fn unknown_long_flag_goes_to_ssh() {
        // `-L 8080:localhost:80` is ssh's, not ours. Treat the first
        // unknown token as "switch to passthrough mode" so it (and the
        // remaining args) reach ssh untouched.
        let cli = parse_strs(&["-L", "8080:localhost:80", "host"]).unwrap();
        assert_eq!(
            ssh_args_str(&cli),
            vec!["-L", "8080:localhost:80", "host"]
        );
    }

    #[test]
    fn vssh_flag_after_ssh_flag_is_treated_as_ssh_arg() {
        // Once we've switched to passthrough, even a known vssh flag
        // is forwarded verbatim. Order matters; document it.
        let cli = parse_strs(&["-p", "22", "--no-update", "host"]).unwrap();
        assert!(!cli.no_update);
        assert_eq!(
            ssh_args_str(&cli),
            vec!["-p", "22", "--no-update", "host"]
        );
    }

    #[test]
    fn no_args_errors() {
        let err = parse_strs(&[]).unwrap_err();
        assert!(err.to_string().contains("usage:"));
    }
}
