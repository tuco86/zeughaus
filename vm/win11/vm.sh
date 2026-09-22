#!/usr/bin/env bash
# Headless Windows 11 under QEMU/KVM for the zeughaus runner.
#
#   vm.sh iso        build unattend.iso from autounattend.xml, setup.ps1,
#                    the ssh public key and the virtio boot drivers
#   vm.sh install    create base.qcow2, run the unattended install until
#                    ssh answers, provision, shut down, mark base.qcow2
#                    read-only
#   vm.sh run        boot a throwaway overlay of base.qcow2 (PERSIST=1 boots
#                    base.qcow2 itself, for updating the golden image)
#   vm.sh ssh [cmd]  ssh into the running VM
#   vm.sh provision  install remaining virtio drivers and qemu-ga over ssh
#   vm.sh stop       ACPI power off, wait for QEMU to exit
#   vm.sh status
#
# State lives in $VM_DIR (default /var/lib/geselle/vm/win11, nodatacow).
# The console is on VNC $VNC while the VM runs; ssh is forwarded to
# 127.0.0.1:$SSH_PORT and reached as ssh host $SSH_HOST, whose entry lives
# in ~/.ssh/config.local. What the runner will later drive over QMP is the
# same QEMU command line as here.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
VM_DIR=${VM_DIR:-/var/lib/geselle/vm/win11}
CPUS=${CPUS:-8}
MEM=${MEM:-16G}
DISK=${DISK:-128G}
SSH_PORT=${SSH_PORT:-2222}
SSH_HOST=${SSH_HOST:-win11-geselle}
VNC=${VNC:-127.0.0.1:0}
GUEST_NAME=${GUEST_NAME:-win11-geselle}
SSH_PUBKEY=${SSH_PUBKEY:-$HOME/.ssh/id_ed25519.pub}

WIN_ISO=$VM_DIR/win11-enterprise-eval.iso
VIRTIO_ISO=$VM_DIR/virtio-win.iso
OPENSSH_MSI=$VM_DIR/OpenSSH-Win64.msi
UNATTEND_ISO=$VM_DIR/unattend.iso
BASE=$VM_DIR/base.qcow2
OVERLAY=$VM_DIR/run.qcow2
OVMF_CODE=/usr/share/edk2/x64/OVMF_CODE.secboot.4m.fd
OVMF_VARS=$VM_DIR/OVMF_VARS.fd
MON=$VM_DIR/monitor.sock
QGA=$VM_DIR/qga.sock
TPM_DIR=$VM_DIR/tpm
TPM_SOCK=$VM_DIR/swtpm.sock
PIDFILE=$VM_DIR/qemu.pid
QEMU_LOG=$VM_DIR/qemu.log

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
die() { log "$*"; exit 1; }

running() { [ -s "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; }
monitor() { printf '%s\n' "$*" | socat - "UNIX-CONNECT:$MON" >/dev/null; }

ssh_cmd() { ssh "$SSH_HOST" "$@"; }

# ---------------------------------------------------------------- btrfs

# btrfs checksums every data write. The image is written through O_DIRECT
# (cache=none in disk_args), so the guest can change a page after btrfs has
# taken its checksum; that block then reads back as EIO although the SSD
# reports no error at all. The guest sees STATUS_IN_PAGE_ERROR, a build in
# it sees LNK1201 without a source line. nodatacow drops the checksums (and
# compression) for the image files, which is how VM images are kept on
# btrfs. New files inherit the flag from their directory, so it has to be
# set before an image is created.
nodatacow() {
    [ "$(findmnt -no FSTYPE -T "$1" 2>/dev/null)" = btrfs ] || return 0
    chattr +C "$1" 2>/dev/null || log "warning: no nodatacow on $1"
}

# An image created before that flag existed keeps its checksums; only
# rewriting the file clears them, and that needs the vm stopped.
warn_datacow() {
    [ -e "$1" ] || return 0
    [ "$(findmnt -no FSTYPE -T "$1" 2>/dev/null)" = btrfs ] || return 0
    case "$(lsattr -l "$1" 2>/dev/null)" in
        *No_COW*) ;;
        *) log "warning: $1 still has btrfs checksums; with the vm stopped: cp --reflink=never $1 $1.new && mv -f $1.new $1" ;;
    esac
}

# ---------------------------------------------------------------- iso

