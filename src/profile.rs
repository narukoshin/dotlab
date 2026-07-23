use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::{
    InitArgs, NameArgs, ProfileAddArgs, ProfileCaptureArgs, ProfileCommand, ProfileRemoveArgs,
    SwitchArgs, SwitchControlArgs,
};
use crate::manifest::{
    ActiveState, BaselineEntry, BaselineIndex, Generation, GenerationMapping, Mapping,
    ProfileManifest, ProfileSource, SCHEMA, Transaction, TransactionOperation, TransactionPhase,
};
use crate::packages;
use crate::paths::{AppPaths, validate_name, validate_relative};
use crate::util::{
    acquire_lock, copy_node, create_symlink, ensure_unprivileged, fingerprint, move_node,
    node_exists, path_key, prompt_yes, read_json, read_link_absolute, read_toml, remove_node,
    run_checked, run_stdout, timestamp, unique_id, validate_tree, write_json, write_toml,
};

const BASE_PROFILE: &str = "base";

pub fn init(paths: &AppPaths, args: InitArgs) -> Result<()> {
    ensure_unprivileged()?;
    paths.ensure_user_dirs()?;
    let _lock = acquire_lock(&paths.user_lock())?;
    recover_pending(paths)?;

    if !node_exists(&paths.profile_manifest(BASE_PROFILE)) {
        prompt_yes(
            "Initialize Dotlab and make its protected minimal Hyprland profile active?",
            args.yes,
        )?;
        create_base_profile(paths)?;
        println!("Created protected profile: {BASE_PROFILE}");
    }

    let active = load_active(paths)?;
    if active.current.as_ref().map(|item| item.profile.as_str()) == Some(BASE_PROFILE) {
        println!("Dotlab is initialized; base is already active.");
        return Ok(());
    }
    switch_to_profile(paths, BASE_PROFILE, false, true)
}

pub fn run(paths: &AppPaths, command: ProfileCommand) -> Result<()> {
    ensure_unprivileged()?;
    paths.ensure_user_dirs()?;
    let _lock = acquire_lock(&paths.user_lock())?;
    recover_pending(paths)?;
    match command {
        ProfileCommand::Add(args) => add(paths, args),
        ProfileCommand::Capture(args) => capture(paths, args),
        ProfileCommand::Show(args) => show(paths, args),
        ProfileCommand::List => list(paths),
        ProfileCommand::Remove(args) => remove(paths, args),
    }
}

pub fn switch(paths: &AppPaths, args: SwitchArgs) -> Result<()> {
    ensure_unprivileged()?;
    paths.ensure_user_dirs()?;
    let _lock = acquire_lock(&paths.user_lock())?;
    recover_pending(paths)?;
    if !args.dry_run {
        prompt_yes(
            &format!("Switch managed dotfiles to profile {:?}?", args.name),
            args.yes,
        )?;
    }
    switch_to_profile(paths, &args.name, args.dry_run, args.next_login)
}

pub fn rollback(paths: &AppPaths, args: SwitchControlArgs) -> Result<()> {
    ensure_unprivileged()?;
    paths.ensure_user_dirs()?;
    let _lock = acquire_lock(&paths.user_lock())?;
    recover_pending(paths)?;
    let mut active = load_active(paths)?;
    let target = active
        .history
        .pop()
        .context("there is no preceding successful generation to roll back to")?;
    verify_active_destinations(paths, active.current.as_ref())?;
    ensure_generation(paths, &target)?;
    check_new_destinations(paths, &target, active.current.as_ref())?;
    if !args.dry_run {
        prompt_yes(
            &format!(
                "Roll managed dotfiles back to generation {} ({})?",
                target.id, target.profile
            ),
            args.yes,
        )?;
    }
    let old_current = active.current.clone();
    active.current = Some(target.clone());
    if args.dry_run {
        print_plan(paths, &target, old_current.as_ref())?;
        return Ok(());
    }
    apply_generation(paths, target.clone(), active)?;
    println!("Rolled back to {} ({})", target.profile, target.id);
    reload_hyprland(false);
    Ok(())
}

fn add(paths: &AppPaths, args: ProfileAddArgs) -> Result<()> {
    validate_name(&args.name)?;
    ensure_profile_absent(paths, &args.name)?;
    validate_git_url(&args.git_url)?;
    for package in &args.packages {
        validate_package_name(package)?;
    }

    let temporary = paths.repos.join(format!(".{}-{}", args.name, unique_id()?));
    let final_repository = paths.repos.join(&args.name);
    let result = (|| -> Result<ProfileManifest> {
        let clone_arguments = vec![
            OsString::from("clone"),
            OsString::from("--recurse-submodules"),
            OsString::from("--"),
            OsString::from(&args.git_url),
            temporary.as_os_str().to_owned(),
        ];
        run_checked("git", clone_arguments).with_context(|| format!("cloning {}", args.git_url))?;
        if let Some(reference) = &args.git_ref {
            if reference.contains(['\n', '\r', '\0']) {
                bail!("git reference contains a control character");
            }
            run_checked(
                "git",
                [
                    OsString::from("-C"),
                    temporary.as_os_str().to_owned(),
                    OsString::from("checkout"),
                    OsString::from("--detach"),
                    OsString::from("--"),
                    OsString::from(reference),
                ],
            )
            .with_context(|| format!("checking out {reference:?}"))?;
        }
        let checkout = run_stdout(
            "git",
            [
                OsString::from("-C"),
                temporary.as_os_str().to_owned(),
                OsString::from("rev-parse"),
                OsString::from("HEAD"),
            ],
        )?;

        let source_prefix = args.source.clone().unwrap_or_default();
        if !source_prefix.as_os_str().is_empty() {
            validate_relative(&source_prefix)?;
        }
        let content_root = temporary.join(&source_prefix);
        if !content_root.is_dir() {
            bail!(
                "profile source is not a directory: {}",
                content_root.display()
            );
        }
        let mappings = if args.mappings.is_empty() {
            detect_mappings(&content_root)?
        } else {
            parse_mappings(&args.mappings)?
        };
        validate_mappings(paths, &args.name, &content_root, &mappings)?;

        // The manifest points at the final repository location, not the
        // temporary clone which is about to be atomically renamed.
        let final_content = final_repository.join(&source_prefix);
        Ok(ProfileManifest {
            schema: SCHEMA,
            name: args.name.clone(),
            protected: false,
            created_at: timestamp()?,
            source: ProfileSource::Git {
                url: args.git_url.clone(),
                checkout,
                directory: final_content,
            },
            mappings,
            packages: sorted_unique(args.packages),
        })
    })();

    let manifest = match result {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = remove_node(&temporary);
            return Err(error);
        }
    };

    fs::rename(&temporary, &final_repository).with_context(|| {
        format!(
            "moving repository into {}",
            final_repository.as_path().display()
        )
    })?;
    if let Err(error) = save_new_manifest(paths, &manifest) {
        let _ = remove_node(&final_repository);
        return Err(error);
    }
    println!(
        "Added profile {} at commit {} with {} mapping(s).",
        manifest.name,
        match &manifest.source {
            ProfileSource::Git { checkout, .. } => checkout,
            _ => unreachable!(),
        },
        manifest.mappings.len()
    );
    println!("Review it with: dotlab profile show {}", manifest.name);
    Ok(())
}

