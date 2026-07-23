use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{PackageGcArgs, PackagesCommand};
use crate::manifest::{PackageState, ProfileManifest};
use crate::paths::AppPaths;
use crate::util::{
    acquire_lock, ensure_unprivileged, node_exists, prompt_yes, read_json, read_toml, remove_node,
    run_checked, write_json,
};

pub fn run(paths: &AppPaths, command: PackagesCommand) -> Result<()> {
    ensure_unprivileged()?;
    paths.ensure_user_dirs()?;
    let _lock = acquire_lock(&paths.user_lock())?;
    recover_pending(paths)?;
    match command {
        PackagesCommand::Status => status(paths),
        PackagesCommand::Keep(args) => keep(paths, &args.package),
        PackagesCommand::Gc(args) => gc(paths, args),
    }
}

pub fn install_required(paths: &AppPaths, packages: &[String]) -> Result<()> {
    recover_pending(paths)?;
    if packages.is_empty() {
        return Ok(());
    }
    for package in packages {
        validate_package(package)?;
    }
    let mut state = load_state(paths)?;
    let before = installed_set()?;
    let absent_before: Vec<String> = packages
        .iter()
        .filter(|package| !before.contains(*package))
        .cloned()
        .collect();
    if absent_before.is_empty() {
        return Ok(());
    }

    println!("Installing profile packages: {}", absent_before.join(" "));
    let pending = PackagePending {
        schema: crate::manifest::SCHEMA,
        before: before.clone(),
        requested: absent_before.clone(),
    };
    write_json(&pending_path(paths), &pending)?;
    let mut arguments = vec![
        OsString::from("pacman"),
        OsString::from("-S"),
        OsString::from("--needed"),
        OsString::from("--noconfirm"),
        OsString::from("--"),
    ];
    arguments.extend(absent_before.iter().map(OsString::from));
    let install_result = run_checked("sudo", arguments);
    if std::env::var_os("DOTLAB_TEST_MODE").is_some()
        && std::env::var_os("DOTLAB_CRASH_AFTER_PACMAN").is_some()
    {
        std::process::exit(98);
    }

    let tracking_result = finalize_pending(paths, &mut state, &pending);
    if let Err(error) = tracking_result {
        return Err(error
            .context("could not finalize package ownership; the recovery journal was retained"));
    }
    install_result.map(|_| ())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PackagePending {
    schema: u32,
    before: BTreeSet<String>,
    requested: Vec<String>,
}

fn pending_path(paths: &AppPaths) -> std::path::PathBuf {
    paths.state.join("package-pending.json")
}

fn recover_pending(paths: &AppPaths) -> Result<()> {
    let path = pending_path(paths);
    if !path.is_file() {
        return Ok(());
    }
    let pending: PackagePending = read_json(&path)?;
    if pending.schema != crate::manifest::SCHEMA {
        bail!("unsupported pending package schema {}", pending.schema);
    }
    eprintln!("dotlab: recovering interrupted package ownership journal");
    let mut state = load_state(paths)?;
    finalize_pending(paths, &mut state, &pending)
}

fn finalize_pending(
    paths: &AppPaths,
    state: &mut PackageState,
    pending: &PackagePending,
) -> Result<()> {
    let after = installed_set()?;
    state
        .introduced
        .extend(after.difference(&pending.before).cloned());
    for package in &pending.requested {
        if !pending.before.contains(package) && after.contains(package) {
            state.roots.insert(package.clone());
        }
    }
    save_state(paths, state)?;
    remove_node(&pending_path(paths))?;
    Ok(())
}

fn status(paths: &AppPaths) -> Result<()> {
    let state = load_state(paths)?;
    let referenced = referenced_packages(paths)?;
    if state.introduced.is_empty() && state.kept.is_empty() {
        println!("No packages are owned by Dotlab.");
        return Ok(());
    }
    for package in state.introduced.union(&state.kept) {
        let ownership = if state.kept.contains(package) {
            "kept"
        } else if state.roots.contains(package) && referenced.contains(package) {
            "referenced root"
        } else if state.roots.contains(package) {
            "unreferenced root"
        } else {
            "introduced dependency"
        };
        let present = if installed(package).unwrap_or(false) {
            "installed"
        } else {
            "absent"
        };
        println!("{package}\t{ownership}\t{present}");
    }
    Ok(())
}

fn keep(paths: &AppPaths, package: &str) -> Result<()> {
    validate_package(package)?;
    let mut state = load_state(paths)?;
    if installed(package)? {
        run_checked("sudo", ["pacman", "-D", "--asexplicit", "--", package])
            .with_context(|| format!("marking {package} as explicitly installed"))?;
    }
    state.kept.insert(package.to_owned());
    save_state(paths, &state)?;
    println!("{package} is now user-owned and will never be garbage-collected by Dotlab.");
    Ok(())
}

fn gc(paths: &AppPaths, args: PackageGcArgs) -> Result<()> {
    let mut state = load_state(paths)?;
    let referenced = referenced_packages(paths)?;
    let installed_now = installed_set()?;
    let orphans = orphan_set()?;
    let mut candidates: BTreeSet<String> = state
        .roots
        .iter()
        .filter(|package| {
            !state.kept.contains(*package)
                && !referenced.contains(*package)
                && installed_now.contains(*package)
        })
        .cloned()
        .collect();
    candidates.extend(
        state
            .introduced
            .intersection(&orphans)
            .filter(|package| !state.kept.contains(*package))
            .cloned(),
    );

    if candidates.is_empty() {
        println!("No unreferenced Dotlab packages to remove.");
        state
            .introduced
            .retain(|package| installed_now.contains(package));
        state
            .roots
            .retain(|package| installed_now.contains(package));
        save_state(paths, &state)?;
        return Ok(());
    }
    println!("Dotlab-introduced packages eligible for removal:");
    for package in &candidates {
        println!("  {package}");
    }
    if args.dry_run {
        return Ok(());
    }
    prompt_yes(
        "Remove these packages and now-unused dependencies with pacman -Rns?",
        args.yes,
    )?;
    let mut arguments = vec![
        OsString::from("pacman"),
        OsString::from("-Rns"),
        OsString::from("--noconfirm"),
        OsString::from("--"),
    ];
    arguments.extend(candidates.iter().map(OsString::from));
    run_checked("sudo", arguments)?;
    let remaining = installed_set()?;
    state
        .introduced
        .retain(|package| remaining.contains(package));
    state.roots.retain(|package| remaining.contains(package));
    save_state(paths, &state)?;
    Ok(())
}

pub fn referenced_packages(paths: &AppPaths) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    if !paths.profiles.is_dir() {
        return Ok(result);
    }
    for entry in fs::read_dir(&paths.profiles)
        .with_context(|| format!("reading {}", paths.profiles.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("profile.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let manifest: ProfileManifest = read_toml(&manifest_path)?;
        result.extend(manifest.packages);
    }
    Ok(result)
}

fn installed(package: &str) -> Result<bool> {
    let output = std::process::Command::new("pacman")
        .args(["-Qq", "--", package])
        .output()
        .with_context(|| "starting pacman; Dotlab package management requires Arch Linux")?;
    Ok(output.status.success())
}

fn installed_set() -> Result<BTreeSet<String>> {
    let output = std::process::Command::new("pacman")
        .arg("-Qq")
        .output()
        .with_context(|| "starting pacman; Dotlab package management requires Arch Linux")?;
    if !output.status.success() {
        bail!(
            "pacman -Qq failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text =
        String::from_utf8(output.stdout).context("pacman returned non-UTF-8 package names")?;
    Ok(text.lines().map(ToOwned::to_owned).collect())
}

fn orphan_set() -> Result<BTreeSet<String>> {
    let output = std::process::Command::new("pacman")
        .arg("-Qdtq")
        .output()
        .context("starting pacman -Qdtq")?;
    // Pacman may use a non-zero status to report an empty query.
    if !output.status.success() && !output.stdout.is_empty() {
        bail!(
            "pacman -Qdtq failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text =
        String::from_utf8(output.stdout).context("pacman returned non-UTF-8 package names")?;
    Ok(text.lines().map(ToOwned::to_owned).collect())
}

fn load_state(paths: &AppPaths) -> Result<PackageState> {
    if node_exists(&paths.package_state()) {
        let state: PackageState = read_json(&paths.package_state())?;
        if state.schema != crate::manifest::SCHEMA {
            bail!("unsupported package-state schema {}", state.schema);
        }
        Ok(state)
    } else {
        Ok(PackageState::empty())
    }
}

fn save_state(paths: &AppPaths, state: &PackageState) -> Result<()> {
    write_json(&paths.package_state(), state)
}

fn validate_package(package: &str) -> Result<()> {
    if package.is_empty()
        || package.len() > 128
        || package.starts_with('-')
        || !package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@._+:-".contains(&byte))
    {
        bail!("unsafe or invalid Arch package name: {package:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_name_validation() {
        assert!(validate_package("xdg-desktop-portal-hyprland").is_ok());
        assert!(validate_package("lib32-foo").is_ok());
        assert!(validate_package("--root").is_err());
        assert!(validate_package("foo bar").is_err());
    }
}
