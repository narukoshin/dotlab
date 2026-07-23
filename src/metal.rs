use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::{
    DoctorArgs, MetalCommand, MetalCreateArgs, MetalDiscardArgs, MetalNameArgs, MetalPreflightArgs,
    MetalPromoteArgs,
};
use crate::doctor;
use crate::manifest::{METAL_SCHEMA, MetalOperation, MetalSlot, MetalState, PendingMetal};
use crate::paths::{AppPaths, validate_name};
use crate::util::{
    acquire_lock, atomic_write, command_exists, ensure_root, file_digest, node_exists, prompt_yes,
    read_json, remove_node, run_checked, run_stdout, timestamp, unique_id, write_json,
};

pub fn run(paths: &AppPaths, command: MetalCommand) -> Result<()> {
    ensure_root()?;
    match command {
        MetalCommand::Preflight(args) => preflight(paths, args),
        command => {
            fs::create_dir_all(&paths.metal_state)
                .with_context(|| format!("creating {}", paths.metal_state.display()))?;
            let _lock = acquire_lock(&paths.metal_lock())?;
            recover_pending(paths)?;
            match command {
                MetalCommand::Create(args) => create(paths, args),
                MetalCommand::Activate(args) => activate(paths, args),
                MetalCommand::Promote(args) => promote(paths, args),
                MetalCommand::Leave(args) => leave(paths, args),
                MetalCommand::Status => status(paths),
                MetalCommand::Discard(args) => discard(paths, args),
                MetalCommand::Preflight(_) => unreachable!(),
            }
        }
    }
}

fn preflight(paths: &AppPaths, args: MetalPreflightArgs) -> Result<()> {
    if args.json {
        doctor::run(
            paths,
            DoctorArgs {
                metal: true,
                json: true,
            },
        )
    } else {
        doctor::require_metal(paths).map(|_| ())
    }
}

fn create(paths: &AppPaths, args: MetalCreateArgs) -> Result<()> {
    validate_name(&args.name)?;
    ensure_original_boot(paths)?;
    let facts = doctor::require_metal(paths)?;
    let old_state = load_state(paths)?;
    if old_state.slots.contains_key(&args.name) {
        bail!("metal slot {:?} already exists", args.name);
    }
    prompt_yes(
        &format!(
            "Create isolated Btrfs root/home snapshots for slot {:?} and add guarded GRUB entries?",
            args.name
        ),
        args.yes,
    )?;

    let id = format!("{}-{}", args.name, unique_id()?);
    let directory = paths.snapshot_root.join(&id);
    let root_snapshot = directory.join("root");
    let home_snapshot = directory.join("home");
    let root_subvol = format!("@snapshots/dotlab/{id}/root");
    let home_subvol = format!("@snapshots/dotlab/{id}/home");
    let efi_directory = paths.boot.join("EFI/Linux");
    let slot_uki = efi_directory.join(format!("dotlab-slot-{id}.efi"));
    let original_uki = efi_directory.join(format!("dotlab-original-{id}.efi"));
    let kernel_arguments = slot_kernel_arguments(&facts.kernel_arguments, &root_subvol, &id)?;
    let slot = MetalSlot {
        schema: METAL_SCHEMA,
        id,
        name: args.name.clone(),
        created_at: timestamp()?,
        root_snapshot,
        home_snapshot,
        root_subvol,
        home_subvol,
        slot_uki,
        original_uki,
        source_uki: facts.source_uki,
        kernel_arguments,
        esp_uuid: facts.esp_uuid,
        promoted: false,
        promoted_at: None,
    };
    validate_slot(paths, &slot)?;

    let mut new_state = old_state.clone();
    new_state.slots.insert(args.name.clone(), slot.clone());
    let pending = PendingMetal {
        schema: METAL_SCHEMA,
        operation: MetalOperation::Create,
        old_state,
        new_state: new_state.clone(),
        slot: slot.clone(),
    };
    save_pending(paths, &pending)?;

    let result = (|| -> Result<()> {
        fs::create_dir_all(&paths.snapshot_root)
            .with_context(|| format!("creating {}", paths.snapshot_root.display()))?;
        fs::create_dir(&directory).with_context(|| format!("creating {}", directory.display()))?;
        run_checked(
            "btrfs",
            [
                OsString::from("subvolume"),
                OsString::from("snapshot"),
                OsString::from("--"),
                OsString::from("/"),
                slot.root_snapshot.as_os_str().to_owned(),
            ],
        )?;
        run_checked(
            "btrfs",
            [
                OsString::from("subvolume"),
                OsString::from("snapshot"),
                OsString::from("--"),
                OsString::from("/home"),
                slot.home_snapshot.as_os_str().to_owned(),
            ],
        )?;
        rewrite_slot_fstab(&slot)?;

        fs::create_dir_all(&efi_directory)?;
        copy_file_atomic(&slot.source_uki, &slot.slot_uki)?;
        copy_file_atomic(&slot.source_uki, &slot.original_uki)?;
        let source_hash = file_digest(&slot.source_uki)?;
        if file_digest(&slot.slot_uki)? != source_hash
            || file_digest(&slot.original_uki)? != source_hash
        {
            bail!("UKI copy verification failed");
        }
        atomic_write(&ready_path(paths, &slot), b"ready\n", 0o600)?;
        save_state(paths, &new_state)?;
        regenerate_grub(paths, &new_state)?;
        remove_node(&pending_path(paths))?;
        remove_node(&ready_path(paths, &slot))?;
        Ok(())
    })();

    if let Err(error) = result {
        let recovery = recover_pending(paths);
        return match recovery {
            Ok(()) => Err(error.context(
                "slot creation was interrupted; Dotlab safely completed or unwound its journal",
            )),
            Err(recovery_error) => Err(anyhow!(
                "slot creation failed: {error:#}; recovery is still journaled and also failed: \
                 {recovery_error:#}. Do not delete snapshots or EFI files; rerun `sudo dotlab \
                 metal status` after fixing the reported cause"
            )),
        };
    }

    println!("Created metal slot {} ({})", slot.name, slot.id);
    println!(
        "Its root and home are isolated; /boot is read-only inside the slot. Activate it with:"
    );
    println!("  sudo dotlab metal activate {}", slot.name);
    Ok(())
}

