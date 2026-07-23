use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tempfile::TempDir;

struct Sandbox {
    root: TempDir,
    home: PathBuf,
    data: PathBuf,
    state: PathBuf,
    fakebin: PathBuf,
    package_db: PathBuf,
}

impl Sandbox {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let data = root.path().join("data");
        let state = root.path().join("state");
        let fakebin = root.path().join("fakebin");
        let package_db = root.path().join("packages");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&fakebin)?;
        fs::write(&package_db, "")?;
        executable(&fakebin.join("sudo"), "#!/bin/sh\nset -eu\nexec \"$@\"\n")?;
        executable(
            &fakebin.join("pacman"),
            r#"#!/bin/sh
set -eu
db=${DOTLAB_FAKE_PACKAGE_DB:?}
command=${1:?}
shift
case "$command" in
  -Qq)
    if [ "$#" -eq 0 ]; then
      cat "$db"
    else
      [ "${1:-}" = "--" ] && shift
      grep -Fxq -- "${1:?}" "$db"
    fi
    ;;
  -Qdtq)
    while IFS= read -r package; do
      case "$package" in
        dep-*)
          root=${package#dep-}
          grep -Fxq -- "$root" "$db" || printf '%s\n' "$package"
          ;;
      esac
    done < "$db"
    ;;
  -S)
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
    [ "$#" -gt 0 ] && shift
    for package in "$@"; do
      grep -Fxq -- "dep-$package" "$db" || printf '%s\n' "dep-$package" >> "$db"
      grep -Fxq -- "$package" "$db" || printf '%s\n' "$package" >> "$db"
    done
    ;;
  -Rns)
    while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done
    [ "$#" -gt 0 ] && shift
    for package in "$@"; do
      grep -Fvx -- "$package" "$db" > "$db.next" || true
      mv "$db.next" "$db"
      grep -Fvx -- "dep-$package" "$db" > "$db.next" || true
      mv "$db.next" "$db"
    done
    ;;
  -D) exit 0 ;;
  *)
    printf 'unsupported fake pacman call: %s\n' "$command" >&2
    exit 70
    ;;
esac
"#,
        )?;
        Ok(Self {
            root,
            home,
            data,
            state,
            fakebin,
            package_db,
        })
    }

    fn base_env(&self) -> BTreeMap<String, String> {
        let path = format!(
            "{}:{}",
            self.fakebin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        BTreeMap::from([
            ("DOTLAB_TEST_MODE".to_owned(), "1".to_owned()),
            (
                "DOTLAB_FAKE_PACKAGE_DB".to_owned(),
                self.package_db.display().to_string(),
            ),
            ("HOME".to_owned(), self.home.display().to_string()),
            ("DOTLAB_HOME".to_owned(), self.data.display().to_string()),
            ("DOTLAB_STATE".to_owned(), self.state.display().to_string()),
            ("PATH".to_owned(), path),
        ])
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.run_with(arguments, &[])
    }

    fn run_with(&self, arguments: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotlab"));
        command.args(arguments);
        command.env_clear();
        for (key, value) in self.base_env() {
            command.env(key, value);
        }
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("running dotlab")
    }
}