fn capture(paths: &AppPaths, args: ProfileCaptureArgs) -> Result<()> {
    validate_name(&args.name)?;
    ensure_profile_absent(paths, &args.name)?;
    for package in &args.packages {
        validate_package_name(package)?;
    }
    let mut destinations = args.paths;
    destinations.sort();
    destinations.dedup();
    if destinations.is_empty() {
        bail!("at least one --path is required");
    }
    for destination in &destinations {
        validate_relative(destination)?;
    }

    let profile_parent = paths
        .profiles
        .join(format!(".{}-{}", args.name, unique_id()?));
    let content = profile_parent.join("content");
    fs::create_dir_all(content.join("objects"))
        .with_context(|| format!("creating {}", content.display()))?;

    let result = (|| -> Result<ProfileManifest> {
        let mut mappings = Vec::new();
        for destination in destinations {
            let home_source = paths.home_path(&destination)?;
            if !node_exists(&home_source) {
                bail!(
                    "cannot capture absent path {}; remove it from --path",
                    destination.display()
                );
            }
            let actual_source = if fs::symlink_metadata(&home_source)?.file_type().is_symlink() {
                read_link_absolute(&home_source)?
            } else {
                home_source
            };
            validate_tree(&actual_source)?;
            let object = PathBuf::from("objects").join(path_key(&destination));
            copy_node(&actual_source, &content.join(&object))?;
            validate_source(&content, &content.join(&object))?;
            mappings.push(Mapping {
                source: object,
                destination,
            });
        }
        validate_mappings(paths, &args.name, &content, &mappings)?;
        Ok(ProfileManifest {
            schema: SCHEMA,
            name: args.name.clone(),
            protected: false,
            created_at: timestamp()?,
            source: ProfileSource::Captured {
                directory: paths.profile_dir(&args.name).join("content"),
            },
            mappings,
            packages: sorted_unique(args.packages),
        })
    })();

    let manifest = match result {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = remove_node(&profile_parent);
            return Err(error);
        }
    };
    write_toml(&profile_parent.join("profile.toml"), &manifest)?;
    fs::rename(&profile_parent, paths.profile_dir(&args.name))
        .with_context(|| format!("installing profile {}", args.name))?;
    println!(
        "Captured {} path(s) into profile {}.",
        manifest.mappings.len(),
        manifest.name
    );
    Ok(())
}

fn show(paths: &AppPaths, args: NameArgs) -> Result<()> {
    let manifest = load_profile(paths, &args.name)?;
    print!("{}", toml::to_string_pretty(&manifest)?);
    Ok(())
}

fn list(paths: &AppPaths) -> Result<()> {
    let active = load_active(paths)?;
    let active_name = active.current.as_ref().map(|value| value.profile.as_str());
    let mut manifests = load_all_profiles(paths)?;
    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    if manifests.is_empty() {
        println!("No profiles. Run: dotlab init");
        return Ok(());
    }
    for manifest in manifests {
        let marker = if active_name == Some(manifest.name.as_str()) {
            "*"
        } else {
            " "
        };
        let protected = if manifest.protected {
            " [protected]"
        } else {
            ""
        };
        println!(
            "{marker} {}{protected}\t{} mapping(s), {} package(s)",
            manifest.name,
            manifest.mappings.len(),
            manifest.packages.len()
        );
    }
    Ok(())
}

fn remove(paths: &AppPaths, args: ProfileRemoveArgs) -> Result<()> {
    validate_name(&args.name)?;
    let manifest = load_profile(paths, &args.name)?;
    if manifest.protected {
        bail!(
            "profile {:?} is protected; it is the known-good fallback",
            args.name
        );
    }
    let mut active = load_active(paths)?;
    if active.current.as_ref().map(|value| value.profile.as_str()) == Some(args.name.as_str()) {
        bail!(
            "profile {:?} is active; switch to base or another profile first",
            args.name
        );
    }
    prompt_yes(
        &format!(
            "Remove profile {:?}, its clone, and its inactive generations?",
            args.name
        ),
        args.yes,
    )?;

    active
        .history
        .retain(|generation| generation.profile != args.name);
    write_json(&paths.active_state(), &active)?;

    if paths.generations.is_dir() {
        for entry in fs::read_dir(&paths.generations)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let manifest_path = entry.path().join("generation.json");
            if !manifest_path.is_file() {
                continue;
            }
            let generation: Generation = read_json(&manifest_path)?;
            if generation.profile == args.name {
                remove_node(&entry.path())?;
            }
        }
    }
    remove_node(&paths.profile_dir(&args.name))?;
    remove_node(&paths.repos.join(&args.name))?;
    println!("Removed profile {}.", args.name);
    println!("Run `dotlab packages gc --dry-run` to review package cleanup.");
    Ok(())
}

