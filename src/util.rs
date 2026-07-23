use std::ffi::CString;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

pub fn ensure_unprivileged() -> Result<()> {
    if effective_uid()? == 0 && std::env::var_os("DOTLAB_TEST_MODE").is_none() {
        bail!("profile commands must run as your normal user, not through sudo");
    }
    Ok(())
}

pub fn ensure_root() -> Result<()> {
    if effective_uid()? != 0 && std::env::var_os("DOTLAB_TEST_MODE").is_none() {
        bail!("this metal command needs root; run it with sudo");
    }
    Ok(())
}

fn effective_uid() -> Result<u32> {
    let status = fs::read_to_string("/proc/self/status").context("reading /proc/self/status")?;
    let line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .context("finding effective uid")?;
    line.split_whitespace()
        .nth(2)
        .context("parsing effective uid")?
        .parse()
        .context("parsing effective uid")
}

pub fn acquire_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    file.try_lock_exclusive()
        .with_context(|| format!("another Dotlab process holds {}", path.display()))?;
    Ok(file)
}

pub fn unique_id() -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock predates Unix epoch")?
        .as_nanos();
    let mut random = [0_u8; 6];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .context("reading /dev/urandom")?;
    Ok(format!(
        "{nanos:x}-{:x}-{}",
        std::process::id(),
        hex::encode(random)
    ))
}

pub fn timestamp() -> Result<String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock predates Unix epoch")?
        .as_secs()
        .to_string())
}

pub fn node_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

pub fn remove_node(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

pub fn move_node(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::rename(source, destination).with_context(|| {
        format!(
            "moving {} to {} (both must be on the same filesystem)",
            source.display(),
            destination.display()
        )
    })
}

pub fn copy_node(source: &Path, destination: &Path) -> Result<()> {
    if !node_exists(source) {
        bail!("copy source does not exist: {}", source.display());
    }
    if node_exists(destination) {
        bail!("copy destination already exists: {}", destination.display());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    run_checked(
        "cp",
        [
            OsString::from("-a"),
            OsString::from("--reflink=auto"),
            OsString::from("--"),
            source.as_os_str().to_owned(),
            destination.as_os_str().to_owned(),
        ],
    )
    .with_context(|| format!("copying {} to {}", source.display(), destination.display()))?;
    Ok(())
}

pub fn create_symlink(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    symlink(source, destination)
        .with_context(|| format!("linking {} to {}", destination.display(), source.display()))
}

pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    let temporary = parent.join(format!(
        ".{}.dotlab-{}",
        file_name.to_string_lossy(),
        unique_id()?
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(mode)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting permissions on {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing {}", parent.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serializing JSON")?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, 0o600)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

pub fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value).context("serializing TOML")?;
    atomic_write(path, text.as_bytes(), 0o600)
}

pub fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn run_checked<I, S>(program: &str, arguments: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = arguments
        .into_iter()
        .map(|value| value.as_ref().to_owned())
        .collect();
    let output = Command::new(program)
        .args(&args)
        .output()
        .with_context(|| format!("starting {program}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        bail!(
            "{} failed with {}{}",
            command_display(program, &args),
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
    }
    Ok(output)
}

pub fn run_stdout<I, S>(program: &str, arguments: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = run_checked(program, arguments)?;
    String::from_utf8(output.stdout)
        .context("command returned non-UTF-8 output")
        .map(|value| value.trim().to_owned())
}

fn command_display(program: &str, args: &[OsString]) -> String {
    let mut display = program.to_owned();
    for argument in args {
        display.push(' ');
        display.push_str(&format!("{:?}", argument));
    }
    display
}

pub fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", "command -v -- \"$1\" >/dev/null 2>&1", "sh", program])
        .status()
        .is_ok_and(|status| status.success())
}

