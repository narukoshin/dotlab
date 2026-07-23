use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "dotlab", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Verify user tools and, when available, the bare-metal layout.
    Doctor(DoctorArgs),
    /// Save the pre-Dotlab baseline and install the protected base profile.
    Init(InitArgs),
    /// Add, inspect, capture, list, or remove dotfile profiles.
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// Switch to a profile generation.
    Switch(SwitchArgs),
    /// Switch back to the preceding successful generation.
    Rollback(SwitchControlArgs),
    /// Inspect or garbage-collect packages introduced by Dotlab.
    #[command(subcommand)]
    Packages(PackagesCommand),
    /// Manage bootable Btrfs experiment slots.
    #[command(subcommand)]
    Metal(MetalCommand),
    /// Print the version.
    Version,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Require the exact LUKS/Btrfs/GRUB/UKI metal layout.
    #[arg(long)]
    pub metal: bool,
    /// Emit stable JSON for automation.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Do not ask for confirmation.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Clone a repository and create a profile from detected or explicit maps.
    Add(ProfileAddArgs),
    /// Capture selected paths from the current home into a local profile.
    Capture(ProfileCaptureArgs),
    /// Show a profile manifest and its mappings.
    Show(NameArgs),
    /// List all profiles.
    List,
    /// Remove an inactive profile and its generations.
    Remove(ProfileRemoveArgs),
}

#[derive(Debug, Args)]
pub struct ProfileAddArgs {
    pub name: String,
    pub git_url: String,
    /// Git branch, tag, or commit to check out.
    #[arg(long)]
    pub git_ref: Option<String>,
    /// Detect below this repository-relative directory.
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Explicit SRC=DEST mapping. DEST is home-relative.
    #[arg(long = "map")]
    pub mappings: Vec<String>,
    /// Official package required by this profile.
    #[arg(long = "package")]
    pub packages: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ProfileCaptureArgs {
    pub name: String,
    /// Home-relative path to capture. Repeatable.
    #[arg(long = "path", required = true)]
    pub paths: Vec<PathBuf>,
    /// Official package required by this profile.
    #[arg(long = "package")]
    pub packages: Vec<String>,
}

#[derive(Debug, Args)]
pub struct NameArgs {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ProfileRemoveArgs {
    pub name: String,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct SwitchArgs {
    pub name: String,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub yes: bool,
    /// Do not reload running desktop components.
    #[arg(long)]
    pub next_login: bool,
}

#[derive(Debug, Args)]
pub struct SwitchControlArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum PackagesCommand {
    /// Show package ownership and possible garbage.
    Status,
    /// Preserve a package permanently as user-owned.
    Keep(PackageKeepArgs),
    /// Remove unreferenced packages introduced by Dotlab.
    Gc(PackageGcArgs),
}

#[derive(Debug, Args)]
pub struct PackageKeepArgs {
    pub package: String,
}

#[derive(Debug, Args)]
pub struct PackageGcArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Subcommand)]
pub enum MetalCommand {
    /// Verify that the machine can safely use metal slots.
    Preflight(MetalPreflightArgs),
    /// Create a writable root/home slot and guarded GRUB entries.
    Create(MetalCreateArgs),
    /// Select a slot for the next boot only.
    Activate(MetalNameArgs),
    /// Make the currently booted slot the persistent primary system.
    Promote(MetalPromoteArgs),
    /// Select the preserved original system for the next boot.
    Leave(MetalNameArgs),
    /// Display all slots and identify the currently booted one.
    Status,
    /// Delete an inactive experiment slot.
    Discard(MetalDiscardArgs),
}

#[derive(Debug, Args)]
pub struct MetalPreflightArgs {
    /// Emit stable JSON for automation.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct MetalCreateArgs {
    pub name: String,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct MetalNameArgs {
    pub name: String,
    /// Reboot immediately after setting the one-shot GRUB entry.
    #[arg(long)]
    pub reboot: bool,
}

#[derive(Debug, Args)]
pub struct MetalPromoteArgs {
    pub name: String,
    /// Do not ask for confirmation.
    #[arg(long)]
    pub yes: bool,
    /// Reboot immediately after committing the persistent GRUB default.
    #[arg(long)]
    pub reboot: bool,
}

#[derive(Debug, Args)]
pub struct MetalDiscardArgs {
    pub name: String,
    #[arg(long)]
    pub yes: bool,
}