fn switch_to_profile(paths: &AppPaths, name: &str, dry_run: bool, next_login: bool) -> Result<()> {
    validate_name(name)?;
    let manifest = load_profile(paths, name)?;
    let old_active = load_active(paths)?;
    verify_active_destinations(paths, old_active.current.as_ref())?;
    if dry_run {
        let preview = preview_generation(&manifest)?;
        check_new_destinations(paths, &preview, old_active.current.as_ref())?;
        print_plan(paths, &preview, old_active.current.as_ref())?;
        if !manifest.packages.is_empty() {
            println!("Packages required: {}", manifest.packages.join(" "));
        }
        return Ok(());
    }

    packages::install_required(paths, &manifest.packages)?;
    let generation = create_generation(paths, &manifest)?;
    let mut new_active = old_active.clone();
    if let Some(current) = old_active.current {
        new_active.history.push(current);
    }
    new_active.current = Some(generation.clone());
    apply_generation(paths, generation.clone(), new_active)?;
    println!("Active profile: {} ({})", generation.profile, generation.id);
    reload_hyprland(next_login);
    Ok(())
}

fn check_new_destinations(
    paths: &AppPaths,
    generation: &Generation,
    old_generation: Option<&Generation>,
) -> Result<()> {
    let active_destinations: BTreeSet<&Path> = old_generation
        .into_iter()
        .flat_map(|value| value.mappings.iter())
        .map(|mapping| mapping.destination.as_path())
        .collect();
    let baseline = load_baseline(paths)?;
    for mapping in &generation.mappings {
        validate_home_parent_chain(paths, &mapping.destination)?;
        if active_destinations.contains(mapping.destination.as_path()) {
            continue;
        }
        if let Some(entry) = baseline.entries.get(&mapping.destination) {
            let destination = paths.home_path(&mapping.destination)?;
            if fingerprint(&destination)? != entry.fingerprint {
                bail!(
                    "unmanaged path {} differs from its recorded baseline",
                    destination.display()
                );
            }
        }
    }
    Ok(())
}

fn apply_generation(
    paths: &AppPaths,
    generation: Generation,
    new_active: ActiveState,
) -> Result<()> {
    ensure_generation(paths, &generation)?;
    let old_active = load_active(paths)?;
    verify_active_destinations(paths, old_active.current.as_ref())?;
    ensure_baselines(paths, &generation, old_active.current.as_ref())?;

    let desired = desired_nodes(paths, &generation, old_active.current.as_ref())?;
    let mut created_parents = BTreeSet::new();
    for destination in desired.keys() {
        validate_home_parent_chain(paths, destination)?;
        created_parents.extend(missing_parent_directories(paths, destination)?);
    }
    let transaction_id = unique_id()?;
    let transaction_dir = paths.transactions.join(&transaction_id);
    fs::create_dir(&transaction_dir)
        .with_context(|| format!("creating {}", transaction_dir.display()))?;
    let home_device = fs::metadata(&paths.home)?.dev();
    let transaction_device = fs::metadata(&transaction_dir)?.dev();
    if home_device != transaction_device {
        remove_node(&transaction_dir)?;
        bail!("DOTLAB_STATE must be on the same filesystem as HOME so renames are atomic");
    }

    let operations = desired
        .keys()
        .map(|destination| {
            let absolute = paths.home_path(destination)?;
            Ok(TransactionOperation {
                destination: absolute,
                backup: transaction_dir.join("backups").join(path_key(destination)),
                original_present: node_exists(&paths.home_path(destination)?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut transaction = Transaction {
        schema: SCHEMA,
        id: transaction_id,
        phase: TransactionPhase::Prepared,
        old_active,
        new_active: new_active.clone(),
        operations,
        created_parents: created_parents.into_iter().collect(),
    };
    let journal = transaction_dir.join("journal.json");
    write_json(&journal, &transaction)?;

    let apply_result = (|| -> Result<()> {
        for (index, (destination, action)) in desired.into_iter().enumerate() {
            let absolute = paths.home_path(&destination)?;
            let operation = transaction
                .operations
                .iter()
                .find(|operation| operation.destination == absolute)
                .context("internal transaction operation mismatch")?;
            if node_exists(&absolute) {
                move_node(&absolute, &operation.backup)?;
            }
            match action {
                DesiredNode::Link(source) => create_symlink(&source, &absolute)?,
                DesiredNode::Restore(Some(source)) => copy_node(&source, &absolute)?,
                DesiredNode::Restore(None) => {}
            }
            test_failure_injection(index + 1)?;
        }
        cleanup_restored_parents(paths, &generation, transaction.old_active.current.as_ref())?;
        transaction.phase = TransactionPhase::FilesApplied;
        write_json(&journal, &transaction)?;
        write_json(&paths.active_state(), &new_active)?;
        Ok(())
    })();

    if let Err(error) = apply_result {
        let recovery = recover_transaction(paths, &transaction_dir);
        return match recovery {
            Ok(()) => Err(error.context("switch failed; prior files were restored")),
            Err(recovery_error) => Err(anyhow!(
                "switch failed: {error:#}; automatic recovery also failed: {recovery_error:#}. \
                 Do not edit managed paths; rerun any Dotlab command to retry recovery"
            )),
        };
    }
    if let Err(error) = remove_node(&transaction_dir) {
        eprintln!(
            "dotlab: warning: switch succeeded but transaction cleanup was deferred: {error:#}"
        );
    }
    Ok(())
}

fn test_failure_injection(completed_operations: usize) -> Result<()> {
    if std::env::var_os("DOTLAB_TEST_MODE").is_none() {
        return Ok(());
    }
    if std::env::var("DOTLAB_FAIL_AFTER_OP")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        == Some(completed_operations)
    {
        bail!("injected switch failure after operation {completed_operations}");
    }
    if std::env::var("DOTLAB_CRASH_AFTER_OP")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        == Some(completed_operations)
    {
        std::process::exit(99);
    }
    Ok(())
}

#[derive(Clone, Debug)]
enum DesiredNode {
    Link(PathBuf),
    Restore(Option<PathBuf>),
}

fn desired_nodes(
    paths: &AppPaths,
    new_generation: &Generation,
    old_generation: Option<&Generation>,
) -> Result<BTreeMap<PathBuf, DesiredNode>> {
    let mut desired = BTreeMap::new();
    if let Some(old) = old_generation {
        let baseline = load_baseline(paths)?;
        for mapping in &old.mappings {
            if !new_generation
                .mappings
                .iter()
                .any(|new| new.destination == mapping.destination)
            {
                let entry = baseline
                    .entries
                    .get(&mapping.destination)
                    .with_context(|| {
                        format!(
                            "baseline is missing {}",
                            mapping.destination.as_path().display()
                        )
                    })?;
                let source = match (&entry.object, entry.present) {
                    (Some(object), true) => Some(paths.baseline.join(object)),
                    (None, false) => None,
                    _ => bail!(
                        "invalid baseline entry for {}",
                        mapping.destination.display()
                    ),
                };
                desired.insert(mapping.destination.clone(), DesiredNode::Restore(source));
            }
        }
    }
    for mapping in &new_generation.mappings {
        desired.insert(
            mapping.destination.clone(),
            DesiredNode::Link(
                paths
                    .generations
                    .join(&new_generation.id)
                    .join(&mapping.object),
            ),
        );
    }
    Ok(desired)
}

fn cleanup_restored_parents(
    paths: &AppPaths,
    new_generation: &Generation,
    old_generation: Option<&Generation>,
) -> Result<()> {
    let Some(old_generation) = old_generation else {
        return Ok(());
    };
    let baseline = load_baseline(paths)?;
    let mut candidates = BTreeSet::new();
    for mapping in &old_generation.mappings {
        if new_generation
            .mappings
            .iter()
            .any(|new| new.destination == mapping.destination)
        {
            continue;
        }
        let entry = baseline
            .entries
            .get(&mapping.destination)
            .with_context(|| format!("baseline is missing {}", mapping.destination.display()))?;
        for relative in &entry.absent_parents {
            validate_relative(relative)?;
            candidates.insert(paths.home_path(relative)?);
        }
    }
    remove_empty_directories(candidates.iter().rev())
}

fn validate_home_parent_chain(paths: &AppPaths, destination: &Path) -> Result<()> {
    validate_relative(destination)?;
    let home_metadata = fs::symlink_metadata(&paths.home)
        .with_context(|| format!("inspecting HOME {}", paths.home.display()))?;
    if !home_metadata.is_dir() || home_metadata.file_type().is_symlink() {
        bail!("HOME must be a real directory, not a symlink");
    }
    let mut current = paths.home.clone();
    if let Some(parent) = destination.parent() {
        for component in parent.components() {
            let std::path::Component::Normal(component) = component else {
                bail!("unsafe destination {}", destination.display());
            };
            current.push(component);
            if !node_exists(&current) {
                continue;
            }
            let metadata = fs::symlink_metadata(&current)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "destination parent {} is a symlink; refusing an operation that could escape HOME",
                    current.display()
                );
            }
            if !metadata.is_dir() {
                bail!(
                    "destination parent {} is not a directory",
                    current.display()
                );
            }
        }
    }
    Ok(())
}

fn missing_parent_directories(paths: &AppPaths, destination: &Path) -> Result<Vec<PathBuf>> {
    validate_home_parent_chain(paths, destination)?;
    let mut result = Vec::new();
    let mut current = paths.home.clone();
    if let Some(parent) = destination.parent() {
        for component in parent.components() {
            let std::path::Component::Normal(component) = component else {
                bail!("unsafe destination {}", destination.display());
            };
            current.push(component);
            if !node_exists(&current) {
                result.push(current.clone());
            }
        }
    }
    Ok(result)
}

fn remove_empty_directories<'a>(directories: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    for directory in directories {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("removing empty {}", directory.display()));
            }
        }
    }
    Ok(())
}

