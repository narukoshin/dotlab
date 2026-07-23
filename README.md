<h1 align="center">🧪 Dotlab</h1>

<p align="center">
  <strong>Swap Hyprland dotfiles on real Arch—and keep an escape route, dummy.</strong><br>
  a sharp little lab with a temper, a backup plan, and just enough impish chaos
</p>

<p align="center">
  <img alt="Release 1.1.0" src="https://img.shields.io/badge/release-1.1.0-cba6f7?style=flat-square">
  <img alt="Arch Linux" src="https://img.shields.io/badge/Arch-btw-1793D1?style=flat-square&logo=archlinux&logoColor=white">
  <img alt="Hyprland 0.56" src="https://img.shields.io/badge/Hyprland-0.56-b4befe?style=flat-square">
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/Rust-1.85%2B-fab387?style=flat-square&logo=rust&logoColor=black">
  <img alt="MIT license" src="https://img.shields.io/badge/license-MIT-a6e3a1?style=flat-square">
  <img alt="Experimental" src="https://img.shields.io/badge/status-EXPERIMENTAL-f38ba8?style=flat-square">
  <img alt="AI powered" src="https://img.shields.io/badge/AI-POWERED-f9e2af?style=flat-square">
</p>

> [!CAUTION]
> **Dotlab was developed with AI assistance (ChatGPT-Chan) and is provided as experimental software without warranty.**
>
> It performs high-risk system operations and may modify home configuration, Pacman packages, Btrfs snapshots, GRUB entries, and UKI files. A bug, interrupted operation, unsupported system layout, or incorrect configuration could cause data loss or leave the machine unbootable.
> 
> Back up anything important, verify that your system matches the documented requirements, inspect the code, and test recovery procedures before relying on it. You use Dotlab entirely at your own risk.
> 
> "AI-assisted" describes how the project was developed. Dotlab does not embed an AI model, assistant, telemetry client, or external AI service at runtime.
---

## What Dotlab is

Dotlab is a Rust tool for testing, swapping, and removing Hyprland dotfiles on
Arch Linux. It exists for one very specific menace: an experienced Arch user
sees one gorgeous desktop screenshot and immediately decides an unknown
installer deserves the whole machine. Pathetic! If you are going to charge
into chaos, at least train first and bring a way home.

- **Profiles** transactionally swap declared paths below your home directory.
  Repositories are treated as data, and their installers are never executed.
  Clean, quick, and almost suspiciously well behaved.
- **Metal slots** snapshot Btrfs root and home, add guarded GRUB entries,
  preserve independent UKI copies, and can promote a tested experiment without
  reinstalling it. This is where the dangerous magic lives. Hehe.

State is ordinary JSON and TOML. There is no SQLite database, daemon, Snapper,
Timeshift, or hidden AI roommate whispering into Hyprland. I brought one sword,
one rollback plan, and one smug little imp. That is already enough trouble.

## Choose your safety layer

Pick your weapon properly. Use a **profile** when the repository contains
configuration files you can map into your home directory. This is the clean,
fast, everyday duel.

Use a **metal slot** when you want to run the repository's own installer and it
may install many packages, modify `/etc`, or enable services. In other words:
when the cute theme has claws.

These modes are deliberately separate:

```text
Clean config repository      profile add -> dry-run -> switch
Invasive installation script metal preflight -> create -> boot -> test -> promote or discard
```

`dotlab switch` never runs a repository's installation script. A metal slot
does not run it automatically either; after booting the isolated slot, you run
the project's documented installer yourself. Yes, manually. Nobody receives
`sudo` just because their screenshots are pretty. If that disappoints you,
good. Hmph.

## Supported setup

Profile commands work on Arch with any normal home filesystem. Metal slots are
intentionally strict because disks are not toys, no matter what the tiny demon
on your shoulder says. They require the layout tested by this release:

