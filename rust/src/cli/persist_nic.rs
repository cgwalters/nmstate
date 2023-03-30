use std::io::BufReader;
use std::path::Path;

use nmstate::{InterfaceType, NetworkState};

use crate::error::CliError;

/// Comment added into our generated link files
const PERSIST_GENERATED_BY: &str = "# Generated by nmstate";
/// The file prefix for our generated pins.
/// 98 here is important as it should be invoked after others but before
/// 99-default.link
const PIN_FILE_PREFIX: &str = "98-nmstate";
/// Subdirectory of `/etc/nmstate` that can contain previously serialized network config.
const PIN_IFACE_NAME_FOLDER: &str = "pin_iface_name";
const PIN_STATE_FILENAME: &str = "pin.yml";
/// See https://www.freedesktop.org/software/systemd/man/systemd.link.html
const SYSTEMD_NETWORK_LINK_FOLDER: &str = "etc/systemd/network";
/// File which if present signals that we have already performed NIC name persistence.
const NMSTATE_PERSIST_STAMP: &str = ".nmstate-persist.stamp";

pub(crate) fn run_persist_from_prior_state(
    folder: &str,
) -> Result<(), CliError> {
    let pin_iface_name_dir = format!("{folder}/{PIN_IFACE_NAME_FOLDER}");
    let pin_iface_path = Path::new(&pin_iface_name_dir);
    if pin_iface_path.exists() {
        // We have a previously saved state for NIC name pinning; execute that now.
        persist_iface_name(&pin_iface_path)?;
    }
    Ok(())
}

/// For all active interfaces, write a systemd .link file which pins to the currently
/// active name.
pub(crate) fn run_persist_immediately(
    root: &str,
    dry_run: bool,
) -> Result<String, CliError> {
    let stamp_path = Path::new(root)
        .join(SYSTEMD_NETWORK_LINK_FOLDER)
        .join(NMSTATE_PERSIST_STAMP);
    if stamp_path.exists() {
        log::info!("{} exists; nothing to do", stamp_path.display());
        return Ok("".to_string());
    }

    let mut state = NetworkState::new();
    state.set_kernel_only(true);
    state.set_running_config_only(true);
    state.retrieve()?;

    let mut changed = false;
    for iface in state
        .interfaces
        .iter()
        .filter(|i| i.iface_type() == InterfaceType::Ethernet)
    {
        let mac = match iface.base_iface().mac_address.as_ref() {
            Some(c) => c,
            None => continue,
        };
        let action = if dry_run { "Would pin" } else { "Pinning" };
        log::info!(
            "{action} the interface with MAC {mac} to \
                        interface name {}",
            iface.name()
        );
        if !dry_run {
            changed |=
                persist_iface_name_via_systemd_link(root, mac, iface.name())?;
        }
    }

    if !changed {
        log::info!("No changes.");
    }

    if !dry_run {
        std::fs::write(stamp_path, b"")?;
    }

    Ok("".to_string())
}

/// Iterate over previously saved network state, and determine if any NICs
/// have changed name since then (using MAC address as a reference point).
/// If so, generate a systemd .link file to pin to the previous name.
fn persist_iface_name(cfg_dir: &Path) -> Result<(), CliError> {
    let root = "/";
    let file_path = cfg_dir.join(PIN_STATE_FILENAME);
    let pin_state: NetworkState = {
        let r = std::fs::File::open(&file_path).map(BufReader::new)?;
        serde_yaml::from_reader(r)?
    };
    let mut cur_state = NetworkState::new();
    cur_state.set_kernel_only(true);
    cur_state.set_running_config_only(true);
    cur_state.retrieve()?;

    for cur_iface in cur_state
        .interfaces
        .iter()
        .filter(|i| i.iface_type() == InterfaceType::Ethernet)
    {
        let cur_mac = match cur_iface.base_iface().mac_address.as_ref() {
            Some(c) => c,
            None => continue,
        };
        // If a NIC with this name already exists in the old state, then we have
        // nothing to do.
        if pin_state
            .interfaces
            .get_iface(cur_iface.name(), cur_iface.iface_type())
            .is_some()
        {
            continue;
        }
        // Look through the pin state for an ethernet device which matches this MAC address.
        for pin_iface in pin_state
            .interfaces
            .iter()
            .filter(|i| i.iface_type() == InterfaceType::Ethernet)
        {
            if pin_iface.base_iface().mac_address.as_ref() == Some(cur_mac)
                && pin_iface.name() != cur_iface.name()
            {
                log::info!(
                    "Pining the interface with MAC {cur_mac} to \
                        interface name {}",
                    pin_iface.name()
                );
                persist_iface_name_via_systemd_link(
                    root,
                    cur_mac,
                    pin_iface.name(),
                )?;
            }
        }
    }

    super::service::relocate_file(&file_path)?;
    Ok(())
}

fn persist_iface_name_via_systemd_link(
    root: &str,
    mac: &str,
    iface_name: &str,
) -> Result<bool, CliError> {
    let link_dir = Path::new(root).join(SYSTEMD_NETWORK_LINK_FOLDER);

    let file_path =
        link_dir.join(format!("{PIN_FILE_PREFIX}-{iface_name}.link"));
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