#[test]
fn profiles_switch_rollback_recover_and_gc() -> Result<()> {
    let sandbox = Sandbox::new()?;
    fs::write(&sandbox.package_db, "manual-package\n")?;
    let original_hypr = sandbox.home.join(".config/hypr");
    let original_waybar = sandbox.home.join(".config/waybar");
    fs::create_dir_all(&original_hypr)?;
    fs::create_dir_all(&original_waybar)?;
    fs::write(
        original_hypr.join("hyprland.lua"),
        "-- original before Dotlab\n",
    )?;
    fs::write(original_waybar.join("config.jsonc"), "original bar\n")?;
    set_xattr(
        &original_waybar.join("config.jsonc"),
        "user.dotlab-fixture",
        b"preserve-me",
    )?;

    succeeds(sandbox.run(&["init", "--yes"]))?;
    assert!(
        fs::symlink_metadata(&original_hypr)?
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(original_hypr.join("hyprland.lua"))?.contains("Dotlab protected"));
    assert_packages(
        &sandbox,
        &[
            "dep-fuzzel",
            "dep-kitty",
            "fuzzel",
            "kitty",
            "manual-package",
        ],
    )?;

    let repository = sandbox.root.path().join("repo");
    fs::create_dir_all(repository.join(".config/hypr"))?;
    fs::create_dir_all(repository.join(".config/waybar"))?;
    fs::write(
        repository.join(".config/hypr/hyprland.lua"),
        "-- experiment\n",
    )?;
    fs::write(
        repository.join(".config/waybar/config.jsonc"),
        "experiment bar\n",
    )?;
    fs::write(repository.join("nested"), "nested fixture\n")?;
    git(&repository, &["init", "-q"])?;
    git(&repository, &["add", "."])?;
    git(
        &repository,
        &[
            "-c",
            "user.name=Dotlab Test",
            "-c",
            "user.email=dotlab@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    )?;
    succeeds(sandbox.run(&[
        "profile",
        "add",
        "experiment",
        repository.to_str().context("UTF-8 repository path")?,
        "--package",
        "waybar",
        "--package",
        "manual-package",
        "--map",
        ".config/hypr=.config/hypr",
        "--map",
        ".config/waybar=.config/waybar",
        "--map",
        "nested=.deep/config/app",
    ]))?;

    let before = fs::read_link(&original_hypr)?;
    succeeds(sandbox.run(&["switch", "experiment", "--dry-run"]))?;
    assert_eq!(fs::read_link(&original_hypr)?, before);

    let package_crash = sandbox.run_with(
        &["switch", "experiment", "--yes"],
        &[("DOTLAB_CRASH_AFTER_PACMAN", "1")],
    );
    assert_eq!(package_crash.status.code(), Some(98));
    assert!(sandbox.state.join("package-pending.json").is_file());
    assert_eq!(fs::read_link(&original_hypr)?, before);

    // An ordinary mid-transaction error is unwound before the command returns.
    fails(sandbox.run_with(
        &["switch", "experiment", "--yes"],
        &[("DOTLAB_FAIL_AFTER_OP", "1")],
    ))?;
    assert!(!sandbox.state.join("package-pending.json").exists());
    assert_eq!(fs::read_link(&original_hypr)?, before);
    assert_eq!(
        fs::read_to_string(original_waybar.join("config.jsonc"))?,
        "original bar\n"
    );
    assert_eq!(
        get_xattr(&original_waybar.join("config.jsonc"), "user.dotlab-fixture")?,
        b"preserve-me"
    );

    // A hard process exit leaves a journal. The next invocation recovers it.
    let crashed = sandbox.run_with(
        &["switch", "experiment", "--yes"],
        &[("DOTLAB_CRASH_AFTER_OP", "3")],
    );
    assert_eq!(crashed.status.code(), Some(99));
    succeeds(sandbox.run(&["profile", "list"]))?;
    assert_eq!(fs::read_link(&original_hypr)?, before);
    assert_eq!(
        fs::read_to_string(original_waybar.join("config.jsonc"))?,
        "original bar\n"
    );
    assert_eq!(
        get_xattr(&original_waybar.join("config.jsonc"), "user.dotlab-fixture")?,
        b"preserve-me"
    );
    assert!(!sandbox.home.join(".deep").exists());

    succeeds(sandbox.run(&["switch", "experiment", "--yes", "--next-login"]))?;
    assert_eq!(
        fs::read_to_string(original_hypr.join("hyprland.lua"))?,
        "-- experiment\n"
    );
    assert_eq!(
        fs::read_to_string(original_waybar.join("config.jsonc"))?,
        "experiment bar\n"
    );
    assert_eq!(
        fs::read_to_string(sandbox.home.join(".deep/config/app"))?,
        "nested fixture\n"
    );
    assert_packages(
        &sandbox,
        &[
            "dep-fuzzel",
            "dep-kitty",
            "dep-waybar",
            "fuzzel",
            "kitty",
            "manual-package",
            "waybar",
        ],
    )?;

    succeeds(sandbox.run(&["switch", "base", "--yes", "--next-login"]))?;
    assert!(
        !fs::symlink_metadata(&original_waybar)?
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(original_waybar.join("config.jsonc"))?,
        "original bar\n"
    );
    assert_eq!(
        get_xattr(&original_waybar.join("config.jsonc"), "user.dotlab-fixture")?,
        b"preserve-me"
    );
    assert!(!sandbox.home.join(".deep").exists());

    succeeds(sandbox.run(&["rollback", "--yes"]))?;
    assert_eq!(
        fs::read_to_string(original_hypr.join("hyprland.lua"))?,
        "-- experiment\n"
    );
    assert!(sandbox.home.join(".deep/config/app").exists());
    succeeds(sandbox.run(&["switch", "base", "--yes", "--next-login"]))?;
    assert!(!sandbox.home.join(".deep").exists());

    fs::write(
        original_waybar.join("config.jsonc"),
        "manual change while unmanaged\n",
    )?;
    fails(sandbox.run(&["switch", "experiment", "--yes", "--next-login"]))?;
    assert_eq!(
        fs::read_to_string(original_waybar.join("config.jsonc"))?,
        "manual change while unmanaged\n"
    );
    fs::write(original_waybar.join("config.jsonc"), "original bar\n")?;

    fails(sandbox.run(&["profile", "remove", "base", "--yes"]))?;
    succeeds(sandbox.run(&["profile", "remove", "experiment", "--yes"]))?;
    succeeds(sandbox.run(&["packages", "gc", "--yes"]))?;
    assert_packages(
        &sandbox,
        &[
            "dep-fuzzel",
            "dep-kitty",
            "fuzzel",
            "kitty",
            "manual-package",
        ],
    )?;

    // Captured profiles are self-contained and restore an absent baseline.
    let foot = sandbox.home.join(".config/foot");
    fs::create_dir_all(&foot)?;
    fs::write(foot.join("foot.ini"), "font=mono\n")?;
    succeeds(sandbox.run(&["profile", "capture", "local", "--path", ".config/foot"]))?;
    succeeds(sandbox.run(&["switch", "local", "--yes", "--next-login"]))?;
    assert!(fs::symlink_metadata(&foot)?.file_type().is_symlink());
    succeeds(sandbox.run(&["switch", "base", "--yes", "--next-login"]))?;
    assert_eq!(fs::read_to_string(foot.join("foot.ini"))?, "font=mono\n");

    let outside = sandbox.root.path().join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("app"), "must stay untouched\n")?;
    std::os::unix::fs::symlink(&outside, sandbox.home.join(".escape"))?;
    fails(sandbox.run(&[
        "profile",
        "capture",
        "escape-attempt",
        "--path",
        ".escape/app",
    ]))?;
    assert_eq!(
        fs::read_to_string(outside.join("app"))?,
        "must stay untouched\n"
    );
    Ok(())
}

#[test]
fn metal_slots_are_journaled_bootable_and_discardable() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let metal = MetalFixture::new(&sandbox)?;

    succeeds(metal.run(&["metal", "preflight"]))?;

    // A partial second snapshot failure is completely unwound.
    fails(metal.run_with(
        &["metal", "create", "partial", "--yes"],
        &[("DOTLAB_FAKE_BTRFS_FAIL_HOME", "1")],
    ))?;
    let partial_state = metal.state()?;
    assert!(
        partial_state["slots"]
            .as_object()
            .is_some_and(|map| map.is_empty())
    );
    assert!(!metal.pending.exists());

    succeeds(metal.run(&["metal", "create", "trial", "--yes"]))?;
    let state = metal.state()?;
    let slot = &state["slots"]["trial"];
    let id = slot["id"].as_str().context("slot id")?;
    let root_snapshot = PathBuf::from(slot["root_snapshot"].as_str().context("root snapshot")?);
    let home_snapshot = PathBuf::from(slot["home_snapshot"].as_str().context("home snapshot")?);
    let slot_uki = PathBuf::from(slot["slot_uki"].as_str().context("slot UKI")?);
    let original_uki = PathBuf::from(
        slot["original_uki"]
            .as_str()
            .context("preserved original UKI")?,
    );
    assert!(root_snapshot.is_dir());
    assert!(home_snapshot.is_dir());
    assert_eq!(fs::read(&slot_uki)?, fs::read(&metal.source_uki)?);
    assert_eq!(fs::read(&original_uki)?, fs::read(&metal.source_uki)?);
    let fstab = fs::read_to_string(root_snapshot.join("etc/fstab"))?;
    assert!(fstab.contains(&format!("subvol=/@snapshots/dotlab/{id}/root")));
    assert!(fstab.contains(&format!("subvol=/@snapshots/dotlab/{id}/home")));
    assert!(fstab.lines().any(|line| {
        line.split_whitespace().nth(1) == Some("/boot")
            && line
                .split_whitespace()
                .nth(3)
                .is_some_and(|options| options.split(',').any(|value| value == "ro"))
    }));
    let grub = fs::read_to_string(&metal.grub_config)?;
    assert!(grub.contains(&format!("dotlab-slot-{id}")));
    assert!(grub.contains(&format!("dotlab-original-{id}")));
    assert!(grub.contains(&format!("rootflags=subvol=/@snapshots/dotlab/{id}/root")));

    succeeds(metal.run(&["metal", "activate", "trial"]))?;
    assert_eq!(
        fs::read_to_string(&metal.selected)?.trim(),
        format!("dotlab-slot-{id}")
    );

    fs::write(
        &metal.proc_cmdline,
        format!(
            "cryptdevice=PARTUUID=fixture:root root=/dev/mapper/root zswap.enabled=0 \
             rootflags=subvol=/@snapshots/dotlab/{id}/root rw dotlab.slot={id}\n"
        ),
    )?;
    fails(metal.run(&["metal", "discard", "trial", "--yes"]))?;
    succeeds(metal.run_with(
        &["metal", "leave", "trial"],
        &[("DOTLAB_FAKE_BOOT_RO", "1")],
    ))?;
    assert_eq!(
        fs::read_to_string(&metal.selected)?.trim(),
        format!("dotlab-original-{id}")
    );
    let mounts = fs::read_to_string(&metal.mount_log)?;
    assert!(mounts.contains("remount,rw"));
    assert!(mounts.contains("remount,ro"));

    fs::write(
        &metal.proc_cmdline,
        "cryptdevice=PARTUUID=fixture:root root=/dev/mapper/root zswap.enabled=0 \
         rootflags=subvol=@ rw\n",
    )?;
    succeeds(metal.run(&["metal", "discard", "trial", "--yes"]))?;
    assert!(!root_snapshot.exists());
    assert!(!home_snapshot.exists());
    assert!(!slot_uki.exists());
    assert!(!original_uki.exists());
    assert!(!fs::read_to_string(&metal.grub_config)?.contains(id));

    // Once all snapshots and UKIs are verified, a GRUB failure is completed
    // from the journal on the next invocation instead of deleting boot assets.
    fails(metal.run_with(
        &["metal", "create", "grub-recovery", "--yes"],
        &[("DOTLAB_FAKE_GRUB_FAIL", "1")],
    ))?;
    assert!(metal.pending.exists());
    succeeds(metal.run(&["metal", "status"]))?;
    assert!(!metal.pending.exists());
    assert!(
        metal.state()?["slots"]
            .as_object()
            .is_some_and(|slots| slots.contains_key("grub-recovery"))
    );
    Ok(())
}

