# Headless Windows 11 under QEMU/KVM

The Windows machine of the zeughaus CI: a Windows 11 Pro guest with ssh, no
display, booted from a read-only golden image into a throwaway overlay, plus
a persistent cache disk. The `zeughaus-ci` runner owns it: it boots the VM
on demand through `vm.sh`, runs `prepare.ps1` in the guest after every
boot, freezes it through the monitor socket and stops it after an idle
timeout. By hand, everything is driven by `vm.sh`; state lives in
`/var/lib/zeughaus-ci/vm/win11` (btrfs, `chattr +C` so qcow2 writes do not
fragment). Guest account, payload directory (`C:\zeughaus`), ISO volume
label and boot task are all named `zeughaus`; the guest is `win11-ci`.

## Files

| File | Role |
| --- | --- |
| `vm.sh` | lifecycle: `iso`, `install`, `toolchain`, `run`, `ssh`, `provision`, `stop`, `status` |
| `autounattend.xml` | answer file: UEFI/GPT layout, virtio drivers, product key, Windows 11 Pro, no OOBE, local admin `zeughaus` |
| `setup.ps1` | specialize pass, offline: sshd config, power/update policy, registers the boot task |
| `firstboot.ps1` | boot task as SYSTEM: installs Win32-OpenSSH, activates, removes itself |
| `toolchain.ps1` | golden image: MSVC Build Tools, Git, CMake, LLVM, rustup-init, Defender exclusions; run by `vm.sh toolchain` |
| `prepare.ps1` | after every boot: brings the cache disk up as `W:`, cleans `W:\ci\runs`, installs Rust on `W:` when missing; the runner copies it into the guest and runs it |

## Host prerequisites

`qemu-base edk2-ovmf swtpm libisoburn socat` (Arch), `/dev/kvm` readable,
and in `$VM_DIR`:

- `win11.iso`: a multi-edition Windows 11 x64 ISO from microsoft.com (the
  answer file selects the image named `Windows 11 Pro`; `wiminfo` or
  `7z l` on the ISO shows the exact names if yours differs)
- `product-key`: a Windows 11 Pro product key on one line. `vm.sh iso`
  substitutes it into the answer file inside the shell (never in argv) and
  `iso` stops when the file is missing. The key ends up on the unattend ISO
  only.
- `virtio-win.iso`: `https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/latest-virtio/virtio-win.iso`

`SSH_PUBKEY` is a `:`-separated list of public key files (default
`$HOME/.ssh/id_ed25519.pub`); their contents are concatenated into the
guest's `authorized_keys`. The CI user cannot read `/home/hannes`, so a
second key is passed as a copy inside `$VM_DIR`.

`vm.sh iso` fetches the Win32-OpenSSH MSI from GitHub on first use (a copy
named `OpenSSH-Win64.msi` in `$VM_DIR` is used instead).

## Lifecycle

```
vm/win11/vm.sh iso           # unattend.iso with answer file, key, ssh keys, drivers
vm/win11/vm.sh install       # ~8 min, fully unattended, ends with base.qcow2 read-only
vm/win11/vm.sh toolchain     # boots base.qcow2 itself, runs toolchain.ps1, shuts down, read-only again
vm/win11/vm.sh run           # overlay boot plus cache disk, ssh reachable in ~20 s
ssh win11-ci hostname        # host entry in ~/.ssh/config; vm.sh ssh is the same
vm/win11/vm.sh stop          # ACPI power off
PERSIST=1 vm/win11/vm.sh run # boot base.qcow2 itself to change the golden image
```

Tunables (all environment variables): `VM_DIR`
(`/var/lib/zeughaus-ci/vm/win11`), `CPUS` (8), `MEM` (12G), `DISK` (128G),
`CACHE_DISK_SIZE` (100G), `SSH_PORT` (2222), `SSH_HOST` (`win11-ci`), `VNC`
(127.0.0.1:10), `GUEST_NAME` (`win11-ci`), `WIN_ISO` (`$VM_DIR/win11.iso`),
`SSH_PUBKEY`. The console is always on VNC; the local admin password is in
`$VM_DIR/password` for console use only, ssh is key-only.

