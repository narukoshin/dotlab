use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::paths::AppPaths;
use crate::util::{command_exists, node_exists, run_stdout};

#[derive(Clone, Debug)]
pub struct MetalFacts {
    pub root_device: String,
    pub esp_uuid: String,
    pub source_uki: PathBuf,
    pub kernel_arguments: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
}

pub fn run(paths: &AppPaths, args: DoctorArgs) -> Result<()> {
    let mut checks = basic_checks(paths);
    let mut metal_facts = None;
    if args.metal {
        let (metal_checks, facts) = probe_metal(paths);
        checks.extend(metal_checks);
        metal_facts = facts;
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for check in &checks {
            println!(
                "{:>6}  {:<28} {}",
                if check.ok { "[ok]" } else { "[FAIL]" },
                check.name,
                check.detail
            );
        }
        if let Some(facts) = metal_facts {
            println!("       root device: {}", facts.root_device);
            println!("       source UKI:  {}", facts.source_uki.display());
            println!("       ESP UUID:    {}", facts.esp_uuid);
        }
    }
    let failed = checks.iter().filter(|check| !check.ok).count();
    if failed > 0 {
        bail!("{failed} doctor check(s) failed");
    }
    Ok(())
}

pub fn require_metal(paths: &AppPaths) -> Result<MetalFacts> {
    let (checks, facts) = probe_metal(paths);
    for check in &checks {
        println!(
            "{:>6}  {:<28} {}",
            if check.ok { "[ok]" } else { "[FAIL]" },
            check.name,
            check.detail
        );
    }
    let failed: Vec<&Check> = checks.iter().filter(|check| !check.ok).collect();
    if !failed.is_empty() {
        bail!(
            "metal preflight failed: {}",
            failed
                .iter()
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    facts.context("metal facts unavailable despite successful checks")
}

fn basic_checks(paths: &AppPaths) -> Vec<Check> {
    let mut checks = Vec::new();
    for command in ["cp", "git", "pacman", "sudo"] {
        checks.push(Check {
            name: format!("command:{command}"),
            ok: command_exists(command),
            detail: if command_exists(command) {
                "available".to_owned()
            } else {
                "not found in PATH".to_owned()
            },
        });
    }
    checks.push(Check {
        name: "HOME".to_owned(),
        ok: paths.home.is_absolute() && paths.home.is_dir(),
        detail: paths.home.display().to_string(),
    });
    checks.push(Check {
        name: "state path".to_owned(),
        ok: paths.state.is_absolute(),
        detail: paths.state.display().to_string(),
    });
    checks
}

fn probe_metal(paths: &AppPaths) -> (Vec<Check>, Option<MetalFacts>) {
    let mut checks = Vec::new();

    for command in [
        "btrfs",
        "bootctl",
        "findmnt",
        "grub-mkconfig",
        "grub-reboot",
        "grub-set-default",
        "grub-script-check",
        "lsblk",
        "mount",
    ] {
        let ok = command_exists(command);
        checks.push(Check {
            name: format!("command:{command}"),
            ok,
            detail: if ok {
                "available".to_owned()
            } else {
                "not found in PATH".to_owned()
            },
        });
    }

    let root = mount_info(Path::new("/"));
    push_mount_check(&mut checks, "root subvolume", &root, "btrfs", "/@", true);
    let home = mount_info(Path::new("/home"));
    push_mount_check(
        &mut checks,
        "home subvolume",
        &home,
        "btrfs",
        "/@home",
        true,
    );
    let snapshots_mount = paths
        .snapshot_root
        .parent()
        .unwrap_or_else(|| Path::new("/.snapshots"));
    let snapshots = mount_info(snapshots_mount);
    push_mount_check(
        &mut checks,
        "snapshots subvolume",
        &snapshots,
        "btrfs",
        "/@snapshots",
        true,
    );
    let log = mount_info(Path::new("/var/log"));
    push_mount_check(&mut checks, "log subvolume", &log, "btrfs", "/@log", true);
    let package_cache = mount_info(Path::new("/var/cache/pacman/pkg"));
    push_mount_check(
        &mut checks,
        "package-cache subvolume",
        &package_cache,
        "btrfs",
        "/@pkg",
        true,
    );
    let boot = mount_info(&paths.boot);
    match &boot {
        Ok(info) => checks.push(Check {
            name: "ESP mount".to_owned(),
            ok: info.fstype == "vfat",
            detail: format!("{} {} ({})", info.fstype, info.source, paths.boot.display()),
        }),
        Err(error) => checks.push(failed("ESP mount", error)),
    }

    let compression_ok = [&root, &home, &snapshots, &log, &package_cache]
        .iter()
        .all(|result| {
            result.as_ref().is_ok_and(|info| {
                info.options
                    .split(',')
                    .any(|value| value == "compress=zstd:3")
            })
        });
    checks.push(Check {
        name: "Btrfs compression".to_owned(),
        ok: compression_ok,
        detail: if compression_ok {
            "compress=zstd:3 on managed subvolumes".to_owned()
        } else {
            "expected compress=zstd:3 on root, home, snapshots, log, and package cache".to_owned()
        },
    });

    let root_device = root
        .as_ref()
        .ok()
        .map(|info| strip_subvolume(&info.source).to_owned());
    let luks = root_device
        .as_deref()
        .map(check_luks_parent)
        .unwrap_or_else(|| Err(anyhow::anyhow!("root device unavailable")));
    match &luks {
        Ok(detail) => checks.push(Check {
            name: "LUKS parent".to_owned(),
            ok: true,
            detail: detail.clone(),
        }),
        Err(error) => checks.push(failed("LUKS parent", error)),
    }

    let pacman_mount = run_stdout(
        "findmnt",
        [
            OsString::from("-n"),
            OsString::from("-o"),
            OsString::from("TARGET"),
            OsString::from("--target"),
            OsString::from("/var/lib/pacman"),
        ],
    );
    match pacman_mount {
        Ok(target) => checks.push(Check {
            name: "pacman database in root".to_owned(),
            ok: target == "/",
            detail: format!("resolved mount: {target}"),
        }),
        Err(error) => checks.push(failed("pacman database in root", &error)),
    }

    let bootctl = run_stdout("bootctl", ["status"]);
    let secure_boot_disabled = bootctl.as_ref().is_ok_and(|output| {
        output
            .lines()
            .any(|line| line.to_ascii_lowercase().contains("secure boot: disabled"))
    });
    checks.push(Check {
        name: "Secure Boot".to_owned(),
        ok: secure_boot_disabled,
        detail: if secure_boot_disabled {
            "disabled; UKI command-line override is usable".to_owned()
        } else {
            "must be disabled for this slot implementation".to_owned()
        },
    });
    let grub_ok = bootctl
        .as_ref()
        .is_ok_and(|output| output.lines().any(|line| line.contains("Product: GRUB 2.")));
    checks.push(Check {
        name: "running boot loader".to_owned(),
        ok: grub_ok,
        detail: if grub_ok {
            "GRUB 2.x".to_owned()
        } else {
            "bootctl did not report GRUB 2.x".to_owned()
        },
    });

    let grub_saved = grub_default_is_saved(&paths.grub_defaults);
    checks.push(Check {
        name: "GRUB_DEFAULT".to_owned(),
        ok: grub_saved,
        detail: if grub_saved {
            "saved (grub-reboot one-shot entries enabled)".to_owned()
        } else {
            format!(
                "set GRUB_DEFAULT=saved in {} before creating slots",
                paths.grub_defaults.display()
            )
        },
    });

    let source_uki = find_source_uki(paths, bootctl.as_deref().unwrap_or(""));
    match &source_uki {
        Ok(path) => checks.push(Check {
            name: "running UKI".to_owned(),
            ok: path.is_file(),
            detail: path.display().to_string(),
        }),
        Err(error) => checks.push(failed("running UKI", error)),
    }

    let esp_uuid = run_stdout(
        "findmnt",
        [
            OsString::from("-n"),
            OsString::from("-o"),
            OsString::from("UUID"),
            OsString::from("--target"),
            paths.boot.as_os_str().to_owned(),
        ],
    );
    match &esp_uuid {
        Ok(uuid) => checks.push(Check {
            name: "ESP UUID".to_owned(),
            ok: !uuid.is_empty(),
            detail: uuid.clone(),
        }),
        Err(error) => checks.push(failed("ESP UUID", error)),
    }

    let kernel_arguments = fs::read_to_string(&paths.proc_cmdline)
        .with_context(|| format!("reading {}", paths.proc_cmdline.display()))
        .map(|value| {
            value
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        });
    match &kernel_arguments {
        Ok(arguments) => {
            let has_root = arguments.iter().any(|value| value.starts_with("root="));
            let has_subvol = has_base_root_subvolume(arguments);
            checks.push(Check {
                name: "kernel root arguments".to_owned(),
                ok: has_root && has_subvol,
                detail: if has_root && has_subvol {
                    "root= and rootflags subvol=@ found".to_owned()
                } else {
                    "expected root=... and rootflags containing subvol=@ or subvol=/@".to_owned()
                },
            });
        }
        Err(error) => checks.push(failed("kernel root arguments", error)),
    }

    checks.push(Check {
        name: "pacman lock".to_owned(),
        ok: !node_exists(&paths.pacman_lock),
        detail: if node_exists(&paths.pacman_lock) {
            format!(
                "{} exists; wait for pacman to finish",
                paths.pacman_lock.display()
            )
        } else {
            "no package transaction in progress".to_owned()
        },
    });

    let all_ok = checks.iter().all(|check| check.ok);
    let facts = if all_ok {
        Some(MetalFacts {
            root_device: root_device.expect("checked root device"),
            esp_uuid: esp_uuid.expect("checked ESP UUID"),
            source_uki: source_uki.expect("checked UKI"),
            kernel_arguments: kernel_arguments.expect("checked kernel arguments"),
        })
    } else {
        None
    };
    (checks, facts)
}

#[derive(Clone, Debug)]
struct MountInfo {
    fstype: String,
    source: String,
    options: String,
}

fn mount_info(target: &Path) -> Result<MountInfo> {
    let output = run_stdout(
        "findmnt",
        [
            OsString::from("-n"),
            OsString::from("-o"),
            OsString::from("FSTYPE,SOURCE,OPTIONS"),
            OsString::from("--target"),
            target.as_os_str().to_owned(),
        ],
    )?;
    let mut fields = output.split_whitespace();
    let fstype = fields.next().context("findmnt omitted FSTYPE")?.to_owned();
    let source = fields.next().context("findmnt omitted SOURCE")?.to_owned();
    let options = fields.collect::<Vec<_>>().join(" ");
    if options.is_empty() {
        bail!("findmnt omitted OPTIONS");
    }
    Ok(MountInfo {
        fstype,
        source,
        options,
    })
}

fn push_mount_check(
    checks: &mut Vec<Check>,
    name: &str,
    result: &Result<MountInfo>,
    fstype: &str,
    subvolume: &str,
    require_compression: bool,
) {
    match result {
        Ok(info) => {
            let correct_subvol = info.source.ends_with(&format!("[{subvolume}]"))
                || info
                    .options
                    .split(',')
                    .any(|value| value == format!("subvol={subvolume}"));
            let compression = !require_compression
                || info
                    .options
                    .split(',')
                    .any(|value| value == "compress=zstd:3");
            checks.push(Check {
                name: name.to_owned(),
                ok: info.fstype == fstype && correct_subvol && compression,
                detail: format!("{} {} {}", info.fstype, info.source, info.options),
            });
        }
        Err(error) => checks.push(failed(name, error)),
    }
}

fn strip_subvolume(source: &str) -> &str {
    source.split_once('[').map_or(source, |(device, _)| device)
}

fn check_luks_parent(root_device: &str) -> Result<String> {
    if !root_device.starts_with("/dev/") {
        bail!("unexpected root source {root_device:?}");
    }
    let ancestry = run_stdout(
        "lsblk",
        [
            OsString::from("--inverse"),
            OsString::from("--raw"),
            OsString::from("--noheadings"),
            OsString::from("--paths"),
            OsString::from("--output"),
            OsString::from("NAME,FSTYPE"),
            OsString::from(root_device),
        ],
    )?;
    find_luks_ancestor(root_device, &ancestry)
}

fn find_luks_ancestor(root_device: &str, ancestry: &str) -> Result<String> {
    for line in ancestry.lines() {
        let mut fields = line.split_whitespace();
        let Some(device) = fields.next() else {
            continue;
        };
        let fstype = fields.next().unwrap_or("");
        if device != root_device && fstype == "crypto_LUKS" {
            return Ok(format!("{root_device} inside {device} (crypto_LUKS)"));
        }
    }
    bail!("{root_device} has no crypto_LUKS ancestor")
}

fn has_base_root_subvolume(arguments: &[String]) -> bool {
    arguments.iter().any(|argument| {
        argument.strip_prefix("rootflags=").is_some_and(|options| {
            options
                .split(',')
                .any(|option| option == "subvol=@" || option == "subvol=/@")
        })
    })
}

fn grub_default_is_saved(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|content| {
        let mut effective = None;
        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "GRUB_DEFAULT" {
                effective = Some(value.trim().trim_matches(['\'', '"']).to_owned());
            }
        }
        effective.as_deref() == Some("saved")
    })
}