fn ensure_baselines(
    paths: &AppPaths,
    generation: &Generation,
    old_generation: Option<&Generation>,
) -> Result<()> {
    let old_destinations: BTreeSet<&Path> = old_generation
        .into_iter()
        .flat_map(|value| value.mappings.iter())
        .map(|mapping| mapping.destination.as_path())
        .collect();
    let mut baseline = load_baseline(paths)?;
    let objects = paths.baseline.join("objects");
    fs::create_dir_all(&objects)?;

    for mapping in &generation.mappings {
        validate_home_parent_chain(paths, &mapping.destination)?;
        if old_destinations.contains(mapping.destination.as_path()) {
            if !baseline.entries.contains_key(&mapping.destination) {
                bail!(
                    "baseline invariant failed for currently managed {}",
                    mapping.destination.display()
                );
            }
            continue;
        }
        if let Some(entry) = baseline.entries.get(&mapping.destination) {
            let destination = paths.home_path(&mapping.destination)?;
            let current_fingerprint = fingerprint(&destination)?;
            if current_fingerprint != entry.fingerprint {
                bail!(
                    "unmanaged path {} changed after its baseline was recorded; refusing to \
                     overwrite it (move it aside or capture it as a new profile first)",
                    destination.display()
                );
            }
            continue;
        }
        let destination = paths.home_path(&mapping.destination)?;
        let absent_parents = missing_parent_directories(paths, &mapping.destination)?
            .into_iter()
            .filter_map(|path| path.strip_prefix(&paths.home).ok().map(Path::to_path_buf))
            .collect();
        let present = node_exists(&destination);
        let key = path_key(&mapping.destination);
        let object = PathBuf::from("objects").join(&key);
        let stored = paths.baseline.join(&object);
        if node_exists(&stored) {
            // This can only be an orphan left before its index write.
            remove_node(&stored)?;
        }
        let original_fingerprint = fingerprint(&destination)?;
        if present {
            validate_tree(&destination)?;
            copy_node(&destination, &stored)?;
        }
        baseline.entries.insert(
            mapping.destination.clone(),
            BaselineEntry {
                present,
                object: present.then_some(object),
                fingerprint: original_fingerprint,
                absent_parents,
            },
        );
        write_json(&paths.baseline_index(), &baseline)?;
    }
    Ok(())
}