```text
/                         Btrfs subvol /@
/home                     Btrfs subvol /@home
/.snapshots               Btrfs subvol /@snapshots
/var/log                  Btrfs subvol /@log
/var/cache/pacman/pkg     Btrfs subvol /@pkg
/boot                     FAT32 ESP
root Btrfs device         inside LUKS
compression               zstd:3
boot loader               GRUB 2.x
kernel                    UKI loaded by systemd-stub
Secure Boot               disabled
```

The protected base profile targets current Arch and Hyprland 0.56 and uses
`~/.config/hypr/hyprland.lua`.

Release 1.1.0 accepts both valid Btrfs kernel argument spellings,
`rootflags=subvol=@` and `rootflags=subvol=/@`, and resolves LUKS ancestry
through device-mapper's inverse block-device tree.

If metal preflight rejects your layout, stop there. That means **stop**, not
“fight the check until it becomes quiet.” Charging harder at the wrong enemy
does not make you strong. Do not reshape partitions or edit boot arguments just
to silence it. Profile mode remains available even when metal mode is
unsupported.

## Installation

Extract the release, enter its directory, and run the installer as your normal
user:

```bash
./install.sh
```

Do **not** run it with `sudo`. Normal. User. Put the giant root-shaped hammer
down before I knock it out of your hands. The installer requests privilege
only when it atomically installs `/usr/local/bin/dotlab` or asks Pacman for an
explicitly declared package.

The installer:

- verifies the shipped release binary;
- installs `dotlab` atomically;
- runs the basic doctor;
- offers to initialize the protected `base` profile;
- installs only `kitty` and `fuzzel` during initialization.

Useful alternatives:

```bash
./install.sh --no-init       # install or upgrade the manager only
./install.sh --user          # install to ~/.local/bin
./install.sh --build         # compile the locked Rust source
```

For a user-scoped installation, call metal commands with the complete path:

```bash
sudo "$HOME/.local/bin/dotlab" metal preflight
```

### Upgrading

Your profiles and journals are not stored in the release directory. Upgrade
the binary without drawing another initialization summoning circle:

```bash
./install.sh --no-init
dotlab version
```

There. Your state survives. I protected it properly, so you can stop glaring
at the installer like you are about to challenge it to a sword fight.

## First-time setup

A normal `./install.sh` offers to do this automatically. If you installed with
`--no-init`, initialize **once** as your normal user:

```bash
dotlab init
```

Initialization records the pre-Dotlab baseline and activates the protected
minimal `base` profile. Never run `dotlab init` with `sudo`. I will know.

Confirm the result:

```bash
dotlab doctor
dotlab profile list
```

The profile list should contain `base [protected]`.

## Working with profiles

Add a repository and inspect what Dotlab detected. Inspect first, trust later;
even an imp understands that much:

```bash
dotlab profile add end4 https://github.com/end-4/dots-hyprland
dotlab profile show end4
```

Automatic detection understands top-level entries below `.config`,
`dotfiles/.config`, or `home/.config`; common shell files; `.local/bin`; and
common standalone application directories.

For a repository with a different layout, declare the boundary yourself:

```bash
dotlab profile add mine https://github.com/me/dots \
  --source dots \
  --map hypr=.config/hypr \
  --map waybar=.config/waybar \
  --package waybar \
  --package hyprpaper
```

`SRC` is repository-relative, or relative to `--source`. `DEST` is
home-relative. Dotlab rejects absolute paths, `..`, nested mapping boundaries,
symlinked destination parents, overlap with its own state, and sensitive
destinations such as `.ssh` and `.gnupg`.

Preview before touching anything:

```bash
dotlab switch end4 --dry-run
```

For the first real switch, apply the files without reloading running desktop
components:

```bash
dotlab switch end4 --next-login
```

Log out of Hyprland and back in. Once you trust the profile, a normal switch
can reload the desktop immediately:

```bash
dotlab switch end4
```

Do the dry run first. Only an idiot charges without reading the terrain. I
check my footing before I draw my sword; the imp checks where the trapdoor
opens. You can manage one command, can't you?

### Going back

Retreat is not defeat when your compositor is on fire. Return to the protected
minimal profile:

```bash
dotlab switch base
```

Return to the preceding successful generation:

```bash
dotlab rollback
```