iso() {
    [ -s "$VIRTIO_ISO" ] || die "missing $VIRTIO_ISO"
    [ -s "$SSH_PUBKEY" ] || die "missing ssh public key $SSH_PUBKEY"
    local stage=$VM_DIR/iso-root
    local payload="$stage/geselle"
    rm -rf "$stage"
    mkdir -p "$stage/drivers" "$payload/drivers"

    # /drivers: boot-critical, loaded by Windows Setup from E:\drivers.
    # /geselle/drivers: the rest, installed with pnputil over ssh later.
    local d
    for d in viostor vioscsi NetKVM; do
        xorriso -osirrox on -indev "$VIRTIO_ISO" -extract "/$d/w11/amd64" "$stage/drivers/$d" 2>/dev/null
    done
    for d in Balloon vioserial viorng pvpanic qemufwcfg qemupciserial; do
        xorriso -osirrox on -indev "$VIRTIO_ISO" -extract "/$d/w11/amd64" "$payload/drivers/$d" 2>/dev/null
    done
    xorriso -osirrox on -indev "$VIRTIO_ISO" \
        -extract /guest-agent/qemu-ga-x86_64.msi "$payload/qemu-ga-x86_64.msi" 2>/dev/null
    chmod -R u+w "$stage"

    # Win32-OpenSSH from GitHub: the in-box capability needs Windows Update
    # at a point where the guest has no network yet.
    if [ ! -s "$OPENSSH_MSI" ]; then
        local url
        url=$(curl -fsSL https://api.github.com/repos/PowerShell/Win32-OpenSSH/releases/latest |
            jq -r '.assets[] | select(.name | test("^OpenSSH-Win64-v.*\\.msi$")) | .browser_download_url')
        [ -n "$url" ] || die "could not resolve the Win32-OpenSSH release asset"
        curl -fsSL -o "$OPENSSH_MSI" "$url"
    fi
    cp "$OPENSSH_MSI" "$payload/OpenSSH-Win64.msi"

    if [ ! -s "$VM_DIR/password" ]; then
        (umask 077; head -c 256 /dev/urandom | LC_ALL=C tr -dc 'A-Za-z0-9' | cut -c1-24 >"$VM_DIR/password")
    fi
    sed -e "s|@@PASSWORD@@|$(cat "$VM_DIR/password")|" \
        -e "s|@@HOSTNAME@@|$GUEST_NAME|" \
        "$HERE/autounattend.xml" >"$stage/autounattend.xml"
    cp "$HERE/setup.ps1" "$HERE/firstboot.ps1" "$payload/"
    cp "$SSH_PUBKEY" "$payload/authorized_keys"

    xorriso -as mkisofs -quiet -o "$UNATTEND_ISO" -J -R -V GESELLE "$stage"
    rm -rf "$stage"
    log "built $UNATTEND_ISO"
}

# ---------------------------------------------------------------- qemu

start_tpm() {
    mkdir -p "$TPM_DIR"
    swtpm socket --tpm2 --tpmstate "dir=$TPM_DIR" \
        --ctrl "type=unixio,path=$TPM_SOCK" --terminate --daemon
}

# Common machine definition. Extra args select disk and media.
qemu_base_args() {
    [ -s "$OVMF_VARS" ] || cp /usr/share/edk2/x64/OVMF_VARS.4m.fd "$OVMF_VARS"
    printf '%s\n' \
        -name "$GUEST_NAME" \
        -machine q35,accel=kvm,smm=on \
        -cpu host,topoext,hv_relaxed,hv_vapic,hv_spinlocks=0x1fff,hv_time,hv_synic,hv_stimer,hv_vpindex,hv_runtime,hv_frequencies,hv_tlbflush,hv_ipi \
        -smp "$CPUS,sockets=1,cores=$((CPUS / 2)),threads=2" \
        -m "$MEM" \
        -rtc base=utc,clock=host \
        -global driver=cfi.pflash01,property=secure,value=on \
        -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
        -drive "if=pflash,format=raw,file=$OVMF_VARS" \
        -chardev "socket,id=chrtpm,path=$TPM_SOCK" \
        -tpmdev emulator,id=tpm0,chardev=chrtpm \
        -device tpm-tis,tpmdev=tpm0 \
        -device qemu-xhci -device usb-tablet \
        -vga std \
        -display none -vnc "$VNC" \
        -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22" \
        -device virtio-net-pci,netdev=net0 \
        -device virtio-serial-pci \
        -chardev "socket,id=qga0,path=$QGA,server=on,wait=off" \
        -device virtserialport,chardev=qga0,name=org.qemu.guest_agent.0 \
        -device virtio-balloon-pci \
        -device virtio-rng-pci \
        -monitor "unix:$MON,server,nowait" \
        -pidfile "$PIDFILE" \
        -daemonize
}

disk_args() {
    printf '%s\n' \
        -drive "if=none,id=hd0,format=qcow2,discard=unmap,cache=none,aio=native,file=$1" \
        -device virtio-blk-pci,drive=hd0,bootindex=2
}