fn find_source_uki(paths: &AppPaths, bootctl: &str) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("DOTLAB_UKI_SOURCE").map(PathBuf::from) {
        if !path.starts_with(&paths.boot) {
            bail!("DOTLAB_UKI_SOURCE must be below {}", paths.boot.display());
        }
        return Ok(path);
    }
    for line in bootctl.lines() {
        if !line.contains("Stub:") {
            continue;
        }
        if let Some(index) = line.find("/EFI/") {
            let relative = line[index..].trim();
            let relative = relative.trim_start_matches('/');
            let path = paths.boot.join(relative);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    let directory = paths.boot.join("EFI/Linux");
    let mut candidates = if directory.is_dir() {
        fs::read_dir(&directory)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("efi"))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    candidates.sort();
    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("no UKI found below {}", directory.display()),
        _ => bail!(
            "cannot identify the running UKI among {} files; set DOTLAB_UKI_SOURCE explicitly",
            candidates.len()
        ),
    }
}

fn failed(name: &str, error: &anyhow::Error) -> Check {
    Check {
        name: name.to_owned(),
        ok: false,
        detail: format!("{error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_luks_through_device_mapper_inverse_ancestry() -> Result<()> {
        let ancestry = "\
/dev/mapper/root btrfs
/dev/nvme1n1p2 crypto_LUKS
/dev/nvme1n1
";
        assert_eq!(
            find_luks_ancestor("/dev/mapper/root", ancestry)?,
            "/dev/mapper/root inside /dev/nvme1n1p2 (crypto_LUKS)"
        );
        Ok(())
    }

    #[test]
    fn rejects_ancestry_without_luks() {
        let ancestry = "\
/dev/mapper/root btrfs
/dev/nvme1n1p2 ext4
/dev/nvme1n1
";
        assert!(find_luks_ancestor("/dev/mapper/root", ancestry).is_err());
    }

    #[test]
    fn accepts_both_base_subvolume_spellings_only() {
        for rootflags in [
            "rootflags=subvol=@",
            "rootflags=subvol=/@",
            "rootflags=compress=zstd:3,subvol=@",
            "rootflags=subvol=/@,compress=zstd:3",
        ] {
            assert!(has_base_root_subvolume(&[rootflags.to_owned()]));
        }
        for rootflags in [
            "rootflags=subvol=@home",
            "rootflags=subvol=/@snapshots/example",
            "rootflags=compress=zstd:3",
        ] {
            assert!(!has_base_root_subvolume(&[rootflags.to_owned()]));
        }
    }
}
