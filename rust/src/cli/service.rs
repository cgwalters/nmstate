// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use nmstate::{InterfaceType, NetworkState};

use crate::{apply::apply, error::CliError};

/// Comment added into our generated link files
const PIN_GENERATED_BY: &str = "# Generated by nmstate";
/// The file prefix for our generated pins.
/// 98 here is important as it should be invoked after others but before
/// 99-default.link
const PIN_FILE_PREFIX: &str = "98-nmstate";
const CONFIG_FILE_EXTENTION: &str = "yml";
const RELOCATE_FILE_EXTENTION: &str = "applied";
/// Subdirectory of `/etc/nmstate` that can contain previously serialized network config.
const PIN_IFACE_NAME_FOLDER: &str = "pin_iface_name";
const PIN_STATE_FILENAME: &str = "pin.yml";
/// See https://www.freedesktop.org/software/systemd/man/systemd.link.html
const SYSTEMD_NETWORK_LINK_FOLDER: &str = "/etc/systemd/network";
/// File which if present signals that we have already performed NIC pinning.
const NMSTATE_PINNED_STAMP: &str = ".nmstate-pinned.stamp";

pub(crate) fn ncl_service(
    matches: &clap::ArgMatches,
) -> Result<String, CliError> {
    let folder = matches
        .value_of(crate::CONFIG_FOLDER_KEY)
        .unwrap_or(crate::DEFAULT_SERVICE_FOLDER);

    let pin_iface_name_dir = format!("{folder}/{PIN_IFACE_NAME_FOLDER}");
    let pin_iface_path = Path::new(&pin_iface_name_dir);
    if pin_iface_path.exists() {
        // We have a previously saved state for NIC name pinning; execute that now.
        pin_iface_name(&pin_iface_path)?;
    }

    let config_files = match get_config_files(folder) {
        Ok(f) => f,
        Err(e) => {
            log::info!(
                "Failed to read config folder {folder} due to \
                error {e}, ignoring"
            );
            return Ok(String::new());
        }
    };
    if config_files.is_empty() {
        log::info!(
            "No nmstate config(end with .{}) found in config folder {}",
            CONFIG_FILE_EXTENTION,
            folder
        );
        return Ok(String::new());
    }

    // Due to bug of NetworkManager, the `After=NetworkManager.service` in
    // `nmstate.service` cannot guarantee the ready of NM dbus.
    // We sleep for 2 seconds here to avoid meaningless retry.
    std::thread::sleep(std::time::Duration::from_secs(2));

    for file_path in config_files {
        let mut fd = match std::fs::File::open(&file_path) {
            Ok(fd) => fd,
            Err(e) => {
                log::error!(
                    "Failed to read config file {}: {e}",
                    file_path.display()
                );
                continue;
            }
        };
        match apply(&mut fd, matches) {
            Ok(_) => {
                log::info!("Applied nmstate config: {}", file_path.display());
                if let Err(e) = relocate_file(&file_path) {
                    log::error!(
                        "Failed to rename applied state file: {} {}",
                        file_path.display(),
                        e
                    );
                }
            }
            Err(e) => {
                log::error!(
                    "Failed to apply state file {}: {}",
                    file_path.display(),
                    e
                );
            }
        }
    }

    Ok("".to_string())
}

// All file ending with `.yml` will be included.
fn get_config_files(folder: &str) -> Result<Vec<PathBuf>, CliError> {
    let folder = Path::new(folder);
    let mut ret = Vec::new();
    for entry in folder.read_dir()? {
        let file = entry?.path();
        if file.extension() == Some(OsStr::new(CONFIG_FILE_EXTENTION)) {
            ret.push(folder.join(file));
        }
    }
    ret.sort_unstable();
    Ok(ret)
}

// rename file by adding a suffix `.applied`.
fn relocate_file(file_path: &Path) -> Result<(), CliError> {
    let new_path = file_path.with_extension(RELOCATE_FILE_EXTENTION);
    std::fs::rename(file_path, &new_path)?;
    log::info!(
        "Renamed applied config {} to {}",
        file_path.display(),
        new_path.display()
    );
    Ok(())
}

/// For all active interfaces, write a systemd .link file which pins to the currently
/// active name.
pub(crate) fn ncl_pin_nic_names(dry_run: bool) -> Result<String, CliError> {
    let stamp_path =
        Path::new(SYSTEMD_NETWORK_LINK_FOLDER).join(NMSTATE_PINNED_STAMP);
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
            changed |= pin_iface_name_via_systemd_link(mac, iface.name())?;
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
fn pin_iface_name(cfg_dir: &Path) -> Result<(), CliError> {
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
                pin_iface_name_via_systemd_link(cur_mac, pin_iface.name())?;
            }
        }
    }

    relocate_file(&file_path)?;
    Ok(())
}

fn pin_iface_name_via_systemd_link(
    mac: &str,
    iface_name: &str,
) -> Result<bool, CliError> {
    let link_dir = Path::new(SYSTEMD_NETWORK_LINK_FOLDER);

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
        format!("{PIN_GENERATED_BY}\n[Match]\nMACAddress={mac}\n\n[Link]\nName={iface_name}\n");

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