The ssh side is one `Host win11-ci` block in `~/.ssh/config`
(127.0.0.1:2222, user `zeughaus`, own `known_hosts.win11-ci`,
`accept-new`). Host keys are part of the golden image, so overlays share
them; `vm.sh install` deletes the known-hosts file along with the image.

Control files in `$VM_DIR`: `monitor.sock` (HMP; `screendump`, `sendkey`,
`system_powerdown`, and `stop`/`cont` which the runner uses to freeze a
job), `qga.sock` (QEMU guest agent, JSON), `qemu.pid` (the runner checks it
for a live VM).

## Cache disk

`vm.sh run` (without `PERSIST=1`) creates `$VM_DIR/cache.qcow2`
(`CACHE_DISK_SIZE`, sparse) when missing and attaches it after the system
disk with `discard=unmap`. The disk outlives every overlay. `prepare.ps1`
initializes it on first use (GPT, NTFS, label `CICACHE`, letter `W:`) and
trims it after each boot, which returns freed space to the qcow2. Layout:
`W:\work\<repo>\<job>` workspaces, `W:\ci\runs\<id>` per run (wiped at each
boot), `W:\cargo` and `W:\rustup` (`CARGO_HOME`, `RUSTUP_HOME`, set
machine-wide by `toolchain.ps1`). Below 20 % free space `prepare.ps1`
deletes every workspace's `target` directory. A `PERSIST=1` boot, used to
change the golden image, gets no cache disk.

## What the unattended install does

1. WinPE: `LabConfig` bypasses for TPM/SecureBoot/RAM (belt and braces: the
   VM has swtpm TPM 2.0 and secure-boot OVMF), viostor/vioscsi/NetKVM from
   `E:\drivers`, GPT with EFI/MSR/NTFS, the image named `Windows 11 Pro`,
   product key from `product-key`.
2. specialize: `setup.ps1` copies the payload from the ISO to
   `C:\zeughaus`, writes sshd defaults and the
   `administrators_authorized_keys`, disables sleep/hibernation/automatic
   updates, enables developer mode, registers `zeughaus-firstboot` as an
   `onstart` task.
3. oobeSystem: all OOBE pages hidden, local account `zeughaus` in
   Administrators. No logon ever happens.
4. First boot: `firstboot.ps1` waits for the NAT gateway, installs the
   OpenSSH MSI, opens port 22, activates Windows online, deletes the task.
5. Over ssh, from the host: `pnputil` for the remaining virtio drivers and
   the qemu-ga MSI, then `shutdown /s`.
6. `vm.sh toolchain`: `toolchain.ps1` installs the build tools into the
   golden image, then `shutdown /s`.

## Things learned the hard way

- 24H2 Setup does not treat an `autounattend.xml` on a second drive as a
  configuration set: `%configsetroot%` is empty in `DriverPaths` and `$OEM$`
  next to it is ignored. Drive letters are deterministic instead: the Windows
  ISO on SATA port 0 is `D:`, ours on port 1 is `E:`.
- `RunSynchronousCommand/Path` is limited to 259 characters. Longer, and
  the specialize pass dies with "The computer restarted unexpectedly".
- The specialize pass has no network (loopback only) and the virtio-win
  guest tools installer hangs in it beyond recovery. Hence the boot task
  and the ssh-driven provisioning.
- The Windows EFI CD loader wants a key press; `vm.sh install` sends `x`
  for 30 s. Enter is wrong: from a warm page cache Setup's UI is up in 20 s
  and Enter hits its focused Cancel button.
- Windows OpenSSH's scp wants `host:/C:/path` for drive paths; if a build
  rejects that form, `host:C:/path` or `sftp` are the alternatives.

## Not done

- Network is user-mode NAT with a port forward. A tap on the WireGuard
  bridge would let the VM reach the store directly.
- No GPU: a single RTX cannot be shared with a guest; wgpu falls back to
  WARP in the VM.