The base profile contains working monitor defaults, Kitty, Fuzzel,
window/workspace bindings, PipeWire volume bindings, and Hyprland's normal
wallpaper. Switching to it restores every path not owned by base to its
pre-Dotlab baseline.

If Hyprland cannot start, open a TTY, log in, and run:

```bash
dotlab rollback --yes
reboot
```

Or force the protected base for the next login:

```bash
dotlab switch base --next-login --yes
reboot
```

I made two escape routes because letting a broken compositor defeat you would
be pathetic. Use one, get back on your feet, and return stronger. What are you
staring at? Of course I was worried!

### Capturing your current setup

Found a setup worth keeping? Good. Snatch it away from chaos and turn selected
files from your current home into a self-contained local profile:

```bash
dotlab profile capture my-current \
  --path .config/hypr \
  --path .config/waybar \
  --path .zshrc
```

Edits made through an active Dotlab symlink belong to that immutable
generation. Capture the version you want to keep as a new local profile.

### Removing a profile

A profile must be inactive before removal. Do not cut off the branch you are
currently standing on:

```bash
dotlab switch base
dotlab profile remove end4
```

Removing a profile removes its repository data and generations. Package
cleanup is separate and reviewable.

## Transaction safety

Here is the serious, un-cute machinery. Before managing a destination for the
first time, Dotlab copies its exact baseline with:

```text
cp -a --reflink=auto
```

Every switch creates an immutable generation and a same-filesystem transaction
journal. Existing nodes are renamed into the journal before links or baseline
copies are installed.

Baseline fingerprints include:

- file contents;
- modes and ownership;
- symlink targets;
- extended attributes;
- POSIX ACL extended attributes.

Timestamps are intentionally not treated as drift.

If an operation fails, Dotlab unwinds it immediately. If the process or machine
dies between operations, the next profile command either completes the
committed state or restores every recorded node. Parent directories created
only for previously absent destinations are removed again when empty.

Dotlab refuses a switch when:

- an active top-level link no longer points to its recorded generation;
- an unmanaged destination differs from its saved baseline;
- a repository symlink escapes its mapped source subtree;
- a destination parent is a symlink;
- home and Dotlab's state directory are on different filesystems.

That refusal is me catching your hand before you grab the blade. Do not mistake
restraint for weakness. The scolding afterward? The imp demanded that part.

## Package ownership

Only official Arch packages explicitly declared with `--package` are
installed. Dotlab records the full Pacman before/after set, including
dependencies and packages left by a partially failed Pacman transaction.

Anything already installed before Dotlab is never marked as introduced.
Packages you install manually remain yours. Dotlab keeps its greedy little imp
fingers off them.

Switching profiles never removes packages. Review cleanup separately:

```bash
dotlab packages status
dotlab packages gc --dry-run
dotlab packages gc
```

Keep a Dotlab-introduced package permanently:

```bash
dotlab packages keep waybar
```

`keep` marks the package explicit with Pacman. Garbage collection removes only
unreferenced roots introduced by Dotlab and their introduced orphan
dependencies. Pacman still blocks removal when another installed package
depends on them. See? Even Pacman has more restraint than you when a new rice
drops.

## Metal slots

Now the real fight begins. Stand straight and pay attention. Use a metal slot
when a project modifies system state or when you want its complete
installation experience instead of only its clean config files. I refuse to
lose a real Arch installation to carelessness, no matter how loudly the imp
chants “do it for the rice.”

### Prepare GRUB

Ensure `/etc/default/grub` has exactly one effective setting. Exactly one. Do
not get creative here:

```text
GRUB_DEFAULT=saved
```

Regenerate GRUB once:

```bash
sudo grub-mkconfig -o /boot/grub/grub.cfg
```

### Run the read-only preflight

```bash
sudo dotlab metal preflight
```

Preflight creates no snapshots, writes no GRUB entries, and changes no state.
Every line must report `[ok]` before you continue. One failure means you do not
charge. Even strength is useless without judgment. Fix the cause or use
profile mode.

### Create and boot a slot