fn activate(paths: &AppPaths, args: MetalNameArgs) -> Result<()> {
    validate_name(&args.name)?;
    let state = load_state(paths)?;
    let slot = state
        .slots
        .get(&args.name)
        .with_context(|| format!("metal slot {:?} does not exist", args.name))?;
    verify_slot_artifacts(paths, slot)?;
    let entry = if slot.promoted {
        primary_entry_id(slot)
    } else {
        slot_entry_id(slot)
    };
    select_grub_entry(paths, &entry)?;
    println!(
        "GRUB will boot Dotlab slot {} once; {}",
        slot.name,
        if slot.promoted {
            "it is also the persistent primary."
        } else {
            "the persistent default is unchanged."
        }
    );
    maybe_reboot(args.reboot)
}

fn promote(paths: &AppPaths, args: MetalPromoteArgs) -> Result<()> {
    validate_name(&args.name)?;
    let active_id = current_slot_id(paths)?.context(
        "promotion must run from the slot being promoted; activate the slot and boot it first",
    )?;
    let old_state = load_state(paths)?;
    let old_slot = old_state
        .slots
        .get(&args.name)
        .cloned()
        .with_context(|| format!("metal slot {:?} does not exist", args.name))?;
    if active_id != old_slot.id {
        bail!(
            "currently booted slot is {active_id}, not {}; promote the matching slot",
            old_slot.id
        );
    }
    if old_slot.promoted {
        bail!(
            "metal slot {:?} is already the persistent primary",
            args.name
        );
    }
    if let Some(primary) = old_state.slots.values().find(|slot| slot.promoted) {
        bail!(
            "metal slot {:?} is already promoted; Dotlab permits only one primary slot",
            primary.name
        );
    }
    for command in [
        "findmnt",
        "grub-mkconfig",
        "grub-script-check",
        "grub-set-default",
        "mount",
    ] {
        if !command_exists(command) {
            bail!("required promotion command {command:?} is not available");
        }
    }
    if node_exists(&paths.pacman_lock) {
        bail!(
            "{} exists; wait for the package transaction to finish before promotion",
            paths.pacman_lock.display()
        );
    }
    verify_slot_artifacts(paths, &old_slot)?;
    ensure_slot_mounts(paths, &old_slot)?;
    verify_promotion_ukis(&old_slot)?;
    if node_exists(&ready_path(paths, &old_slot)) {
        // With no pending journal, a ready marker can only remain after the
        // journal itself was durably removed from a committed create.
        remove_node(&ready_path(paths, &old_slot))?;
    }
    prompt_yes(
        &format!(
            "Promote slot {:?} to the persistent GRUB default, make /boot writable, and preserve both original and last-known-good rescue entries?",
            args.name
        ),
        args.yes,
    )?;

    let mut promoted = old_slot.clone();
    promoted.schema = METAL_SCHEMA;
    promoted.promoted = true;
    promoted.promoted_at = Some(timestamp()?);
    let mut new_state = old_state.clone();
    new_state.schema = METAL_SCHEMA;
    for slot in new_state.slots.values_mut() {
        slot.schema = METAL_SCHEMA;
    }
    new_state.slots.insert(args.name.clone(), promoted.clone());
    let pending = PendingMetal {
        schema: METAL_SCHEMA,
        operation: MetalOperation::Promote,
        old_state,
        new_state: new_state.clone(),
        slot: promoted.clone(),
    };
    save_pending(paths, &pending)?;

    let result = (|| -> Result<()> {
        ensure_boot_writable(paths)?;
        rewrite_slot_boot_mode(&promoted, false)?;
        save_state(paths, &new_state)?;
        regenerate_grub(paths, &new_state)?;
        // This marker is the commit point. Recovery rolls back before it and
        // completes promotion after it.
        atomic_write(&ready_path(paths, &promoted), b"promote\n", 0o600)?;
        select_persistent_grub_entry(paths, &primary_entry_id(&promoted))?;
        remove_node(&pending_path(paths))?;
        remove_node(&ready_path(paths, &promoted))?;
        Ok(())
    })();

    if let Err(error) = result {
        let recovery = recover_pending(paths);
        return match recovery {
            Ok(()) => Err(error.context(
                "promotion was interrupted; Dotlab safely completed or rolled back its journal",
            )),
            Err(recovery_error) => Err(anyhow!(
                "promotion failed: {error:#}; recovery is still journaled and also failed: \
                 {recovery_error:#}. Do not alter GRUB, /boot, or slot snapshots; rerun `sudo \
                 dotlab metal status` after fixing the reported cause"
            )),
        };
    }

    println!(
        "Promoted metal slot {} ({}) to primary.",
        promoted.name, promoted.id
    );
    println!("Persistent GRUB entry: {}", primary_entry_id(&promoted));
    println!("Preserved fallbacks: last-known-good slot and original system.");
    println!("Run `sudo mkinitcpio -P` now to complete any package hook that /boot blocked.");
    maybe_reboot(args.reboot)
}

