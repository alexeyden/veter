//! vssh — ssh wrapper that keeps veter-tools fresh on remote hosts.
//!
//! Phase 3 (current): open ControlMaster, probe (arch, manifest,
//! vmux path, $HOME writability), decide what to do, log it, then
//! hand off to interactive ssh. `install::perform` is still a stub
//! — Phase 4 wires in the actual tarball upload.

mod args;
mod dist;
mod handoff;
mod install;
mod probe;
mod ssh;

use anyhow::Result;

fn main() -> Result<()> {
    let cli = args::parse(std::env::args_os())?;
    init_logging(cli.verbose);
    log::debug!("vssh args: {:?}", cli);

    let master = ssh::Master::open(cli.ssh_args.clone())?;
    let probe_result = probe::probe(&master)?;
    log::info!(
        "remote: arch={} vmux={} manifest={} home_writable={}",
        probe_result.arch,
        probe_result
            .vmux_path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".into()),
        probe_result
            .installed_manifest
            .as_ref()
            .map(|m| m.sha256.as_str())
            .unwrap_or("<none>"),
        probe_result.home_writable
    );

    let bundle = dist::locate(&probe_result.arch)?;
    if let Some(b) = &bundle {
        log::debug!(
            "local bundle: {} (sha256={}, version={})",
            b.tarball.display(),
            b.manifest.sha256,
            b.manifest.version
        );
    }

    let action = install::decide(bundle.as_ref(), &probe_result, &cli);
    match &action {
        install::Action::Install => {
            // Phase 4 will call install::perform(&master, bundle.as_ref().unwrap())
            // here. Phase 3 just logs.
            install::perform(&master, bundle.as_ref().unwrap())?;
        }
        install::Action::Skip(r) => log::info!("skipping install: {r:?}"),
        install::Action::RefuseSystem { path } => {
            log::warn!(
                "remote vmux at {} is managed externally; \
                 pass --overwrite-system to install over it",
                path.display()
            );
        }
    }

    let control_path = master.disarm();
    handoff::exec_ssh(&cli.ssh_args, Some(&control_path), cli.fix_path)
}

fn init_logging(verbose: bool) {
    let default = if verbose { "vssh=debug" } else { "vssh=info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .format_target(false)
        .format_timestamp(None)
        .init();
}