fn verify_active_destinations(paths: &AppPaths, active: Option<&Generation>) -> Result<()> {
    let Some(active) = active else {
        return Ok(());
    };
    ensure_generation(paths, active)?;
    for mapping in &active.mappings {
        validate_home_parent_chain(paths, &mapping.destination)?;
        let destination = paths.home_path(&mapping.destination)?;
        let expected = paths.generations.join(&active.id).join(&mapping.object);
        let metadata = fs::symlink_metadata(&destination).with_context(|| {
            format!(
                "managed path {} disappeared; refusing to overwrite drift",
                destination.display()
            )
        })?;
        if !metadata.file_type().is_symlink() {
            bail!(
                "managed path {} is no longer Dotlab's symlink; refusing to overwrite it",
                destination.display()
            );
        }
        let actual = read_link_absolute(&destination)?;
        if actual != expected {
            bail!(
                "managed link {} points to {}, expected {}; refusing to overwrite drift",
                destination.display(),
                actual.display(),
                expected.display()
            );
        }
        if !node_exists(&expected) {
            bail!(
                "generation object is missing: {}; restore Dotlab state before switching",
                expected.display()
            );
        }
    }
    Ok(())
}

fn create_generation(paths: &AppPaths, profile: &ProfileManifest) -> Result<Generation> {
    let id = format!("{}-{}", profile.name, unique_id()?);
    let temporary = paths.generations.join(format!(".{id}"));
    let final_directory = paths.generations.join(&id);
    fs::create_dir(&temporary)?;
    let result = (|| -> Result<Generation> {
        let mut generated = Vec::new();
        for mapping in &profile.mappings {
            let source = profile.source.directory().join(&mapping.source);
            validate_source(profile.source.directory(), &source)?;
            let copy_source = if fs::symlink_metadata(&source)?.file_type().is_symlink() {
                fs::canonicalize(&source)?
            } else {
                source.clone()
            };
            let object = PathBuf::from("objects").join(path_key(&mapping.destination));
            let destination = temporary.join(&object);
            copy_node(&copy_source, &destination)?;
            generated.push(GenerationMapping {
                destination: mapping.destination.clone(),
                object,
                fingerprint: fingerprint(&copy_source)?,
            });
        }
        let generation = Generation {
            schema: SCHEMA,
            id: id.clone(),
            profile: profile.name.clone(),
            created_at: timestamp()?,
            mappings: generated,
        };
        write_json(&temporary.join("generation.json"), &generation)?;
        Ok(generation)
    })();
    let generation = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = remove_node(&temporary);
            return Err(error);
        }
    };
    fs::rename(&temporary, &final_directory)
        .with_context(|| format!("publishing generation {id}"))?;
    Ok(generation)
}

fn preview_generation(profile: &ProfileManifest) -> Result<Generation> {
    Ok(Generation {
        schema: SCHEMA,
        id: "<new-generation>".to_owned(),
        profile: profile.name.clone(),
        created_at: timestamp()?,
        mappings: profile
            .mappings
            .iter()
            .map(|mapping| GenerationMapping {
                destination: mapping.destination.clone(),
                object: PathBuf::from("objects").join(path_key(&mapping.destination)),
                fingerprint: "<calculated-at-switch>".to_owned(),
            })
            .collect(),
    })
}

fn print_plan(
    paths: &AppPaths,
    generation: &Generation,
    old_generation: Option<&Generation>,
) -> Result<()> {
    println!("Dry-run plan for profile {}:", generation.profile);
    let old_destinations: BTreeSet<&Path> = old_generation
        .into_iter()
        .flat_map(|value| value.mappings.iter())
        .map(|mapping| mapping.destination.as_path())
        .collect();
    for mapping in &generation.mappings {
        let marker = if old_destinations.contains(mapping.destination.as_path()) {
            "replace"
        } else {
            "save baseline, then link"
        };
        println!(
            "  {marker}: {}",
            paths.home_path(&mapping.destination)?.display()
        );
    }
    if let Some(old) = old_generation {
        for mapping in &old.mappings {
            if !generation
                .mappings
                .iter()
                .any(|item| item.destination == mapping.destination)
            {
                println!(
                    "  restore baseline: {}",
                    paths.home_path(&mapping.destination)?.display()
                );
            }
        }
    }
    Ok(())
}

fn recover_pending(paths: &AppPaths) -> Result<()> {
    if !paths.transactions.is_dir() {
        return Ok(());
    }
    let mut entries =
        fs::read_dir(&paths.transactions)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if entry.file_type()?.is_dir() && entry.path().join("journal.json").is_file() {
            eprintln!(
                "dotlab: recovering interrupted transaction {}",
                entry.file_name().to_string_lossy()
            );
            recover_transaction(paths, &entry.path())?;
        }
    }
    Ok(())
}

fn recover_transaction(paths: &AppPaths, transaction_dir: &Path) -> Result<()> {
    let transaction: Transaction = read_json(&transaction_dir.join("journal.json"))?;
    if transaction.schema != SCHEMA {
        bail!(
            "unsupported transaction schema {} in {}",
            transaction.schema,
            transaction_dir.display()
        );
    }
    for operation in &transaction.operations {
        let destination_relative = operation.destination.strip_prefix(&paths.home);
        let backup_relative = operation.backup.strip_prefix(transaction_dir);
        if destination_relative
            .ok()
            .is_none_or(|relative| validate_relative(relative).is_err())
            || backup_relative
                .ok()
                .is_none_or(|relative| validate_relative(relative).is_err())
        {
            bail!(
                "unsafe path in transaction {}; refusing recovery",
                transaction.id
            );
        }
    }
    for parent in &transaction.created_parents {
        if parent
            .strip_prefix(&paths.home)
            .ok()
            .is_none_or(|relative| validate_relative(relative).is_err())
        {
            bail!(
                "unsafe created-parent path in transaction {}; refusing recovery",
                transaction.id
            );
        }
    }

    let active = load_active(paths)?;
    let new_id = transaction
        .new_active
        .current
        .as_ref()
        .map(|value| value.id.as_str());
    let active_id = active.current.as_ref().map(|value| value.id.as_str());
    if transaction.phase == TransactionPhase::FilesApplied && active_id == new_id {
        remove_node(transaction_dir)?;
        return Ok(());
    }

    for operation in transaction.operations.iter().rev() {
        if operation.original_present {
            if node_exists(&operation.backup) {
                remove_node(&operation.destination)?;
                move_node(&operation.backup, &operation.destination)?;
            } else if !node_exists(&operation.destination) {
                bail!(
                    "cannot recover {}: both original and backup are missing",
                    operation.destination.display()
                );
            }
        } else {
            remove_node(&operation.destination)?;
        }
    }
    remove_empty_directories(transaction.created_parents.iter().rev())?;
    write_json(&paths.active_state(), &transaction.old_active)?;
    remove_node(transaction_dir)?;
    Ok(())
}

