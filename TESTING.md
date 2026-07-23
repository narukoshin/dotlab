# Safety verification

Dotlab's automated tests do not touch the host package database, Btrfs
subvolumes, ESP, or GRUB config. Every behavioral test runs the real Rust
binary against a disposable home and command fixtures.

Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked --all-targets -- --test-threads=1
```

The suite currently covers:

- baseline capture and restoration of content, modes, ownership, symlinks,
  extended attributes, and ACL xattrs;
- automatic and explicit repository mapping;
- a no-write dry run;
- immediate rollback after an injected mid-switch error;
- recovery after a simulated hard process exit between rename operations;
- rejection of modified active links and modified unmanaged baselines;
- rejection of hierarchical mappings, sensitive paths, escaping repository
  symlinks, and symlinked home parents;
- cleanup of parent directories which did not exist before a profile;
- protected-base behavior, local profile capture, rollback history, and profile
  removal;
- Pacman before/after ownership including dependencies;
- package preservation while referenced and explicit garbage collection after
  profile removal;
- exact metal-layout preflight with LUKS, Btrfs, compression, FAT32 ESP, GRUB,
  UKI, kernel arguments, and Secure Boot checks, including device-mapper
  ancestry and both `subvol=@` and `subvol=/@` root-flag spellings;
- root/home slot snapshot creation and slot-specific `fstab` rewriting;
- verified UKI copies and generated experiment/original GRUB entries;
- temporary GRUB generation plus script validation before atomic replacement;
- one-shot activation, read-only ESP remount/restore, and leave;
- promotion only from the matching mounted root/home slot;
- persistent GRUB selection with normal, last-known-good, and original UKIs;
- writable-ESP transition after promotion and promoted-slot deletion refusal;
- rollback before the promotion commit marker and completion after it;
- metal-state schema migration which makes older binaries fail closed;
- refusal to discard while a slot is booted;
- deletion ordering for snapshots, UKIs, state, and GRUB entries;
- unwind after a partial Btrfs snapshot failure;
- completion of a journal after GRUB generation is interrupted.

For a built release archive:

```bash
tests/archive-smoke.sh ../dotlab-1.1.0-x86_64.tar.gz
```

That extracts the archive, verifies the binary checksum through the real
installer, performs a disposable user install, and executes the installed
binary.

## Manual release checks on the target Arch machine

The automated fixtures validate command construction and failure ordering, but
they cannot emulate firmware or actually reboot. Before trusting metal slots:

```bash
sudo dotlab metal preflight
```

Then create a disposable slot, boot it, confirm these commands show the slot
root/home and read-only ESP, leave, and discard it:

```bash
findmnt / /home /boot
cat /proc/cmdline
sudo dotlab metal status
```

After returning to the original system, verify `/proc/cmdline` has no
`dotlab.slot=`, then run `sudo dotlab metal discard NAME`.

For a promotion rehearsal, create a separate disposable slot, boot it, and run:

```bash
sudo dotlab metal promote NAME
findmnt -no OPTIONS /boot
sudo mkinitcpio -P
sudo dotlab metal status
```

Confirm `/boot` is writable, status reports `PRIMARY`, and the GRUB menu
contains primary, last-known-good, and preserved-original entries. Do not use
the first promotion rehearsal on a slot containing irreplaceable data.
