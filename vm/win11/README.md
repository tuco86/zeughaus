# Headless Windows 11 under QEMU/KVM

The first machine the zeughaus runner will boot on demand: a Windows 11
guest with ssh, no display, booted from a read-only golden image into a
throwaway overlay. Everything is driven by `vm.sh`; state lives in
`/var/lib/geselle/vm/win11` (btrfs, `chattr +C` so qcow2 writes do not
fragment). The name `geselle` (state directory, guest account, `C:\geselle`,
ssh host) is baked into the installed golden image and stays until the
image is rebuilt.

## Files

| File | Role |
| --- | --- |
| `vm.sh` | lifecycle: `iso`, `install`, `run`, `ssh`, `provision`, `stop`, `status` |
| `autounattend.xml` | answer file: UEFI/GPT layout, virtio drivers, no OOBE, local admin `geselle` |
| `setup.ps1` | specialize pass, offline: sshd config, power/update policy, registers the boot task |
| `firstboot.ps1` | boot task as SYSTEM: installs Win32-OpenSSH, activates, removes itself |

## Host prerequisites

`qemu-base edk2-ovmf swtpm libisoburn` (Arch), `/dev/kvm` readable, an ssh
public key at `~/.ssh/id_ed25519.pub` (override with `SSH_PUBKEY`), and in
`$VM_DIR`:

- `win11-enterprise-eval.iso`: Windows 11 Enterprise Evaluation from
  `https://go.microsoft.com/fwlink/?linkid=2289031` (24H2, x64, en-us)
- `virtio-win.iso`: `https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/latest-virtio/virtio-win.iso`

`vm.sh iso` fetches the Win32-OpenSSH MSI from GitHub on first use.

## Lifecycle

```
vm/win11/vm.sh install       # ~8 min, fully unattended, ends with base.qcow2 read-only
vm/win11/vm.sh run           # overlay boot, ssh reachable in ~20 s
ssh win11-geselle hostname   # host entry in ~/.ssh/config.local; vm.sh ssh is the same
vm/win11/vm.sh stop          # ACPI power off
PERSIST=1 vm/win11/vm.sh run # boot base.qcow2 itself to change the golden image
```

Tunables: `CPUS` (8), `MEM` (16G), `DISK` (128G), `SSH_PORT` (2222),
`SSH_HOST` (win11-geselle), `VNC` (127.0.0.1:0), `GUEST_NAME`
(win11-geselle). The console is always on VNC; the local admin password is
in `$VM_DIR/password` for console use only, ssh is key-only.

The ssh side is one `Host win11-geselle` block in `~/.ssh/config.local`
(127.0.0.1:2222, user `geselle`, own `known_hosts.geselle`, `accept-new`).
Host keys are part of the golden image, so overlays share them;
`vm.sh install` deletes the known-hosts file along with the image. From
another machine add `ProxyJump tuco-pc-linux` to the same block. The
config file is the right place for the WezTerm ssh domain, too.

Control sockets in `$VM_DIR`: `monitor.sock` (HMP; `screendump`, `sendkey`,
`system_powerdown`), `qga.sock` (QEMU guest agent, JSON; `guest-sync`,
`guest-exec`, `guest-shutdown`). These are what the runner will drive.

## What the unattended install does

1. WinPE: `LabConfig` bypasses for TPM/SecureBoot/RAM (belt and braces: the
   VM has swtpm TPM 2.0 and secure-boot OVMF), viostor/vioscsi/NetKVM from
   `E:\drivers`, GPT with EFI/MSR/NTFS, image index 1.
2. specialize: `setup.ps1` copies the payload from the ISO to `C:\geselle`,
   writes sshd defaults and the `administrators_authorized_keys`, disables
   sleep/hibernation/automatic updates, enables developer mode, registers
   `geselle-firstboot` as an `onstart` task.
3. oobeSystem: all OOBE pages hidden, local account `geselle` in
   Administrators. No logon ever happens.
4. First boot: `firstboot.ps1` waits for the NAT gateway, installs the
   OpenSSH MSI, opens port 22, activates the evaluation license, deletes
   the task.
5. Over ssh, from the host: `pnputil` for the remaining virtio drivers and
   the qemu-ga MSI, then `shutdown /s`.

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
- The evaluation media installs with its grace period spent; `slmgr /ato`
  online grants 90 days. Afterwards `slmgr /rearm` twice, or reinstall,
  which is one command.

## Not done

- Network is user-mode NAT with a port forward. A tap on the WireGuard
  bridge comes with the runner, so the VM can reach the store directly.
- No GPU: a single RTX cannot be shared with a guest; wgpu falls back to
  WARP in the VM.
- No toolchain yet (MSVC Build Tools, rustup, git). That is the next
  `PERSIST=1` session, or a job.