#[test]
fn metal_slot_promotion_is_persistent_and_fail_closed() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let metal = MetalFixture::new(&sandbox)?;
    succeeds(metal.run(&["metal", "create", "primary", "--yes"]))?;
    let created = metal.state()?;
    let slot = &created["slots"]["primary"];
    let id = slot["id"].as_str().context("slot id")?.to_owned();
    let root_subvol = slot["root_subvol"]
        .as_str()
        .context("root subvolume")?
        .to_owned();
    let home_subvol = slot["home_subvol"]
        .as_str()
        .context("home subvolume")?
        .to_owned();
    let root_snapshot = PathBuf::from(slot["root_snapshot"].as_str().context("root snapshot")?);
    fs::write(
        &metal.proc_cmdline,
        format!(
            "cryptdevice=PARTUUID=fixture:root root=/dev/mapper/root \
             rootflags=subvol=/{root_subvol} rw dotlab.slot={id}\n"
        ),
    )?;

    succeeds(metal.run_with(
        &["metal", "promote", "primary", "--yes"],
        &[
            ("DOTLAB_FAKE_BOOT_RO", "1"),
            ("DOTLAB_FAKE_ROOT_SUBVOL", root_subvol.as_str()),
            ("DOTLAB_FAKE_HOME_SUBVOL", home_subvol.as_str()),
        ],
    ))?;
    let promoted = metal.state()?;
    assert_eq!(promoted["schema"], 2);
    assert_eq!(promoted["slots"]["primary"]["schema"], 2);
    assert_eq!(promoted["slots"]["primary"]["promoted"], true);
    assert!(
        promoted["slots"]["primary"]["promoted_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(!metal.pending.exists());
    assert_eq!(
        fs::read_to_string(&metal.persistent)?.trim(),
        format!("dotlab-slot-{id}")
    );
    assert_eq!(fs::read_to_string(&metal.boot_mode)?.trim(), "rw");
    let fstab = fs::read_to_string(root_snapshot.join("etc/fstab"))?;
    assert!(fstab.lines().any(|line| {
        line.split_whitespace().nth(1) == Some("/boot")
            && line
                .split_whitespace()
                .nth(3)
                .is_some_and(|options| options.split(',').any(|value| value == "rw"))
    }));
    let grub = fs::read_to_string(&metal.grub_config)?;
    assert!(grub.contains(&format!("dotlab-slot-{id}")));
    assert!(grub.contains(&format!("dotlab-last-good-{id}")));
    assert!(grub.contains(&format!("dotlab-original-{id}")));
    assert!(grub.contains("Dotlab: primary (last-known-good)"));
    assert!(grub.contains("EFI/Linux/arch-linux-zen.efi"));
    let mounts = fs::read_to_string(&metal.mount_log)?;
    assert!(mounts.contains("remount,rw"));
    assert!(!mounts.contains("remount,ro"));

    succeeds(metal.run(&["metal", "leave", "primary"]))?;
    assert_eq!(
        fs::read_to_string(&metal.selected)?.trim(),
        format!("dotlab-original-{id}")
    );
    fs::write(
        &metal.proc_cmdline,
        "cryptdevice=PARTUUID=fixture:root root=/dev/mapper/root \
         rootflags=subvol=/@ rw\n",
    )?;
    succeeds(metal.run(&["metal", "activate", "primary"]))?;
    assert_eq!(
        fs::read_to_string(&metal.selected)?.trim(),
        format!("dotlab-slot-{id}")
    );
    fails(metal.run(&["metal", "discard", "primary", "--yes"]))?;

    // A failure before the ready marker rolls back state, fstab, and mount mode.
    let rollback_sandbox = Sandbox::new()?;
    let rollback = MetalFixture::new(&rollback_sandbox)?;
    succeeds(rollback.run(&["metal", "create", "rollback", "--yes"]))?;
    let rollback_state = rollback.state()?;
    let rollback_slot = &rollback_state["slots"]["rollback"];
    let rollback_id = rollback_slot["id"].as_str().context("rollback id")?;
    let rollback_root = rollback_slot["root_subvol"]
        .as_str()
        .context("rollback root")?;
    let rollback_home = rollback_slot["home_subvol"]
        .as_str()
        .context("rollback home")?;
    let rollback_snapshot = PathBuf::from(
        rollback_slot["root_snapshot"]
            .as_str()
            .context("rollback snapshot")?,
    );
    fs::write(
        &rollback.proc_cmdline,
        format!(
            "root=/dev/mapper/root rootflags=subvol=/{rollback_root} rw \
             dotlab.slot={rollback_id}\n"
        ),
    )?;
    fails(rollback.run_with(
        &["metal", "promote", "rollback", "--yes"],
        &[
            ("DOTLAB_FAKE_BOOT_RO", "1"),
            ("DOTLAB_FAKE_ROOT_SUBVOL", rollback_root),
            ("DOTLAB_FAKE_HOME_SUBVOL", rollback_home),
            ("DOTLAB_FAKE_GRUB_FAIL", "1"),
        ],
    ))?;
    assert!(rollback.pending.exists());
    succeeds(rollback.run(&["metal", "status"]))?;
    assert!(!rollback.pending.exists());
    assert_eq!(rollback.state()?["slots"]["rollback"]["promoted"], false);
    assert_eq!(fs::read_to_string(&rollback.boot_mode)?.trim(), "ro");
    let rollback_fstab = fs::read_to_string(rollback_snapshot.join("etc/fstab"))?;
    assert!(rollback_fstab.lines().any(|line| {
        line.split_whitespace().nth(1) == Some("/boot")
            && line
                .split_whitespace()
                .nth(3)
                .is_some_and(|options| options.split(',').any(|value| value == "ro"))
    }));
    assert!(!rollback.persistent.exists());

    // Once the ready marker exists, recovery completes promotion instead of
    // reverting a potentially selected persistent default.
    let finalize_sandbox = Sandbox::new()?;
    let finalize = MetalFixture::new(&finalize_sandbox)?;
    succeeds(finalize.run(&["metal", "create", "finalize", "--yes"]))?;
    let finalize_state = finalize.state()?;
    let finalize_slot = &finalize_state["slots"]["finalize"];
    let finalize_id = finalize_slot["id"]
        .as_str()
        .context("finalize id")?
        .to_owned();
    let finalize_root = finalize_slot["root_subvol"]
        .as_str()
        .context("finalize root")?
        .to_owned();
    let finalize_home = finalize_slot["home_subvol"]
        .as_str()
        .context("finalize home")?
        .to_owned();
    fs::write(
        &finalize.proc_cmdline,
        format!(
            "root=/dev/mapper/root rootflags=subvol=/{finalize_root} rw \
             dotlab.slot={finalize_id}\n"
        ),
    )?;
    fails(finalize.run_with(
        &["metal", "promote", "finalize", "--yes"],
        &[
            ("DOTLAB_FAKE_BOOT_RO", "1"),
            ("DOTLAB_FAKE_ROOT_SUBVOL", finalize_root.as_str()),
            ("DOTLAB_FAKE_HOME_SUBVOL", finalize_home.as_str()),
            ("DOTLAB_FAKE_GRUB_SET_DEFAULT_FAIL", "1"),
        ],
    ))?;
    assert!(finalize.pending.exists());
    succeeds(finalize.run(&["metal", "status"]))?;
    assert!(!finalize.pending.exists());
    assert_eq!(finalize.state()?["slots"]["finalize"]["promoted"], true);
    assert_eq!(
        fs::read_to_string(&finalize.persistent)?.trim(),
        format!("dotlab-slot-{finalize_id}")
    );
    assert_eq!(fs::read_to_string(&finalize.boot_mode)?.trim(), "rw");

    // Promotion refuses a forged slot marker, mismatched mounts, and a normal
    // UKI that changed since the known-good copies were made.
    let guarded_sandbox = Sandbox::new()?;
    let guarded = MetalFixture::new(&guarded_sandbox)?;
    succeeds(guarded.run(&["metal", "create", "guarded", "--yes"]))?;
    let guarded_state = guarded.state()?;
    let guarded_slot = &guarded_state["slots"]["guarded"];
    let guarded_id = guarded_slot["id"].as_str().context("guarded id")?;
    let guarded_root = guarded_slot["root_subvol"]
        .as_str()
        .context("guarded root")?;
    let guarded_home = guarded_slot["home_subvol"]
        .as_str()
        .context("guarded home")?;
    fs::write(
        &guarded.proc_cmdline,
        format!(
            "root=/dev/mapper/root rootflags=subvol=/{guarded_root} rw \
             dotlab.slot={guarded_id}\n"
        ),
    )?;
    fails(guarded.run_with(
        &["metal", "promote", "guarded", "--yes"],
        &[("DOTLAB_FAKE_BOOT_RO", "1")],
    ))?;
    assert!(!guarded.pending.exists());
    fs::write(&guarded.source_uki, b"MZ changed but untested UKI\n")?;
    fails(guarded.run_with(
        &["metal", "promote", "guarded", "--yes"],
        &[
            ("DOTLAB_FAKE_BOOT_RO", "1"),
            ("DOTLAB_FAKE_ROOT_SUBVOL", guarded_root),
            ("DOTLAB_FAKE_HOME_SUBVOL", guarded_home),
        ],
    ))?;
    assert!(!guarded.pending.exists());
    assert_eq!(guarded.state()?["slots"]["guarded"]["promoted"], false);
    Ok(())
}

#[test]
fn metal_schema_one_state_migrates_on_the_next_mutation() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let metal = MetalFixture::new(&sandbox)?;
    succeeds(metal.run(&["metal", "create", "legacy", "--yes"]))?;
    let mut legacy = metal.state()?;
    legacy["schema"] = Value::from(1);
    let legacy_slot = legacy["slots"]["legacy"]
        .as_object_mut()
        .context("legacy slot object")?;
    legacy_slot.insert("schema".to_owned(), Value::from(1));
    legacy_slot.remove("promoted");
    legacy_slot.remove("promoted_at");
    fs::write(
        metal.metal_state.join("slots.json"),
        serde_json::to_vec_pretty(&legacy)?,
    )?;

    succeeds(metal.run(&["metal", "status"]))?;
    assert_eq!(metal.state()?["schema"], 1);
    succeeds(metal.run(&["metal", "create", "second", "--yes"]))?;
    let migrated = metal.state()?;
    assert_eq!(migrated["schema"], 2);
    for slot in migrated["slots"]
        .as_object()
        .context("migrated slots")?
        .values()
    {
        assert_eq!(slot["schema"], 2);
        assert_eq!(slot["promoted"], false);
    }
    Ok(())
}

