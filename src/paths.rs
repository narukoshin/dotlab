use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub repos: PathBuf,
    pub profiles: PathBuf,
    pub generations: PathBuf,
    pub baseline: PathBuf,
    pub transactions: PathBuf,
    pub metal_state: PathBuf,
    pub snapshot_root: PathBuf,
    pub boot: PathBuf,
    pub proc_cmdline: PathBuf,
    pub grub_defaults: PathBuf,
    pub grub_script: PathBuf,
    pub grub_config: PathBuf,
    pub pacman_lock: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home = required_absolute_env("HOME")?;
        let data = env_path("DOTLAB_HOME")
            .or_else(|| env_path("XDG_DATA_HOME").map(|path| path.join("dotlab")))
            .unwrap_or_else(|| home.join(".local/share/dotlab"));
        let state = env_path("DOTLAB_STATE")
            .or_else(|| env_path("XDG_STATE_HOME").map(|path| path.join("dotlab")))
            .unwrap_or_else(|| home.join(".local/state/dotlab"));
        let snapshot_root =
            env_path("DOTLAB_SNAPSHOT_ROOT").unwrap_or_else(|| PathBuf::from("/.snapshots/dotlab"));
        // Keep the journal on the separately mounted @snapshots subvolume.
        // A root snapshot must not contain a stale copy of its own creation
        // journal, otherwise booting that slot could replay the operation.
        let metal_state =
            env_path("DOTLAB_METAL_STATE").unwrap_or_else(|| snapshot_root.join(".dotlab-state"));
        let boot = env_path("DOTLAB_BOOT").unwrap_or_else(|| PathBuf::from("/boot"));
        let proc_cmdline =
            env_path("DOTLAB_PROC_CMDLINE").unwrap_or_else(|| PathBuf::from("/proc/cmdline"));
        let grub_defaults =
            env_path("DOTLAB_GRUB_DEFAULTS").unwrap_or_else(|| PathBuf::from("/etc/default/grub"));
        let grub_script = env_path("DOTLAB_GRUB_SCRIPT")
            .unwrap_or_else(|| PathBuf::from("/etc/grub.d/41_dotlab"));
        let grub_config =
            env_path("DOTLAB_GRUB_CONFIG").unwrap_or_else(|| PathBuf::from("/boot/grub/grub.cfg"));
        let pacman_lock = env_path("DOTLAB_PACMAN_LOCK")
            .unwrap_or_else(|| PathBuf::from("/var/lib/pacman/db.lck"));

        Ok(Self {
            home,
            repos: data.join("repos"),
            profiles: data.join("profiles"),
            generations: data.join("generations"),
            baseline: state.join("baseline"),
            transactions: state.join("transactions"),
            data,
            state,
            metal_state,
            snapshot_root,
            boot,
            proc_cmdline,
            grub_defaults,
            grub_script,
            grub_config,
            pacman_lock,
        })
    }

    pub fn ensure_user_dirs(&self) -> Result<()> {
        for path in [
            &self.data,
            &self.state,
            &self.repos,
            &self.profiles,
            &self.generations,
            &self.baseline,
            &self.transactions,
        ] {
            std::fs::create_dir_all(path)
                .with_context(|| format!("creating {}", path.display()))?;
        }
        Ok(())
    }

    pub fn home_path(&self, relative: &Path) -> Result<PathBuf> {
        validate_relative(relative)?;
        Ok(self.home.join(relative))
    }

    pub fn profile_dir(&self, name: &str) -> PathBuf {
        self.profiles.join(name)
    }

    pub fn profile_manifest(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("profile.toml")
    }

    pub fn active_state(&self) -> PathBuf {
        self.state.join("active.json")
    }

    pub fn baseline_index(&self) -> PathBuf {
        self.baseline.join("index.json")
    }

    pub fn package_state(&self) -> PathBuf {
        self.state.join("packages.json")
    }

    pub fn user_lock(&self) -> PathBuf {
        self.state.join("dotlab.lock")
    }

    pub fn metal_index(&self) -> PathBuf {
        self.metal_state.join("slots.json")
    }

    pub fn metal_lock(&self) -> PathBuf {
        self.metal_state.join("dotlab.lock")
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn required_absolute_env(name: &str) -> Result<PathBuf> {
    let path = env_path(name).with_context(|| format!("{name} is not set"))?;
    if !path.is_absolute() {
        bail!("{name} must be absolute: {}", path.display());
    }
    Ok(path)
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
        })
    {
        bail!(
            "invalid name {name:?}; use 1-64 ASCII letters, digits, dots, dashes, or underscores"
        );
    }
    Ok(())
}

pub fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!(
            "expected a non-empty home-relative path: {}",
            path.display()
        );
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => {
                let bytes = value.as_encoded_bytes();
                if bytes.contains(&b'\n') || bytes.contains(&b'\r') || bytes.contains(&0) {
                    bail!("unsupported control character in path: {}", path.display());
                }
            }
            _ => bail!("unsafe path component in {}", path.display()),
        }
    }
    Ok(())
}