fn create_base_profile(paths: &AppPaths) -> Result<()> {
    let temporary = paths
        .profiles
        .join(format!(".{BASE_PROFILE}-{}", unique_id()?));
    let content = temporary.join("content");
    let hypr = content.join("hypr");
    fs::create_dir_all(&hypr)?;
    crate::util::atomic_write(
        &hypr.join("hyprland.lua"),
        BASE_HYPRLAND_LUA.as_bytes(),
        0o644,
    )?;
    let manifest = ProfileManifest {
        schema: SCHEMA,
        name: BASE_PROFILE.to_owned(),
        protected: true,
        created_at: timestamp()?,
        source: ProfileSource::Builtin {
            directory: paths.profile_dir(BASE_PROFILE).join("content"),
        },
        mappings: vec![Mapping {
            source: PathBuf::from("hypr"),
            destination: PathBuf::from(".config/hypr"),
        }],
        packages: vec!["fuzzel".to_owned(), "kitty".to_owned()],
    };
    write_toml(&temporary.join("profile.toml"), &manifest)?;
    fs::rename(&temporary, paths.profile_dir(BASE_PROFILE))?;
    Ok(())
}

fn parse_mappings(values: &[String]) -> Result<Vec<Mapping>> {
    let mut mappings = Vec::new();
    for value in values {
        let (source, destination) = value
            .split_once('=')
            .with_context(|| format!("mapping {value:?} must be SRC=DEST"))?;
        let source = PathBuf::from(source);
        let destination = PathBuf::from(destination);
        validate_relative(&source)?;
        validate_relative(&destination)?;
        mappings.push(Mapping {
            source,
            destination,
        });
    }
    Ok(mappings)
}

fn detect_mappings(root: &Path) -> Result<Vec<Mapping>> {
    let mut mappings = Vec::new();
    for config_prefix in [
        Path::new(".config"),
        Path::new("dotfiles/.config"),
        Path::new("home/.config"),
    ] {
        let config = root.join(config_prefix);
        if config.is_dir() {
            let mut entries = fs::read_dir(&config)?.collect::<std::result::Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let name = entry.file_name();
                mappings.push(Mapping {
                    source: config_prefix.join(&name),
                    destination: PathBuf::from(".config").join(name),
                });
            }
        }
    }
    for home_prefix in [Path::new(""), Path::new("dotfiles"), Path::new("home")] {
        for name in [
            ".bash_profile",
            ".bashrc",
            ".gitconfig",
            ".profile",
            ".zprofile",
            ".zshenv",
            ".zshrc",
        ] {
            let source = home_prefix.join(name);
            if node_exists(&root.join(&source))
                && !mappings.iter().any(|item| item.source == source)
            {
                mappings.push(Mapping {
                    source,
                    destination: PathBuf::from(name),
                });
            }
        }
        let local_bin = home_prefix.join(".local/bin");
        if root.join(&local_bin).is_dir() {
            mappings.push(Mapping {
                source: local_bin,
                destination: PathBuf::from(".local/bin"),
            });
        }
    }
    if mappings.is_empty() {
        for name in [
            "alacritty",
            "btop",
            "cava",
            "fastfetch",
            "foot",
            "fuzzel",
            "hypr",
            "kitty",
            "mako",
            "nvim",
            "rofi",
            "starship",
            "swaync",
            "waybar",
            "wlogout",
            "yazi",
        ] {
            if root.join(name).is_dir() {
                mappings.push(Mapping {
                    source: PathBuf::from(name),
                    destination: PathBuf::from(".config").join(name),
                });
            }
        }
    }
    mappings.sort_by(|left, right| left.destination.cmp(&right.destination));
    for pair in mappings.windows(2) {
        if pair[0].destination == pair[1].destination {
            bail!(
                "auto-detection found multiple sources for {}; pass --source or explicit --map",
                pair[0].destination.display()
            );
        }
    }
    if mappings.is_empty() {
        bail!("could not safely detect dotfiles; pass --source DIR or one or more --map SRC=DEST");
    }
    Ok(mappings)
}