pub fn validate_tree(path: &Path) -> Result<()> {
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry.with_context(|| format!("walking {}", path.display()))?;
        let file_type = entry.file_type();
        if !(file_type.is_dir() || file_type.is_file() || file_type.is_symlink()) {
            bail!(
                "unsupported socket, device, or FIFO in dotfiles: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

pub fn fingerprint(path: &Path) -> Result<String> {
    if !node_exists(path) {
        return Ok("absent".to_owned());
    }
    let root_parent = path.parent().unwrap_or_else(|| Path::new("/"));
    let mut entries: Vec<PathBuf> = WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .map(|entry| entry.map(|entry| entry.into_path()))
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("walking {}", path.display()))?;
    entries.sort_by(|left, right| {
        left.as_os_str()
            .as_bytes()
            .cmp(right.as_os_str().as_bytes())
    });

    let mut digest = Sha256::new();
    for entry in entries {
        let relative = entry.strip_prefix(root_parent).unwrap_or(&entry);
        digest.update(relative.as_os_str().as_bytes());
        digest.update([0]);
        let metadata = fs::symlink_metadata(&entry)
            .with_context(|| format!("inspecting {}", entry.display()))?;
        digest.update(metadata.mode().to_le_bytes());
        digest.update(metadata.uid().to_le_bytes());
        digest.update(metadata.gid().to_le_bytes());
        if metadata.file_type().is_symlink() {
            digest.update(b"L");
            digest.update(
                fs::read_link(&entry)
                    .with_context(|| format!("reading link {}", entry.display()))?
                    .as_os_str()
                    .as_bytes(),
            );
        } else if metadata.is_file() {
            digest.update(b"F");
            let mut file =
                File::open(&entry).with_context(|| format!("reading {}", entry.display()))?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file
                    .read(&mut buffer)
                    .with_context(|| format!("reading {}", entry.display()))?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
        } else if metadata.is_dir() {
            digest.update(b"D");
        } else {
            bail!("unsupported file type: {}", entry.display());
        }
        hash_xattrs(&entry, &mut digest)?;
        digest.update([0xff]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn hash_xattrs(path: &Path, digest: &mut Sha256) -> Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path contains NUL: {}", path.display()))?;
    // l* calls intentionally do not follow symlinks. The first call obtains
    // the kernel-sized buffer; a concurrent xattr change fails closed.
    let list_size = unsafe { libc::llistxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
    if list_size < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOTSUP) {
            return Ok(());
        }
        return Err(error).with_context(|| format!("listing xattrs on {}", path.display()));
    }
    if list_size == 0 {
        return Ok(());
    }
    let mut list = vec![0_u8; list_size as usize];
    let actual = unsafe {
        libc::llistxattr(
            c_path.as_ptr(),
            list.as_mut_ptr().cast::<libc::c_char>(),
            list.len(),
        )
    };
    if actual < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("listing xattrs on {}", path.display()));
    }
    list.truncate(actual as usize);
    let mut names = list
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    names.sort();
    for name in names {
        let c_name = CString::new(name.clone()).context("xattr name contains NUL")?;
        let value_size =
            unsafe { libc::lgetxattr(c_path.as_ptr(), c_name.as_ptr(), std::ptr::null_mut(), 0) };
        if value_size < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "reading xattr {} on {}",
                    String::from_utf8_lossy(&name),
                    path.display()
                )
            });
        }
        let mut value = vec![0_u8; value_size as usize];
        if value_size > 0 {
            let actual = unsafe {
                libc::lgetxattr(
                    c_path.as_ptr(),
                    c_name.as_ptr(),
                    value.as_mut_ptr().cast::<libc::c_void>(),
                    value.len(),
                )
            };
            if actual < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "reading xattr {} on {}",
                        String::from_utf8_lossy(&name),
                        path.display()
                    )
                });
            }
            value.truncate(actual as usize);
        }
        digest.update(b"X");
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(&name);
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(&value);
    }
    Ok(())
}

pub fn file_digest(path: &Path) -> Result<String> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("expected a regular file: {}", path.display());
    }
    let mut digest = Sha256::new();
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}

pub fn path_key(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_bytes());
    hex::encode(digest.finalize())
}

pub fn read_link_absolute(path: &Path) -> Result<PathBuf> {
    let target = fs::read_link(path).with_context(|| format!("reading link {}", path.display()))?;
    if target.is_absolute() {
        Ok(target)
    } else {
        Ok(path.parent().unwrap_or_else(|| Path::new("/")).join(target))
    }
}

pub fn prompt_yes(message: &str, assume_yes: bool) -> Result<()> {
    if assume_yes {
        return Ok(());
    }
    eprint!("{message} [y/N] ");
    std::io::stderr().flush().context("flushing prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading confirmation")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
        bail!("cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fingerprint_changes_with_content_and_target() -> Result<()> {
        let temporary = tempdir()?;
        let root = temporary.path().join("tree");
        fs::create_dir(&root)?;
        fs::write(root.join("a"), "one")?;
        symlink("a", root.join("link"))?;
        let first = fingerprint(&root)?;
        fs::write(root.join("a"), "two")?;
        assert_ne!(first, fingerprint(&root)?);
        Ok(())
    }

    #[test]
    fn fingerprint_includes_extended_attributes() -> Result<()> {
        let temporary = tempdir()?;
        let path = temporary.path().join("file");
        fs::write(&path, "same contents")?;
        let before = fingerprint(&path)?;
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        let name = c"user.dotlab-test";
        let value = b"changed";
        let result = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast::<libc::c_void>(),
                value.len(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error()).context("setting test xattr");
        }
        assert_ne!(before, fingerprint(&path)?);
        Ok(())
    }

    #[test]
    fn atomic_json_round_trip() -> Result<()> {
        let temporary = tempdir()?;
        let path = temporary.path().join("state.json");
        write_json(&path, &vec!["hello"])?;
        let value: Vec<String> = read_json(&path)?;
        assert_eq!(value, ["hello"]);
        Ok(())
    }
}