fn leave(paths: &AppPaths, args: MetalNameArgs) -> Result<()> {
    validate_name(&args.name)?;
    let state = load_state(paths)?;
    let slot = state
        .slots
        .get(&args.name)
        .with_context(|| format!("metal slot {:?} does not exist", args.name))?;
    if let Some(active_id) = current_slot_id(paths)? {
        if active_id != slot.id {
            bail!(
                "currently booted slot is {active_id}, not {}; leave the matching slot",
                slot.id
            );
        }
    }
    verify_slot_artifacts(paths, slot)?;
    let entry = original_entry_id(slot);
    select_grub_entry(paths, &entry)?;
    println!(
        "GRUB will boot the preserved original system for slot {} once.",
        slot.name
    );
    maybe_reboot(args.reboot)
}

fn status(paths: &AppPaths) -> Result<()> {
    let state = load_state(paths)?;
    let active_id = current_slot_id(paths)?;
    if state.slots.is_empty() {
        println!("No metal slots.");
        println!("Current boot: original system");
        return Ok(());
    }
    for slot in state.slots.values() {
        let active = active_id.as_deref() == Some(slot.id.as_str());
        let artifacts = verify_slot_artifacts(paths, slot).is_ok();
        let condition = match (artifacts, slot.promoted) {
            (true, true) => "PRIMARY",
            (true, false) => "ready",
            (false, true) => "PRIMARY INCOMPLETE",
            (false, false) => "INCOMPLETE",
        };
        println!(
            "{} {} ({})\t{}",
            if active { "*" } else { " " },
            slot.name,
            slot.id,
            condition
        );
    }
    match active_id {
        Some(id) if state.slots.values().any(|slot| slot.id == id) => {
            println!("Current boot: Dotlab slot {id}");
        }
        Some(id) => {
            bail!("kernel reports unknown dotlab.slot={id}; refusing mutations")
        }
        None => println!("Current boot: original system"),
    }
    Ok(())
}

fn discard(paths: &AppPaths, args: MetalDiscardArgs) -> Result<()> {
    validate_name(&args.name)?;
    ensure_original_boot(paths)?;
    let old_state = load_state(paths)?;
    let slot = old_state
        .slots
        .get(&args.name)
        .cloned()
        .with_context(|| format!("metal slot {:?} does not exist", args.name))?;
    if slot.promoted {
        bail!(
            "refusing to discard promoted primary {:?}; boot the preserved original with `sudo \
             dotlab metal leave {} --reboot` and keep this slot until a supported demotion is \
             completed",
            slot.name,
            slot.name
        );
    }
    prompt_yes(
        &format!(
            "Permanently delete slot {:?}, including its root/home snapshots and two UKI copies?",
            args.name
        ),
        args.yes,
    )?;
    let mut new_state = old_state.clone();
    new_state.slots.remove(&args.name);
    let pending = PendingMetal {
        schema: METAL_SCHEMA,
        operation: MetalOperation::Discard,
        old_state,
        new_state: new_state.clone(),
        slot: slot.clone(),
    };
    save_pending(paths, &pending)?;

    let result = (|| -> Result<()> {
        save_state(paths, &new_state)?;
        // Remove boot references before deleting anything they point to.
        regenerate_grub(paths, &new_state)?;
        delete_slot_artifacts(paths, &slot)?;
        remove_node(&pending_path(paths))?;
        Ok(())
    })();
    if let Err(error) = result {
        return Err(error.context(
            "discard remains journaled; rerun any `sudo dotlab metal ...` command to resume it",
        ));
    }
    println!("Discarded metal slot {}.", slot.name);
    Ok(())
}

fn recover_pending(paths: &AppPaths) -> Result<()> {
    let path = pending_path(paths);
    if !path.is_file() {
        return Ok(());
    }
    let pending: PendingMetal = read_json(&path)?;
    if !supported_metal_schema(pending.schema) || !supported_metal_schema(pending.slot.schema) {
        bail!("unsupported pending metal journal schema");
    }
    validate_slot(paths, &pending.slot)?;
    eprintln!(
        "dotlab: recovering pending metal {:?} for {}",
        pending.operation, pending.slot.name
    );
    let remove_ready = match pending.operation {
        MetalOperation::Create => {
            if ready_path(paths, &pending.slot).is_file() {
                verify_slot_artifacts(paths, &pending.slot)?;
                save_state(paths, &pending.new_state)?;
                regenerate_grub(paths, &pending.new_state)?;
                true
            } else {
                save_state(paths, &pending.old_state)?;
                regenerate_grub(paths, &pending.old_state)?;
                delete_slot_artifacts(paths, &pending.slot)?;
                false
            }
        }
        MetalOperation::Discard => {
            save_state(paths, &pending.new_state)?;
            regenerate_grub(paths, &pending.new_state)?;
            delete_slot_artifacts(paths, &pending.slot)?;
            false
        }
        MetalOperation::Promote => recover_promotion(paths, &pending)?,
    };
    remove_node(&path)?;
    if remove_ready {
        remove_node(&ready_path(paths, &pending.slot))?;
    }
    Ok(())
}