fn validate_mappings(
    paths: &AppPaths,
    new_name: &str,
    content_root: &Path,
    mappings: &[Mapping],
) -> Result<()> {
    if mappings.is_empty() {
        bail!("a profile must manage at least one path");
    }
    let mut destinations = BTreeSet::new();
    for mapping in mappings {
        validate_relative(&mapping.source)?;
        validate_relative(&mapping.destination)?;
        validate_destination(paths, &mapping.destination)?;
        validate_home_parent_chain(paths, &mapping.destination)?;
        if !destinations.insert(mapping.destination.clone()) {
            bail!("duplicate destination {}", mapping.destination.display());
        }
        validate_source(content_root, &content_root.join(&mapping.source))?;
    }
    let destinations: Vec<&Path> = destinations.iter().map(PathBuf::as_path).collect();
    for (index, left) in destinations.iter().enumerate() {
        for right in destinations.iter().skip(index + 1) {
            if paths_overlap(left, right) {
                bail!(
                    "overlapping destinations {} and {} cannot be swapped atomically",
                    left.display(),
                    right.display()
                );
            }
        }
    }
    for existing in load_all_profiles(paths)? {
        if existing.name == new_name {
            continue;
        }
        for new in mappings {
            for old in &existing.mappings {
                if new.destination != old.destination
                    && paths_overlap(&new.destination, &old.destination)
                {
                    bail!(
                        "{} overlaps {} from profile {}; use the same destination boundary",
                        new.destination.display(),
                        old.destination.display(),
                        existing.name
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_destination(paths: &AppPaths, destination: &Path) -> Result<()> {
    const SENSITIVE: &[&str] = &[
        ".aws",
        ".gnupg",
        ".kube",
        ".local/share/keyrings",
        ".password-store",
        ".pki",
        ".ssh",
    ];
    for sensitive in SENSITIVE {
        let sensitive = Path::new(sensitive);
        if paths_overlap(destination, sensitive) {
            bail!(
                "refusing sensitive destination {}; manage it outside Dotlab",
                destination.display()
            );
        }
    }
    for internal in [&paths.data, &paths.state] {
        if let Ok(relative) = internal.strip_prefix(&paths.home) {
            if paths_overlap(destination, relative) {
                bail!(
                    "destination {} overlaps Dotlab's own state {}",
                    destination.display(),
                    relative.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_source(root: &Path, source: &Path) -> Result<()> {
    if !node_exists(source) {
        bail!("mapping source does not exist: {}", source.display());
    }
    validate_tree(source)?;
    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    let canonical_source =
        fs::canonicalize(source).with_context(|| format!("resolving {}", source.display()))?;
    if !canonical_source.starts_with(&canonical_root) {
        bail!(
            "mapping source escapes its profile through a symlink: {}",
            source.display()
        );
    }
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            let resolved = fs::canonicalize(entry.path())
                .with_context(|| format!("resolving symlink {}", entry.path().display()))?;
            if !resolved.starts_with(&canonical_root) {
                bail!(
                    "symlink escapes profile content: {} -> {}",
                    entry.path().display(),
                    resolved.display()
                );
            }
            if entry.path() != source && !resolved.starts_with(&canonical_source) {
                bail!(
                    "symlink leaves its mapped source subtree and would break when isolated: {} -> {}",
                    entry.path().display(),
                    resolved.display()
                );
            }
        }
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_git_url(url: &str) -> Result<()> {
    if url.is_empty() || url.contains(['\n', '\r', '\0']) || url.starts_with('-') {
        bail!("unsafe or invalid git URL/path");
    }
    Ok(())
}

fn validate_package_name(package: &str) -> Result<()> {
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

fn sorted_unique(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn save_new_manifest(paths: &AppPaths, manifest: &ProfileManifest) -> Result<()> {
    let directory = paths.profile_dir(&manifest.name);
    fs::create_dir(&directory)
        .with_context(|| format!("creating profile directory {}", directory.display()))?;
    if let Err(error) = write_toml(&directory.join("profile.toml"), manifest) {
        let _ = remove_node(&directory);
        return Err(error);
    }
    Ok(())
}

fn ensure_profile_absent(paths: &AppPaths, name: &str) -> Result<()> {
    if node_exists(&paths.profile_dir(name)) || node_exists(&paths.repos.join(name)) {
        bail!(
            "profile or repository {:?} already exists; remove it explicitly first",
            name
        );
    }
    Ok(())
}

fn load_profile(paths: &AppPaths, name: &str) -> Result<ProfileManifest> {
    validate_name(name)?;
    let manifest: ProfileManifest = read_toml(&paths.profile_manifest(name))
        .with_context(|| format!("profile {name:?} does not exist"))?;
    if manifest.schema != SCHEMA {
        bail!(
            "profile {name:?} uses unsupported schema {}",
            manifest.schema
        );
    }
    if manifest.name != name {
        bail!("profile directory and manifest name disagree for {name:?}");
    }
    Ok(manifest)
}

fn load_all_profiles(paths: &AppPaths) -> Result<Vec<ProfileManifest>> {
    if !paths.profiles.is_dir() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(&paths.profiles)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let manifest = entry.path().join("profile.toml");
        if manifest.is_file() {
            result.push(read_toml(&manifest)?);
        }
    }
    Ok(result)
}

fn load_active(paths: &AppPaths) -> Result<ActiveState> {
    if !node_exists(&paths.active_state()) {
        return Ok(ActiveState::empty());
    }
    let state: ActiveState = read_json(&paths.active_state())?;
    if state.schema != SCHEMA {
        bail!("unsupported active-state schema {}", state.schema);
    }
    Ok(state)
}

fn load_baseline(paths: &AppPaths) -> Result<BaselineIndex> {
    if !node_exists(&paths.baseline_index()) {
        return Ok(BaselineIndex::empty());
    }
    let baseline: BaselineIndex = read_json(&paths.baseline_index())?;
    if baseline.schema != SCHEMA {
        bail!("unsupported baseline schema {}", baseline.schema);
    }
    Ok(baseline)
}

fn ensure_generation(paths: &AppPaths, generation: &Generation) -> Result<()> {
    if generation.schema != SCHEMA {
        bail!("unsupported generation schema {}", generation.schema);
    }
    let directory = paths.generations.join(&generation.id);
    if !directory.is_dir() || !directory.join("generation.json").is_file() {
        bail!("generation {} is missing or incomplete", generation.id);
    }
    for mapping in &generation.mappings {
        validate_relative(&mapping.destination)?;
        validate_relative(&mapping.object)?;
        let object = directory.join(&mapping.object);
        if !node_exists(&object) {
            bail!("generation object is missing: {}", object.display());
        }
    }
    Ok(())
}

fn reload_hyprland(next_login: bool) {
    if next_login || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
        println!("The new profile will be fully applied at the next Hyprland login.");
        return;
    }
    match Command::new("hyprctl").arg("reload").status() {
        Ok(status) if status.success() => println!("Reloaded Hyprland."),
        Ok(status) => eprintln!(
            "dotlab: warning: hyprctl reload exited with {status}; log out if the theme is inconsistent"
        ),
        Err(error) => eprintln!(
            "dotlab: warning: could not start hyprctl ({error}); log out to finish applying the theme"
        ),
    }
}

const BASE_HYPRLAND_LUA: &str = r#"-- Dotlab protected fallback profile for Hyprland 0.55+
-- Intentionally small: no bar, daemon bundle, plugins, or AI integration.

hl.monitor({
    output = "",
    mode = "preferred",
    position = "auto",
    scale = "auto",
})

local terminal = "kitty"
local menu = "fuzzel"
local mainMod = "SUPER"

hl.env("XCURSOR_SIZE", "24")
hl.env("HYPRCURSOR_SIZE", "24")

hl.config({
    general = {
        gaps_in = 5,
        gaps_out = 10,
        border_size = 2,
        layout = "dwindle",
        col = {
            active_border = "rgba(7aa2f7ee)",
            inactive_border = "rgba(414868aa)",
        },
    },
    decoration = {
        rounding = 8,
        active_opacity = 1.0,
        inactive_opacity = 0.96,
        shadow = { enabled = true, range = 4, render_power = 3 },
        blur = { enabled = true, size = 4, passes = 2 },
    },
    animations = { enabled = true },
    input = {
        kb_layout = "us",
        follow_mouse = 1,
        touchpad = { natural_scroll = true },
    },
    dwindle = { preserve_split = true },
    misc = {
        force_default_wallpaper = -1,
        disable_hyprland_logo = false,
    },
})

hl.bind(mainMod .. " + Q", hl.dsp.exec_cmd(terminal))
hl.bind(mainMod .. " + R", hl.dsp.exec_cmd(menu))
hl.bind(mainMod .. " + C", hl.dsp.window.close())
hl.bind(mainMod .. " + V", hl.dsp.window.float({ action = "toggle" }))
hl.bind(mainMod .. " + M", hl.dsp.exec_cmd("command -v hyprshutdown >/dev/null 2>&1 && hyprshutdown || hyprctl dispatch 'hl.dsp.exit()'"))

hl.bind(mainMod .. " + left", hl.dsp.focus({ direction = "left" }))
hl.bind(mainMod .. " + right", hl.dsp.focus({ direction = "right" }))
hl.bind(mainMod .. " + up", hl.dsp.focus({ direction = "up" }))
hl.bind(mainMod .. " + down", hl.dsp.focus({ direction = "down" }))

for i = 1, 10 do
    local key = i % 10
    hl.bind(mainMod .. " + " .. key, hl.dsp.focus({ workspace = i }))
    hl.bind(mainMod .. " + SHIFT + " .. key, hl.dsp.window.move({ workspace = i }))
end

hl.bind(mainMod .. " + mouse:272", hl.dsp.window.drag(), { mouse = true })
hl.bind(mainMod .. " + mouse:273", hl.dsp.window.resize(), { mouse = true })

hl.bind("XF86AudioRaiseVolume", hl.dsp.exec_cmd("wpctl set-volume -l 1 @DEFAULT_AUDIO_SINK@ 5%+"), { locked = true, repeating = true })
hl.bind("XF86AudioLowerVolume", hl.dsp.exec_cmd("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"), { locked = true, repeating = true })
hl.bind("XF86AudioMute", hl.dsp.exec_cmd("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"), { locked = true })
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_paths() -> Result<AppPaths> {
        let temporary = tempdir()?;
        let root = temporary.keep();
        let home = root.join("home");
        fs::create_dir(&home)?;
        let data = root.join("data");
        let state = root.join("state");
        Ok(AppPaths {
            home,
            repos: data.join("repos"),
            profiles: data.join("profiles"),
            generations: data.join("generations"),
            baseline: state.join("baseline"),
            transactions: state.join("transactions"),
            metal_state: root.join("metal"),
            snapshot_root: root.join("snapshots"),
            boot: root.join("boot"),
            proc_cmdline: root.join("proc-cmdline"),
            grub_defaults: root.join("grub-defaults"),
            grub_script: root.join("41_dotlab"),
            grub_config: root.join("grub.cfg"),
            pacman_lock: root.join("pacman.lock"),
            data,
            state,
        })
    }

    #[test]
    fn detects_xdg_and_shell_files() -> Result<()> {
        let temporary = tempdir()?;
        fs::create_dir_all(temporary.path().join(".config/hypr"))?;
        fs::write(temporary.path().join(".zshrc"), "hello")?;
        let mappings = detect_mappings(temporary.path())?;
        assert!(
            mappings
                .iter()
                .any(|item| item.destination == Path::new(".config/hypr"))
        );
        assert!(
            mappings
                .iter()
                .any(|item| item.destination == Path::new(".zshrc"))
        );
        Ok(())
    }

    #[test]
    fn refuses_hierarchical_mapping_overlap() -> Result<()> {
        assert!(paths_overlap(
            Path::new(".config"),
            Path::new(".config/hypr")
        ));
        assert!(!paths_overlap(
            Path::new(".config/hypr"),
            Path::new(".config/waybar")
        ));
        Ok(())
    }

    #[test]
    fn rejects_sensitive_destination() -> Result<()> {
        let paths = test_paths()?;
        assert!(validate_destination(&paths, Path::new(".ssh")).is_err());
        assert!(validate_destination(&paths, Path::new(".config/hypr")).is_ok());
        Ok(())
    }

    #[test]
    fn rejects_repository_symlink_leaving_mapping() -> Result<()> {
        let temporary = tempdir()?;
        let mapped = temporary.path().join("mapped");
        let sibling = temporary.path().join("sibling");
        fs::create_dir(&mapped)?;
        fs::create_dir(&sibling)?;
        fs::write(sibling.join("colors"), "secret")?;
        std::os::unix::fs::symlink("../sibling/colors", mapped.join("colors"))?;
        assert!(validate_source(temporary.path(), &mapped).is_err());
        Ok(())
    }
}