struct MetalFixture<'a> {
    sandbox: &'a Sandbox,
    boot: PathBuf,
    source_uki: PathBuf,
    snapshots: PathBuf,
    metal_state: PathBuf,
    pending: PathBuf,
    grub_script: PathBuf,
    grub_config: PathBuf,
    grub_defaults: PathBuf,
    proc_cmdline: PathBuf,
    fake_fstab: PathBuf,
    selected: PathBuf,
    persistent: PathBuf,
    boot_mode: PathBuf,
    mount_log: PathBuf,
}

impl<'a> MetalFixture<'a> {
    fn new(sandbox: &'a Sandbox) -> Result<Self> {
        let system = sandbox.root.path().join("system");
        let boot = system.join("boot");
        let source_uki = boot.join("EFI/Linux/arch-linux-zen.efi");
        let snapshots = system.join("snapshots/dotlab");
        let metal_state = system.join("var/lib/dotlab/metal");
        let pending = metal_state.join("pending.json");
        let grub_script = system.join("etc/grub.d/41_dotlab");
        let grub_config = boot.join("grub/grub.cfg");
        let grub_defaults = system.join("etc/default/grub");
        let proc_cmdline = system.join("proc-cmdline");
        let fake_fstab = system.join("fstab");
        let selected = system.join("selected-entry");
        let persistent = system.join("persistent-entry");
        let boot_mode = system.join("boot-mode");
        let mount_log = system.join("mounts");
        fs::create_dir_all(source_uki.parent().context("UKI parent")?)?;
        fs::create_dir_all(grub_config.parent().context("GRUB config parent")?)?;
        fs::create_dir_all(grub_script.parent().context("GRUB script parent")?)?;
        fs::create_dir_all(grub_defaults.parent().context("GRUB defaults parent")?)?;
        fs::write(&source_uki, b"MZ fake UKI fixture\n")?;
        fs::write(&grub_config, "# original grub config\n")?;
        fs::write(&grub_defaults, "GRUB_DEFAULT=saved\n")?;
        fs::write(
            &proc_cmdline,
            "cryptdevice=PARTUUID=fixture:root root=/dev/mapper/root zswap.enabled=0 \
             rootflags=subvol=@ rw\n",
        )?;
        fs::write(
            &fake_fstab,
            "UUID=root / btrfs rw,compress=zstd:3,subvol=/@ 0 0\n\
             UUID=root /home btrfs rw,compress=zstd:3,subvol=/@home 0 0\n\
             UUID=root /.snapshots btrfs rw,compress=zstd:3,subvol=/@snapshots 0 0\n\
             UUID=root /var/log btrfs rw,compress=zstd:3,subvol=/@log 0 0\n\
             UUID=root /var/cache/pacman/pkg btrfs rw,compress=zstd:3,subvol=/@pkg 0 0\n\
             UUID=21B1-F485 /boot vfat rw,fmask=0077,dmask=0077 0 2\n",
        )?;
        fs::write(&boot_mode, "rw\n")?;
        fs::write(&mount_log, "")?;
        install_metal_fakes(&sandbox.fakebin)?;
        Ok(Self {
            sandbox,
            boot,
            source_uki,
            snapshots,
            metal_state,
            pending,
            grub_script,
            grub_config,
            grub_defaults,
            proc_cmdline,
            fake_fstab,
            selected,
            persistent,
            boot_mode,
            mount_log,
        })
    }