fn recover_promotion(paths: &AppPaths, pending: &PendingMetal) -> Result<bool> {
    let committed = ready_path(paths, &pending.slot).is_file();
    let active = current_slot_id(paths)?.as_deref() == Some(pending.slot.id.as_str());
    ensure_boot_writable(paths)?;

    let recovery = if committed {
        (|| -> Result<()> {
            verify_slot_artifacts(paths, &pending.slot)?;
            rewrite_slot_boot_mode(&pending.slot, false)?;
            save_state(paths, &pending.new_state)?;
            regenerate_grub(paths, &pending.new_state)?;
            select_persistent_grub_entry(paths, &primary_entry_id(&pending.slot))?;
            Ok(())
        })()
    } else {
        (|| -> Result<()> {
            rewrite_slot_boot_mode(&pending.slot, true)?;
            save_state(paths, &pending.old_state)?;
            regenerate_grub(paths, &pending.old_state)?;
            Ok(())
        })()
    };

    let mount_recovery = if !committed && active {
        ensure_boot_read_only(paths)
    } else {
        Ok(())
    };
    match (recovery, mount_recovery) {
        (Err(error), Err(mount_error)) => Err(anyhow!(
            "promotion recovery failed: {error:#}; additionally failed to restore read-only \
             /boot: {mount_error:#}"
        )),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => {
            Err(error.context("promotion rolled back, but /boot could not be remounted read-only"))
        }
        (Ok(()), Ok(())) => Ok(committed),
    }
}

fn rewrite_slot_fstab(slot: &MetalSlot) -> Result<()> {
    let path = slot.root_snapshot.join("etc/fstab");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading snapshot fstab {}", path.display()))?;
    let mut found_root = false;
    let mut found_home = false;
    let mut found_boot = false;
    let mut output = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            output.push_str(line);
            output.push('\n');
            continue;
        }
        let mut fields = trimmed
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if fields.len() < 4 {
            bail!("malformed fstab line in snapshot: {line:?}");
        }
        match fields[1].as_str() {
            "/" => {
                fields[3] = replace_subvolume(&fields[3], &slot.root_subvol)?;
                found_root = true;
            }
            "/home" => {
                fields[3] = replace_subvolume(&fields[3], &slot.home_subvol)?;
                found_home = true;
            }
            "/boot" => {
                fields[3] = boot_mode_options(&fields[3], true);
                found_boot = true;
            }
            _ => {}
        }
        output.push_str(&fields.join("\t"));
        output.push('\n');
    }
    if !(found_root && found_home && found_boot) {
        bail!(
            "snapshot fstab must contain explicit /, /home, and /boot rows (found root={found_root}, home={found_home}, boot={found_boot})"
        );
    }
    atomic_write(&path, output.as_bytes(), 0o644)
}

fn rewrite_slot_boot_mode(slot: &MetalSlot, read_only: bool) -> Result<()> {
    let path = slot.root_snapshot.join("etc/fstab");
    let content = fs::read_to_string(&path)
        .with_context(|| format!("reading slot fstab {}", path.display()))?;
    let mut found_boot = false;
    let mut output = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            output.push_str(line);
            output.push('\n');
            continue;
        }
        let mut fields = trimmed
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if fields.len() < 4 {
            bail!("malformed fstab line in slot: {line:?}");
        }
        if fields[1] == "/boot" {
            fields[3] = boot_mode_options(&fields[3], read_only);
            found_boot = true;
        }
        output.push_str(&fields.join("\t"));
        output.push('\n');
    }
    if !found_boot {
        bail!("slot fstab has no explicit /boot row");
    }
    atomic_write(&path, output.as_bytes(), 0o644)
}

fn replace_subvolume(options: &str, subvolume: &str) -> Result<String> {
    let mut found = false;
    let mut result = Vec::new();
    for option in options.split(',') {
        if option.starts_with("subvol=") {
            if found {
                bail!("multiple subvol= options in fstab");
            }
            result.push(format!("subvol=/{subvolume}"));
            found = true;
        } else if !option.starts_with("subvolid=") {
            result.push(option.to_owned());
        }
    }
    if !found {
        bail!("Btrfs fstab row has no subvol= option");
    }
    Ok(result.join(","))
}

fn boot_mode_options(options: &str, read_only: bool) -> String {
    let mut values = options
        .split(',')
        .filter(|option| *option != "rw" && *option != "ro")
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    values.push(if read_only { "ro" } else { "rw" }.to_owned());
    values.join(",")
}