```bash
sudo dotlab metal create end4-test
sudo dotlab metal activate end4-test --reboot
```

Activation is one-shot. It does not crown itself as your persistent GRUB
default just because you looked away for a second.

### Verify the slot

After reboot:

```bash
sudo dotlab metal status
cat /proc/cmdline
findmnt / /home /boot
```

The kernel command line must contain `dotlab.slot=...`, root and home must
point into the slot snapshots, and `/boot` must be read-only. Check all three;
“the wallpaper appeared” is not evidence.

The slot root and home subvolumes are:

```text
@snapshots/dotlab/<slot-id>/root
@snapshots/dotlab/<slot-id>/home
```

### Try the dotfiles

The arena is ready. Draw your sword—and release the pretty chaos **inside the
slot**. Clone or enter the repository, inspect its current documentation, and
run its documented installer manually:

```bash
git clone https://github.com/end-4/dots-hyprland.git ~/end4-test
cd ~/end4-test
# Run the installation command documented by the repository.
```

Dotlab does not guess the entry point and does not silently grant an unknown
script permission. I may be an imp, but I am not *that* irresponsible. Hmph.
You are welcome.

Kernel, bootloader, and initramfs package operations that need to write
`/boot` will fail because the slot mounts it read-only. Do not remount it just
to satisfy an installer. That locked door is guarding your escape route, not
teasing you.

### Promote a tested slot

If the complete installation survives your testing and you want to keep it
exactly as it is, promote it from inside that same slot. Fine, you may crown
your chaos:

```bash
sudo dotlab metal promote end4-test
```

Promotion does not reinstall anything and does not rename live Btrfs
subvolumes. It:

- verifies that `/` and `/home` are the slot recorded in Dotlab state;
- refuses to continue during a Pacman transaction;
- verifies the normal, last-known-good, and original UKIs still match;
- changes only the slot's `/boot` entry in `fstab` from `ro` to `rw`;
- makes the existing slot GRUB identifier the persistent saved default;
- keeps separate last-known-good and preserved-original GRUB entries.

The existing GRUB identifier is deliberately retained. If the preserved
original system regenerates GRUB before its Dotlab binary is upgraded, that
identifier still boots the tested slot through its immutable UKI.

One flashy victory proves nothing. Do not promote a slot because it looked
adorable for thirty seconds. Test audio, networking, suspend, login, package
updates, and a normal reboot first. Train it until it survives. Yes, I am
ordering you around. Someone has to.

After promotion, finish any kernel hook that previously failed on the
read-only ESP:

```bash
findmnt -no OPTIONS /boot
sudo mkinitcpio -P
dkms status
sudo dotlab metal status
```

`/boot` must now report `rw`, and status marks the slot as `PRIMARY`. A
promotion is journaled around a durable commit marker: interruption before the
marker rolls back state, GRUB, `fstab`, and the mount mode; interruption after
it completes the promotion.

Promotion is intentionally conservative in 1.1.0: only one slot may be
primary, and a promoted slot cannot be discarded. Keep the preserved original
entry as your recovery path. If you boot that original system later, install
Dotlab 1.1.0 there before running metal mutations; older binaries reject the
new metal-state schema instead of guessing. Guessing around GRUB is how mortals
become cautionary tales.

### Return to your normal system

Need to visit the old system? From inside the slot:

```bash
sudo dotlab metal leave end4-test --reboot
```

Back on the original system:

```bash
sudo dotlab metal status
cat /proc/cmdline
```

Confirm that `dotlab.slot=...` is gone.

For a promoted slot, `leave` selects the preserved original for one boot only;
the promoted slot remains the persistent default. It is a visit, not an
abdication.

### Remove the slot

```bash
sudo dotlab metal discard end4-test
```

Only inactive, unpromoted slots can be discarded. No cutting away the ground
under your own boots. Discard removes GRUB references before deleting either
preserved UKI or the root/home snapshots. A create/discard/promote journal
lives on the shared
`@snapshots` subvolume at:

```text
/.snapshots/dotlab/.dotlab-state
```

