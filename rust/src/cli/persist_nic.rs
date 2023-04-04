use std::path::Path;

use nmstate::{InterfaceType, NetworkState};

use crate::error::CliError;

/// Comment added into our generated link files
const PERSIST_GENERATED_BY: &str = "# Generated by nmstate";
/// The file prefix for our generated persisted NIC names.
/// 98 here is important as it should be invoked after others but before
/// 99-default.link
const PERSIST_FILE_PREFIX: &str = "98-nmstate";
/// See https://www.freedesktop.org/software/systemd/man/systemd.link.html
const SYSTEMD_NETWORK_LINK_FOLDER: &str = "etc/systemd/network";
/// File which if present signals that we have already performed NIC name persistence.
const NMSTATE_PERSIST_STAMP: &str = ".nmstate-persist.stamp";

/// The action to take
pub(crate) enum PersistAction {
    /// Persist NIC name state
    Save,
    /// Print what we would do in Save mode
    DryRun,
    /// Output any persisted state
    Inspect,
}

fn gather_state() -> Result<NetworkState, CliError> {
    let mut state = NetworkState::new();
    state.set_kernel_only(true);
    state.set_running_config_only(true);
    state.retrieve()?;
    Ok(state)
}

fn process_interfaces<F>(state: &NetworkState, mut f: F) -> Result<(), CliError>
where
    F: FnMut(&nmstate::Interface, &str) -> Result<(), CliError>,
{
    for iface in state
        .interfaces
        .iter()
        .filter(|i| i.iface_type() == InterfaceType::Ethernet)
    {
        let iface_name = iface.name();
        let mac = match iface.base_iface().mac_address.as_ref() {
            Some(c) => c,
            None => continue,
        };
        let base_iface = iface.base_iface();
        let ipv4_manual = base_iface
            .ipv4
            .as_ref()
            .map(|ip| ip.is_static())
            .unwrap_or_default();
        let ipv6_manual = base_iface
            .ipv6
            .as_ref()
            .map(|ip| ip.is_static())
            .unwrap_or_default();
        let ip_manual = ipv4_manual || ipv6_manual;
        if !ip_manual {
            log::info!("Skipping interface {iface_name} as no static IP addressing was found");
            continue;
        }

        f(iface, mac.as_str())?;
    }
    Ok(())
}

/// For all active interfaces, write a systemd .link file which persists the currently
/// active name.
pub(crate) fn run_persist_immediately(
    root: &str,
    action: PersistAction,
) -> Result<String, CliError> {
    let dry_run = match action {
        PersistAction::Save => false,
        PersistAction::DryRun => true,
        PersistAction::Inspect => return inspect(root),
    };

    let stamp_path = Path::new(root)
        .join(SYSTEMD_NETWORK_LINK_FOLDER)
        .join(NMSTATE_PERSIST_STAMP);
    if stamp_path.exists() {
        log::info!("{} exists; nothing to do", stamp_path.display());
        return Ok("".to_string());
    }

    let state = gather_state()?;
    let mut changed = false;
    process_interfaces(&state, |iface, mac| {
        let iface_name = iface.name();
        let action = if dry_run {
            "Would persist"
        } else {
            "Persisting"
        };
        log::info!(
            "{action} the interface with MAC {mac} to \
                        interface name {iface_name}"
        );
        if !dry_run {
            changed |=
                persist_iface_name_via_systemd_link(root, mac, iface.name())?;
        }
        Ok(())
    })?;

    if !changed {
        log::info!("No changes.");
    }

    if !dry_run {
        std::fs::write(stamp_path, b"")?;
    }

    Ok("".to_string())
}

pub(crate) fn inspect(root: &str) -> Result<String, CliError> {
    let netdir = Path::new(root).join(SYSTEMD_NETWORK_LINK_FOLDER);
    let stamp_path = netdir.join(NMSTATE_PERSIST_STAMP);
    if !stamp_path.exists() {
        log::info!(
            "{} does not exist, no prior persisted state",
            stamp_path.display()
        );
        return Ok("".to_string());
    }

    let mut n = 0;
    for e in netdir.read_dir()? {
        let e = e?;
        let name = e.file_name();
        let name = if let Some(n) = name.to_str() {
            n
        } else {
            continue;
        };
        if !name.ends_with(".link") {
            continue;
        }
        if !name.starts_with(PERSIST_FILE_PREFIX) {
            continue;
        }
        log::info!("Found persisted NIC file: {name}");
        n += 1;
    }
    if n == 0 {
        log::info!("No persisted NICs found");
    }

    let state = gather_state()?;
    process_interfaces(&state, |iface, mac| {
        let iface_name = iface.name();
        log::info!(
            "NOTE: would persist the interface with MAC {mac} to interface name {iface_name}"
        );
        Ok(())
    })?;

    Ok("".to_string())
}

fn persist_iface_name_via_systemd_link(
    root: &str,
    mac: &str,
    iface_name: &str,
) -> Result<bool, CliError> {
    let link_dir = Path::new(root).join(SYSTEMD_NETWORK_LINK_FOLDER);

    let file_path =
        link_dir.join(format!("{PERSIST_FILE_PREFIX}-{iface_name}.link"));
    if file_path.exists() {
        log::info!("Network link file {} already exists", file_path.display());
        return Ok(false);
    }

    if !link_dir.exists() {
        std::fs::create_dir(&link_dir)?;
    }

    let content =
        format!("{PERSIST_GENERATED_BY}\n[Match]\nMACAddress={mac}\n\n[Link]\nName={iface_name}\n");

    std::fs::write(&file_path, content.as_bytes()).map_err(|e| {
        CliError::from(format!(
            "Failed to store captured states to file {}: {e}",
            file_path.display()
        ))
    })?;
    log::info!(
        "Systemd network link file created at {}",
        file_path.display()
    );
    Ok(true)
}