    fn env(&self) -> BTreeMap<String, String> {
        let mut env = self.sandbox.base_env();
        env.extend([
            ("DOTLAB_BOOT".to_owned(), self.boot.display().to_string()),
            (
                "DOTLAB_SNAPSHOT_ROOT".to_owned(),
                self.snapshots.display().to_string(),
            ),
            (
                "DOTLAB_METAL_STATE".to_owned(),
                self.metal_state.display().to_string(),
            ),
            (
                "DOTLAB_GRUB_SCRIPT".to_owned(),
                self.grub_script.display().to_string(),
            ),
            (
                "DOTLAB_GRUB_CONFIG".to_owned(),
                self.grub_config.display().to_string(),
            ),
            (
                "DOTLAB_GRUB_DEFAULTS".to_owned(),
                self.grub_defaults.display().to_string(),
            ),
            (
                "DOTLAB_PROC_CMDLINE".to_owned(),
                self.proc_cmdline.display().to_string(),
            ),
            (
                "DOTLAB_PACMAN_LOCK".to_owned(),
                self.sandbox
                    .root
                    .path()
                    .join("pacman.lock")
                    .display()
                    .to_string(),
            ),
            (
                "DOTLAB_UKI_SOURCE".to_owned(),
                self.source_uki.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_FSTAB".to_owned(),
                self.fake_fstab.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_BOOT".to_owned(),
                self.boot.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_SNAPSHOTS_MOUNT".to_owned(),
                self.snapshots
                    .parent()
                    .expect("snapshot mount")
                    .display()
                    .to_string(),
            ),
            (
                "DOTLAB_FAKE_GRUB_SCRIPT".to_owned(),
                self.grub_script.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_SELECTED".to_owned(),
                self.selected.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_PERSISTENT".to_owned(),
                self.persistent.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_BOOT_MODE".to_owned(),
                self.boot_mode.display().to_string(),
            ),
            (
                "DOTLAB_FAKE_MOUNT_LOG".to_owned(),
                self.mount_log.display().to_string(),
            ),
        ]);
        env
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.run_with(arguments, &[])
    }