launch() {
    running && die "already running, pid $(cat "$PIDFILE")"
    start_tpm
    local -a args
    mapfile -t args < <(qemu_base_args)
    qemu-system-x86_64 "${args[@]}" "$@" 2>"$QEMU_LOG" || die "qemu failed: $(cat "$QEMU_LOG")"
    log "qemu pid $(cat "$PIDFILE"), vnc $VNC, ssh $SSH_HOST"
}

wait_exit() {
    while running; do sleep 2; done
}

wait_ssh() {
    local deadline=$(( $(date +%s) + ${1:-3600} ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        running || die "qemu exited during install: $(cat "$QEMU_LOG")"
        if ssh_cmd hostname >/dev/null 2>&1; then return 0; fi
        sleep 10
    done
    die "ssh did not come up in time"
}

# ---------------------------------------------------------------- commands

install() {
    [ -s "$WIN_ISO" ] || die "missing $WIN_ISO"
    [ -s "$UNATTEND_ISO" ] || iso
    [ -e "$BASE" ] && die "$BASE exists; remove it to reinstall"
    rm -f "$OVMF_VARS"
    rm -rf "$TPM_DIR"
    rm -f "$HOME/.ssh/known_hosts.geselle"
    nodatacow "$VM_DIR"
    qemu-img create -q -f qcow2 "$BASE" "$DISK"

    local -a media
    mapfile -t media < <(disk_args "$BASE"; printf '%s\n' \
        -drive "if=none,id=cd0,media=cdrom,file=$WIN_ISO" \
        -device ide-cd,drive=cd0,bus=ide.0,bootindex=1 \
        -drive "if=none,id=cd1,media=cdrom,file=$UNATTEND_ISO" \
        -device ide-cd,drive=cd1,bus=ide.1)
    launch "${media[@]}"

    # The Windows EFI loader asks for a key press before booting from CD
    # and otherwise falls through to the (empty) disk; on later reboots that
    # fall-through is what boots the installed system. Any key satisfies
    # the prompt. Not Enter: once Setup's UI is up (after ~20 s from a warm
    # cache) Enter hits its focused Cancel button.
    local i
    for i in $(seq 1 30); do monitor sendkey x; sleep 1; done
    log "install running; watch it on vnc $VNC"

    wait_ssh 3600
    log "ssh answers, guest setup log follows"
    ssh_cmd 'Get-Content C:\geselle\setup.log' || true
    provision
    ssh_cmd 'shutdown /s /t 0' || true
    wait_exit
    chmod 444 "$BASE"
    log "golden image ready: $BASE"
}

# In-guest steps that are not safe in the specialize pass: the remaining
# virtio drivers and the QEMU guest agent. Idempotent; PERSIST=1 run + this
# is how the golden image gets updated.
provision() {
    log "installing virtio drivers and qemu-ga"
    ssh_cmd 'pnputil /add-driver C:\geselle\drivers\*.inf /subdirs /install' || true
    ssh_cmd 'Start-Process -Wait msiexec.exe -ArgumentList "/i","C:\geselle\qemu-ga-x86_64.msi","/qn"; Get-Service QEMU-GA | Select-Object Status,StartType'
    ssh_cmd 'Get-PnpDevice -PresentOnly | Where-Object { $_.Status -ne "OK" } | Select-Object Status,Class,FriendlyName'
}

run() {
    [ -s "$BASE" ] || die "no base image; run install first"
    nodatacow "$VM_DIR"
    local disk=$BASE
    if [ "${PERSIST:-0}" != 1 ]; then
        disk=$OVERLAY
        rm -f "$OVERLAY"
        qemu-img create -q -f qcow2 -b "$BASE" -F qcow2 "$OVERLAY"
    else
        warn_datacow "$BASE"
        chmod 644 "$BASE"
    fi
    local -a media
    mapfile -t media < <(disk_args "$disk")
    launch "${media[@]}"
    wait_ssh 300
    log "up"
}

stop() {
    running || { log "not running"; return 0; }
    monitor system_powerdown
    local i
    for i in $(seq 1 60); do running || break; sleep 2; done
    if running; then log "no clean shutdown, killing"; monitor quit; wait_exit; fi
    [ "${PERSIST:-0}" = 1 ] && chmod 444 "$BASE"
    log "stopped"
}

status() {
    if running; then
        log "running, pid $(cat "$PIDFILE"), vnc $VNC, ssh $SSH_HOST"
    else
        log "stopped"
    fi
    [ -s "$BASE" ] && qemu-img info "$BASE" | sed -n '/virtual size\|disk size/p'
    true
}

case "${1:-}" in
    iso) iso ;;
    install) install ;;
    run) run ;;
    ssh) shift; ssh_cmd "$@" ;;
    stop) stop ;;
    status) status ;;
    provision) provision ;;
    *) sed -n '2,19p' "$0"; exit 2 ;;
esac