That prevents a root snapshot from containing and replaying the journal that
created it. Clever, right? Of course it is. You may praise the tiny imp now.
Briefly.

## Rollback limits

A metal slot gives practical rollback for root and home, including `/etc`, the
Pacman database, user data, and system-service configuration stored there.
Real strength means knowing where your defense ends. Read the limits instead
of assuming “snapshot” means “invincible.”

These paths remain shared:

- `/boot`, protected read-only in an experiment and intentionally writable
  after promotion;
- `/var/log`;
- `/var/cache/pacman/pkg`;
- `/.snapshots`;
- every other separately mounted filesystem.

Logs and cached package files therefore remain after discard. They are not
active configuration, and retaining Pacman's cache prevents unnecessary
downloads. After promotion, later `/boot` updates are also shared and are not
rolled back with root or home; the immutable last-known-good and
preserved-original UKI copies are the boot recovery boundary.

A metal slot is not a malware sandbox. A process with root can remount `/boot`,
change firmware or NVRAM, write raw block devices, or delete snapshots. Inspect
unknown code before granting it root. Seriously. If you hand `sudo` to mystery
code and then act surprised, I will be furious—and the imp will laugh at you
while we repair it.

The automated release tests cannot certify a firmware-to-desktop reboot from a
container. The final boot test necessarily happens on your machine. Preflight
rejects mismatched layouts, Secure Boot, a busy Pacman database, a missing UKI,
and unsafe GRUB settings before it creates snapshots or boot configuration.
That is not cowardice. That is knowing which fights are real.

## State and recovery

User-owned state:

```text
~/.local/share/dotlab/       repositories, profiles, generations
~/.local/state/dotlab/       baseline, active state, switch journals, packages
```

Root-owned metal state:

```text
/.snapshots/dotlab/                   slot root/home snapshots
/.snapshots/dotlab/.dotlab-state/     shared metal state and journal
/etc/grub.d/41_dotlab                 generated entries
/boot/EFI/Linux/dotlab-*.efi          protected UKI copies
```

Never manually delete a journal. Rerun the same class of command and Dotlab
will recover it under an exclusive lock. Touch it by hand and I am confiscating
your root shell.

Pre-1.0 Bash prototype state is intentionally not imported. If you used that
prototype, roll back its active experiment first, move its old
`~/.local/share/dotlab` and `~/.local/state/dotlab` directories aside, and
keep the backup until the Rust base profile is verified.

## Command reference

```text
dotlab doctor [--metal] [--json]
dotlab init
dotlab profile add NAME URL [...]
dotlab profile capture NAME --path PATH [...]
dotlab profile show NAME
dotlab profile list
dotlab profile remove NAME
dotlab switch NAME [--dry-run] [--next-login]
dotlab rollback
dotlab packages status
dotlab packages keep PACKAGE
dotlab packages gc [--dry-run]
sudo dotlab metal preflight
sudo dotlab metal create NAME
sudo dotlab metal activate NAME [--reboot]
sudo dotlab metal promote NAME [--yes] [--reboot]
sudo dotlab metal leave NAME [--reboot]
sudo dotlab metal status
sudo dotlab metal discard NAME
```

Run `dotlab <command> --help` for every flag and option. I wrote the help; you
are going to use it.

## Testing

Automated tests throw Dotlab into a disposable little combat arena with fake
Pacman, Btrfs, GRUB, mount, and boot fixtures. They never modify the host
package database, subvolumes, ESP, or boot configuration.

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets -- --test-threads=1
cargo test --release --locked --all-targets -- --test-threads=1
```

See `TESTING.md` for the full failure-injection and release verification
matrix. Yes, it was tested. Repeatedly. Brutally. The imp sabotaged it; I made
it stand back up and fight again until it passed. You may relax by
approximately three percent.

---

<p align="center">
  <sub>Forged for stubborn Arch users, pretty Hyprland desktops, and recoverable experiments.</sub><br>
  <sub>Guarded by red-haired fury, tiny horns, and several extremely serious rollback paths.</sub>
</p>

Powered by ChatGPT-Chan.