    fn run_with(&self, arguments: &[&str], extra: &[(&str, &str)]) -> Output {
        if let Some((_, value)) = extra.iter().find(|(key, _)| *key == "DOTLAB_FAKE_BOOT_RO") {
            fs::write(&self.boot_mode, if *value == "1" { "ro\n" } else { "rw\n" })
                .expect("setting fake boot mode");
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_dotlab"));
        command.args(arguments).env_clear();
        for (key, value) in self.env() {
            command.env(key, value);
        }
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("running metal command")
    }

    fn state(&self) -> Result<Value> {
        let bytes = fs::read(self.metal_state.join("slots.json"))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

fn install_metal_fakes(directory: &Path) -> Result<()> {
    executable(
        &directory.join("findmnt"),
        r#"#!/bin/sh
set -eu
field=
target=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) field=$2; shift 2 ;;
    --target) target=$2; shift 2 ;;
    *) shift ;;
  esac
done
case "$field:$target" in
  "TARGET:/var/lib/pacman") printf '/\n' ;;
  "SOURCE,OPTIONS:/")
    subvol=${DOTLAB_FAKE_ROOT_SUBVOL:-@}
    printf '/dev/mapper/root[/%s] rw,compress=zstd:3,subvol=/%s\n' "$subvol" "$subvol"
    ;;
  "SOURCE,OPTIONS:/home")
    subvol=${DOTLAB_FAKE_HOME_SUBVOL:-@home}
    printf '/dev/mapper/root[/%s] rw,compress=zstd:3,subvol=/%s\n' "$subvol" "$subvol"
    ;;
  UUID:*) printf '21B1-F485\n' ;;
  OPTIONS:*)
    if [ "$(cat "$DOTLAB_FAKE_BOOT_MODE")" = ro ]; then
      printf 'ro,relatime\n'
    else
      printf 'rw,relatime\n'
    fi
    ;;
  "FSTYPE,SOURCE,OPTIONS:/")
    printf 'btrfs /dev/mapper/root[/@] rw,compress=zstd:3,subvol=/@\n'
    ;;
  "FSTYPE,SOURCE,OPTIONS:/home")
    printf 'btrfs /dev/mapper/root[/@home] rw,compress=zstd:3,subvol=/@home\n'
    ;;
  "FSTYPE,SOURCE,OPTIONS:/var/log")
    printf 'btrfs /dev/mapper/root[/@log] rw,compress=zstd:3,subvol=/@log\n'
    ;;
  "FSTYPE,SOURCE,OPTIONS:/var/cache/pacman/pkg")
    printf 'btrfs /dev/mapper/root[/@pkg] rw,compress=zstd:3,subvol=/@pkg\n'
    ;;
  "FSTYPE,SOURCE,OPTIONS:${DOTLAB_FAKE_BOOT}")
    printf 'vfat /dev/nvme1n1p1 rw,fmask=0077,dmask=0077\n'
    ;;
  "FSTYPE,SOURCE,OPTIONS:${DOTLAB_FAKE_SNAPSHOTS_MOUNT}")
    printf 'btrfs /dev/mapper/root[/@snapshots] rw,compress=zstd:3,subvol=/@snapshots\n'
    ;;
  *)
    printf 'unsupported fake findmnt query: %s:%s\n' "$field" "$target" >&2
    exit 64
    ;;