fn slot_kernel_arguments(original: &[String], root_subvol: &str, id: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut replaced_rootflags = false;
    for argument in original {
        if argument.starts_with("dotlab.slot=") {
            continue;
        }
        if let Some(options) = argument.strip_prefix("rootflags=") {
            let mut values = options
                .split(',')
                .filter(|value| !value.starts_with("subvol=") && !value.starts_with("subvolid="))
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            values.push(format!("subvol=/{root_subvol}"));
            result.push(format!("rootflags={}", values.join(",")));
            replaced_rootflags = true;
        } else {
            result.push(argument.clone());
        }
    }
    if !replaced_rootflags {
        bail!("kernel command line has no rootflags= argument");
    }
    result.push(format!("dotlab.slot={id}"));
    Ok(result)
}

fn regenerate_grub(paths: &AppPaths, state: &MetalState) -> Result<()> {
    let script = grub_script(paths, state)?;
    let old_script = if node_exists(&paths.grub_script) {
        let metadata = fs::symlink_metadata(&paths.grub_script)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("{} must be a regular file", paths.grub_script.display());
        }
        Some((fs::read(&paths.grub_script)?, metadata.mode() & 0o7777))
    } else {
        None
    };
    atomic_write(&paths.grub_script, script.as_bytes(), 0o755)?;

    let result = (|| -> Result<()> {
        let parent = paths
            .grub_config
            .parent()
            .context("GRUB config has no parent")?;
        let temporary = parent.join(format!(".grub.cfg.dotlab-{}", unique_id()?));
        let generation = run_checked(
            "grub-mkconfig",
            [OsString::from("-o"), temporary.as_os_str().to_owned()],
        );
        if let Err(error) = generation {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if fs::metadata(&temporary).map_or(true, |metadata| metadata.len() == 0) {
            let _ = fs::remove_file(&temporary);
            bail!("grub-mkconfig produced an empty file");
        }
        let check = run_checked("grub-script-check", [temporary.as_os_str().to_owned()]);
        if let Err(error) = check {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        fs::rename(&temporary, &paths.grub_config)
            .with_context(|| format!("atomically replacing {}", paths.grub_config.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if let Err(error) = result {
        match old_script {
            Some((bytes, mode)) => atomic_write(&paths.grub_script, &bytes, mode)?,
            None => remove_node(&paths.grub_script)?,
        }
        return Err(error.context("existing grub.cfg was left untouched"));
    }
    Ok(())
}

fn grub_script(paths: &AppPaths, state: &MetalState) -> Result<String> {
    let mut output =
        String::from("#!/bin/sh\n# Generated by Dotlab. Do not edit.\nexec tail -n +4 \"$0\"\n\n");
    for slot in state.slots.values() {
        validate_slot(paths, slot)?;
        let slot_path = efi_grub_path(paths, &slot.slot_uki)?;
        let original_path = efi_grub_path(paths, &slot.original_uki)?;
        if slot.promoted {
            let primary_path = efi_grub_path(paths, &slot.source_uki)?;
            append_grub_entry(
                &mut output,
                &format!("Dotlab: {} (primary)", slot.name),
                &primary_entry_id(slot),
                &primary_path,
                &slot.kernel_arguments,
                &slot.esp_uuid,
            );
            append_grub_entry(
                &mut output,
                &format!("Dotlab: {} (last-known-good)", slot.name),
                &last_known_good_entry_id(slot),
                &slot_path,
                &slot.kernel_arguments,
                &slot.esp_uuid,
            );
        } else {
            append_grub_entry(
                &mut output,
                &format!("Dotlab: {} (experiment)", slot.name),
                &slot_entry_id(slot),
                &slot_path,
                &slot.kernel_arguments,
                &slot.esp_uuid,
            );
        }

        let original_arguments = original_kernel_arguments(slot)?;
        append_grub_entry(
            &mut output,
            &format!("Dotlab: {} (preserved original)", slot.name),
            &original_entry_id(slot),
            &original_path,
            &original_arguments,
            &slot.esp_uuid,
        );
    }
    Ok(output)
}

fn append_grub_entry(
    output: &mut String,
    title: &str,
    id: &str,
    uki_path: &str,
    kernel_arguments: &[String],
    esp_uuid: &str,
) {
    output.push_str(&format!("menuentry '{title}' --id '{id}' {{\n"));
    output.push_str("    insmod part_gpt\n    insmod fat\n    insmod chain\n");
    output.push_str(&format!(
        "    search --no-floppy --fs-uuid --set=dotlab_esp '{}'\n",
        grub_quote(esp_uuid)
    ));
    output.push_str(&format!(
        "    chainloader ($dotlab_esp)/{}",
        grub_quote(uki_path)
    ));
    for argument in kernel_arguments {
        output.push(' ');
        output.push('\'');
        output.push_str(&grub_quote(argument));
        output.push('\'');
    }
    output.push_str("\n    boot\n}\n\n");
}

fn original_kernel_arguments(slot: &MetalSlot) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut replaced = false;
    for argument in &slot.kernel_arguments {
        if argument.starts_with("dotlab.slot=") {
            continue;
        }
        if let Some(options) = argument.strip_prefix("rootflags=") {
            let mut values = options
                .split(',')
                .filter(|value| !value.starts_with("subvol=") && !value.starts_with("subvolid="))
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            values.push("subvol=/@".to_owned());
            result.push(format!("rootflags={}", values.join(",")));
            replaced = true;
        } else {
            result.push(argument.clone());
        }
    }
    if !replaced {
        bail!("slot {} has no rootflags argument", slot.name);
    }
    Ok(result)
}

fn grub_quote(value: &str) -> String {
    value.to_owned()
}

fn efi_grub_path(paths: &AppPaths, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(&paths.boot)
        .with_context(|| format!("{} is outside {}", path.display(), paths.boot.display()))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn select_grub_entry(paths: &AppPaths, entry: &str) -> Result<()> {
    require_grub_entry(paths, entry)?;
    let was_read_only = boot_is_read_only(paths)?;
    if was_read_only {
        ensure_boot_writable(paths)?;
    }
    let selection = run_checked("grub-reboot", [entry]);
    if was_read_only {
        let remount = ensure_boot_read_only(paths);
        match (selection, remount) {
            (Err(select_error), Err(remount_error)) => {
                return Err(anyhow!(
                    "grub-reboot failed: {select_error:#}; additionally failed to restore \
                     read-only /boot: {remount_error:#}"
                ));
            }
            (Err(error), Ok(())) => return Err(error),
            (Ok(_), Err(error)) => {
                return Err(error.context(
                    "GRUB entry was selected, but /boot could not be remounted read-only",
                ));
            }
            (Ok(_), Ok(())) => {}
        }
    } else {
        selection?;
    }
    Ok(())
}

fn select_persistent_grub_entry(paths: &AppPaths, entry: &str) -> Result<()> {
    require_grub_entry(paths, entry)?;
    if boot_is_read_only(paths)? {
        bail!("cannot set a persistent GRUB default while /boot is read-only");
    }
    run_checked(
        "grub-set-default",
        [
            OsString::from(format!("--boot-directory={}", paths.boot.display())),
            OsString::from(entry),
        ],
    )?;
    Ok(())
}

fn require_grub_entry(paths: &AppPaths, entry: &str) -> Result<()> {
    let config = fs::read_to_string(&paths.grub_config)
        .with_context(|| format!("reading {}", paths.grub_config.display()))?;
    if !config.contains(&format!("--id '{entry}'")) && !config.contains(&format!("--id {entry}")) {
        bail!(
            "GRUB entry {entry:?} is missing from {}; run a metal command to recover the journal",
            paths.grub_config.display()
        );
    }
    Ok(())
}

fn boot_is_read_only(paths: &AppPaths) -> Result<bool> {
    let options = run_stdout(
        "findmnt",
        [
            OsString::from("-n"),
            OsString::from("-o"),
            OsString::from("OPTIONS"),
            OsString::from("--target"),
            paths.boot.as_os_str().to_owned(),
        ],
    )?;
    Ok(options.split(',').any(|option| option == "ro"))
}

fn ensure_boot_writable(paths: &AppPaths) -> Result<()> {
    if !boot_is_read_only(paths)? {
        return Ok(());
    }
    run_checked(
        "mount",
        [
            OsString::from("-o"),
            OsString::from("remount,rw"),
            paths.boot.as_os_str().to_owned(),
        ],
    )?;
    Ok(())
}

fn ensure_boot_read_only(paths: &AppPaths) -> Result<()> {
    if boot_is_read_only(paths)? {
        return Ok(());
    }
    run_checked(
        "mount",
        [
            OsString::from("-o"),
            OsString::from("remount,ro"),
            paths.boot.as_os_str().to_owned(),
        ],
    )?;
    Ok(())
}

fn maybe_reboot(reboot: bool) -> Result<()> {
    if reboot {
        run_checked("systemctl", ["reboot"])?;
    } else {
        println!("Reboot when ready, or repeat with --reboot.");
    }
    Ok(())
}

fn verify_slot_artifacts(paths: &AppPaths, slot: &MetalSlot) -> Result<()> {
    validate_slot(paths, slot)?;
    let mut required = vec![
        &slot.root_snapshot,
        &slot.home_snapshot,
        &slot.slot_uki,
        &slot.original_uki,
    ];
    if slot.promoted {
        required.push(&slot.source_uki);
    }
    for path in required {
        if !node_exists(path) {
            bail!("slot artifact is missing: {}", path.display());
        }
    }
    Ok(())
}

fn verify_promotion_ukis(slot: &MetalSlot) -> Result<()> {
    let slot_hash = file_digest(&slot.slot_uki)?;
    if file_digest(&slot.original_uki)? != slot_hash {
        bail!(
            "preserved original UKI differs from the last-known-good slot UKI; refusing promotion"
        );
    }
    if file_digest(&slot.source_uki)? != slot_hash {
        bail!(
            "the normal UKI changed after this slot was created; refusing to promote an untested \
             primary boot image"
        );
    }
    Ok(())
}

fn ensure_slot_mounts(paths: &AppPaths, slot: &MetalSlot) -> Result<()> {
    require_mount_subvolume(Path::new("/"), &slot.root_subvol)?;
    require_mount_subvolume(Path::new("/home"), &slot.home_subvol)?;
    if current_slot_id(paths)?.as_deref() != Some(slot.id.as_str()) {
        bail!("kernel command line does not identify slot {}", slot.id);
    }
    Ok(())
}

fn require_mount_subvolume(target: &Path, expected: &str) -> Result<()> {
    let output = run_stdout(
        "findmnt",
        [
            OsString::from("-n"),
            OsString::from("-o"),
            OsString::from("SOURCE,OPTIONS"),
            OsString::from("--target"),
            target.as_os_str().to_owned(),
        ],
    )?;
    let mut fields = output.split_whitespace();
    let source = fields.next().context("findmnt omitted SOURCE")?;
    let options = fields.next().context("findmnt omitted OPTIONS")?;
    let expected = expected.trim_start_matches('/');
    let source_matches = source.ends_with(&format!("[/{expected}]"));
    let options_match = options.split(',').any(|option| {
        option
            .strip_prefix("subvol=")
            .is_some_and(|value| value.trim_start_matches('/') == expected)
    });
    if !source_matches && !options_match {
        bail!(
            "{} is not mounted from expected subvolume /{}: {}",
            target.display(),
            expected,
            output
        );
    }
    Ok(())
}

fn validate_slot(paths: &AppPaths, slot: &MetalSlot) -> Result<()> {
    validate_name(&slot.name)?;
    let valid_id = !slot.id.is_empty()
        && slot.id.len() <= 192
        && slot.id.starts_with(&format!("{}-", slot.name))
        && slot
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    let expected_root = paths.snapshot_root.join(&slot.id).join("root");
    let expected_home = paths.snapshot_root.join(&slot.id).join("home");
    let expected_slot_uki = paths
        .boot
        .join("EFI/Linux")
        .join(format!("dotlab-slot-{}.efi", slot.id));
    let expected_original_uki = paths
        .boot
        .join("EFI/Linux")
        .join(format!("dotlab-original-{}.efi", slot.id));
    let expected_root_subvol = format!("@snapshots/dotlab/{}/root", slot.id);
    let expected_home_subvol = format!("@snapshots/dotlab/{}/home", slot.id);
    let efi_directory = paths.boot.join("EFI/Linux");
    let valid_source_uki = slot.source_uki.parent() == Some(efi_directory.as_path())
        && slot
            .source_uki
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("efi"))
        && slot.source_uki != slot.slot_uki
        && slot.source_uki != slot.original_uki;
    let promotion_fields_valid = if slot.promoted {
        slot.schema == METAL_SCHEMA
            && slot.promoted_at.as_deref().is_some_and(|value| {
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
            })
    } else {
        slot.promoted_at.is_none()
    };
    if !supported_metal_schema(slot.schema)
        || !valid_id
        || !valid_source_uki
        || !promotion_fields_valid
        || slot.root_snapshot != expected_root
        || slot.home_snapshot != expected_home
        || slot.slot_uki != expected_slot_uki
        || slot.original_uki != expected_original_uki
        || slot.root_subvol != expected_root_subvol
        || slot.home_subvol != expected_home_subvol
    {
        bail!("unsafe or malformed metal slot state for {}", slot.name);
    }
    if slot.id.contains(['/', '\n', '\r', '\0'])
        || slot.esp_uuid.contains(['\n', '\r', '\0', '\''])
        || slot
            .kernel_arguments
            .iter()
            .any(|value| value.contains(['\n', '\r', '\0', '\'']))
    {
        bail!("unsafe control character in metal slot state");
    }
    Ok(())
}

fn delete_slot_artifacts(paths: &AppPaths, slot: &MetalSlot) -> Result<()> {
    validate_slot(paths, slot)?;
    for snapshot in [&slot.home_snapshot, &slot.root_snapshot] {
        if node_exists(snapshot) {
            run_checked(
                "btrfs",
                [
                    OsString::from("subvolume"),
                    OsString::from("delete"),
                    OsString::from("--"),
                    snapshot.as_os_str().to_owned(),
                ],
            )?;
        }
    }
    if let Some(parent) = slot.root_snapshot.parent() {
        if parent.starts_with(&paths.snapshot_root) && parent.is_dir() {
            fs::remove_dir(parent)
                .with_context(|| format!("removing empty {}", parent.display()))?;
        }
    }
    for uki in [&slot.slot_uki, &slot.original_uki] {
        remove_node(uki)?;
    }
    remove_node(&ready_path(paths, slot))?;
    Ok(())
}

fn copy_file_atomic(source: &Path, destination: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("inspecting {}", source.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("UKI source must be a regular file: {}", source.display());
    }
    if node_exists(destination) {
        bail!("refusing to overwrite existing {}", destination.display());
    }
    let parent = destination
        .parent()
        .context("UKI destination has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".dotlab-uki-{}", unique_id()?));
    let result = (|| -> Result<()> {
        fs::copy(source, &temporary)
            .with_context(|| format!("copying {} to {}", source.display(), temporary.display()))?;
        let mut file = fs::OpenOptions::new().append(true).open(&temporary)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temporary, destination)
            .with_context(|| format!("publishing {}", destination.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_original_boot(paths: &AppPaths) -> Result<()> {
    if let Some(id) = current_slot_id(paths)? {
        bail!(
            "this operation must run from the original system, not slot {id}; use `sudo dotlab metal leave <name> --reboot` first"
        );
    }
    let root = run_stdout(
        "findmnt",
        [
            OsString::from("-n"),
            OsString::from("-o"),
            OsString::from("SOURCE,OPTIONS"),
            OsString::from("--target"),
            OsString::from("/"),
        ],
    )?;
    let mut fields = root.split_whitespace();
    let source = fields.next().unwrap_or_default();
    let options = fields.next().unwrap_or_default();
    let original_root =
        source.ends_with("[/@]") || options.split(',').any(|option| option == "subvol=/@");
    if !original_root {
        bail!("root is not the original /@ subvolume; refusing this operation");
    }
    Ok(())
}

fn current_slot_id(paths: &AppPaths) -> Result<Option<String>> {
    let content = fs::read_to_string(&paths.proc_cmdline)
        .with_context(|| format!("reading {}", paths.proc_cmdline.display()))?;
    let values = content
        .split_whitespace()
        .filter_map(|argument| argument.strip_prefix("dotlab.slot="))
        .collect::<Vec<_>>();
    match values.as_slice() {
        [] => Ok(None),
        [only] if !only.is_empty() => Ok(Some((*only).to_owned())),
        _ => bail!("kernel command line contains ambiguous dotlab.slot arguments"),
    }
}

fn slot_entry_id(slot: &MetalSlot) -> String {
    format!("dotlab-slot-{}", slot.id)
}

fn primary_entry_id(slot: &MetalSlot) -> String {
    // Reuse the original experiment ID. The preserved original root may still
    // carry Dotlab 1.0's generated GRUB script; if it regenerates grub.cfg,
    // the saved default remains resolvable and falls back to the immutable
    // slot UKI instead of becoming a missing entry.
    slot_entry_id(slot)
}

fn last_known_good_entry_id(slot: &MetalSlot) -> String {
    format!("dotlab-last-good-{}", slot.id)
}

fn original_entry_id(slot: &MetalSlot) -> String {
    format!("dotlab-original-{}", slot.id)
}

fn load_state(paths: &AppPaths) -> Result<MetalState> {
    if !paths.metal_index().is_file() {
        return Ok(MetalState::empty());
    }
    let mut state: MetalState = read_json(&paths.metal_index())?;
    if !supported_metal_schema(state.schema) {
        bail!("unsupported metal-state schema {}", state.schema);
    }
    let mut promoted_count = 0;
    for (name, slot) in &state.slots {
        if name != &slot.name {
            bail!("metal slot key and manifest name disagree for {name}");
        }
        validate_slot(paths, slot)?;
        promoted_count += usize::from(slot.promoted);
    }
    if promoted_count > 1 {
        bail!("metal state contains more than one promoted primary");
    }
    // Schema 1 states predate promotion. Upgrade only in memory; the first
    // successful mutation writes schema 2, causing old binaries to fail closed.
    state.schema = METAL_SCHEMA;
    for slot in state.slots.values_mut() {
        slot.schema = METAL_SCHEMA;
    }
    Ok(state)
}

fn save_state(paths: &AppPaths, state: &MetalState) -> Result<()> {
    write_json(&paths.metal_index(), state)
}

fn pending_path(paths: &AppPaths) -> PathBuf {
    paths.metal_state.join("pending.json")
}

fn ready_path(paths: &AppPaths, slot: &MetalSlot) -> PathBuf {
    paths.metal_state.join(format!("ready-{}", slot.id))
}

fn save_pending(paths: &AppPaths, pending: &PendingMetal) -> Result<()> {
    if pending_path(paths).exists() {
        bail!("another metal operation is pending recovery");
    }
    write_json(&pending_path(paths), pending)
}

fn supported_metal_schema(schema: u32) -> bool {
    (1..=METAL_SCHEMA).contains(&schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_rootflags_without_losing_other_options() -> Result<()> {
        let args = vec![
            "root=UUID=abc".to_owned(),
            "rw".to_owned(),
            "rootflags=subvol=/@,compress=zstd:3".to_owned(),
        ];
        let result = slot_kernel_arguments(&args, "@snapshots/dotlab/x/root", "x")?;
        assert!(
            result
                .contains(&"rootflags=compress=zstd:3,subvol=/@snapshots/dotlab/x/root".to_owned())
        );
        assert!(result.contains(&"dotlab.slot=x".to_owned()));
        Ok(())
    }

    #[test]
    fn fstab_options_replace_subvolume_and_remove_subvolid() -> Result<()> {
        assert_eq!(
            replace_subvolume(
                "rw,compress=zstd:3,subvolid=256,subvol=/@",
                "@snapshots/dotlab/x/root"
            )?,
            "rw,compress=zstd:3,subvol=/@snapshots/dotlab/x/root"
        );
        Ok(())
    }

    #[test]
    fn boot_mode_replacement_is_idempotent_and_preserves_other_options() {
        assert_eq!(
            boot_mode_options("rw,fmask=0077,dmask=0077", true),
            "fmask=0077,dmask=0077,ro"
        );
        assert_eq!(
            boot_mode_options("fmask=0077,ro,dmask=0077", false),
            "fmask=0077,dmask=0077,rw"
        );
    }
}