esac
"#,
    )?;
    executable(
        &directory.join("bootctl"),
        "#!/bin/sh\nprintf 'System:\\n   Secure Boot: disabled\\nCurrent Boot Loader:\\n       Product: GRUB 2.14\\nCurrent Stub:\\n          Stub: /EFI/Linux/arch-linux-zen.efi\\n'\n",
    )?;
    executable(
        &directory.join("lsblk"),
        r#"#!/bin/sh
set -eu
field=
last=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|--output) field=$2; shift 2 ;;
    -n|-p|-r|-s|--inverse|--raw|--noheadings|--paths) shift ;;
    *) last=$1; shift ;;
  esac
done
case "$field:$last" in
  "NAME,FSTYPE:/dev/mapper/root")
    printf '/dev/mapper/root btrfs\n'
    printf '/dev/nvme1n1p2 crypto_LUKS\n'
    printf '/dev/nvme1n1\n'
    ;;
  *) exit 65 ;;
esac
"#,
    )?;
    executable(
        &directory.join("btrfs"),
        r#"#!/bin/sh
set -eu
case "${1:-} ${2:-}" in
  "subvolume snapshot")
    source=$4
    destination=$5
    if [ "$source" = /home ] && [ "${DOTLAB_FAKE_BTRFS_FAIL_HOME:-0}" = 1 ]; then
      printf 'injected home snapshot failure\n' >&2
      exit 72
    fi
    mkdir -p "$destination"
    if [ "$source" = / ]; then
      mkdir -p "$destination/etc"
      cp -- "$DOTLAB_FAKE_FSTAB" "$destination/etc/fstab"
    else
      printf 'isolated home\n' > "$destination/home-fixture"
    fi
    ;;
  "subvolume delete")
    destination=$4
    find "$destination" -depth -delete
    ;;
  *) exit 66 ;;
esac
"#,
    )?;
    executable(
        &directory.join("grub-mkconfig"),
        r#"#!/bin/sh
set -eu
[ "${DOTLAB_FAKE_GRUB_FAIL:-0}" = 1 ] && exit 73
[ "${1:-}" = -o ]
output=$2
{
  printf '# fake generated grub config\n'
  "$DOTLAB_FAKE_GRUB_SCRIPT"
} > "$output"
"#,
    )?;
    executable(
        &directory.join("grub-script-check"),
        "#!/bin/sh\nset -eu\n[ -s \"${1:?}\" ]\n",
    )?;
    executable(
        &directory.join("grub-reboot"),
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"${1:?}\" > \"$DOTLAB_FAKE_SELECTED\"\n",
    )?;
    executable(
        &directory.join("grub-set-default"),
        r#"#!/bin/sh
set -eu
[ "${DOTLAB_FAKE_GRUB_SET_DEFAULT_FAIL:-0}" = 1 ] && exit 74
entry=
for argument in "$@"; do
  case "$argument" in
    --boot-directory=*) ;;
    *) entry=$argument ;;
  esac
done
[ -n "$entry" ]
printf '%s\n' "$entry" > "$DOTLAB_FAKE_PERSISTENT"
"#,
    )?;
    executable(
        &directory.join("mount"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DOTLAB_FAKE_MOUNT_LOG"
case "$*" in
  *remount,rw*) printf 'rw\n' > "$DOTLAB_FAKE_BOOT_MODE" ;;
  *remount,ro*) printf 'ro\n' > "$DOTLAB_FAKE_BOOT_MODE" ;;
esac
"#,
    )?;
    executable(&directory.join("systemctl"), "#!/bin/sh\nexit 0\n")?;
    Ok(())
}

fn executable(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn git(repository: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn succeeds(output: Output) -> Result<()> {
    if !output.status.success() {
        bail!(
            "expected success, got {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn fails(output: Output) -> Result<()> {
    if output.status.success() {
        bail!(
            "expected failure\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn assert_packages(sandbox: &Sandbox, expected: &[&str]) -> Result<()> {
    let mut actual = fs::read_to_string(&sandbox.package_db)?
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = expected
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
    Ok(())
}

fn set_xattr(path: &Path, name: &str, value: &[u8]) -> Result<()> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    let name = CString::new(name)?;
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast::<libc::c_void>(),
            value.len(),
            0,
        )
    };
    if result != 0 {
        bail!("setxattr failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn get_xattr(path: &Path, name: &str) -> Result<Vec<u8>> {
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    let name = CString::new(name)?;
    let size = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        bail!("getxattr failed: {}", std::io::Error::last_os_error());
    }
    let mut value = vec![0_u8; size as usize];
    let actual = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast::<libc::c_void>(),
            value.len(),
        )
    };
    if actual < 0 {
        bail!("getxattr failed: {}", std::io::Error::last_os_error());
    }
    value.truncate(actual as usize);
    Ok(value)
}
