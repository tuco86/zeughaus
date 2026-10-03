# Runs as SYSTEM during the specialize pass (see autounattend.xml), before
# any logon. The pass has no network (no DHCP lease, only loopback) and the
# virtio-win installer wedges in it, so this script only does offline work
# and registers firstboot.ps1 as a scheduled task that runs at every boot
# until ssh and activation are in place. Log: C:\zeughaus\setup.log
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\zeughaus\setup.log -Force

# sshd configuration ahead of the install: default shell and the
# administrators key file the Win32-OpenSSH sshd_config refers to.
New-Item -Path HKLM:\SOFTWARE\OpenSSH -Force | Out-Null
New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell `
    -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -PropertyType String -Force | Out-Null
New-Item -ItemType Directory -Path C:\ProgramData\ssh -Force | Out-Null
Copy-Item -Path C:\zeughaus\authorized_keys -Destination C:\ProgramData\ssh\administrators_authorized_keys -Force
icacls C:\ProgramData\ssh\administrators_authorized_keys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F'

# Headless build host: never sleep, no hibernation file, no update-driven
# reboots in the middle of a job.
powercfg /hibernate off
powercfg /change standby-timeout-ac 0
powercfg /change monitor-timeout-ac 0
powercfg /change disk-timeout-ac 0
reg add HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU /v NoAutoUpdate /t REG_DWORD /d 1 /f
reg add HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU /v NoAutoRebootWithLoggedOnUsers /t REG_DWORD /d 1 /f

# Developer mode: symlinks without elevation, which cargo and git expect.
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock" /v AllowDevelopmentWithoutDevLicense /t REG_DWORD /d 1 /f

# Network-dependent work runs at boot, as SYSTEM, once services are up.
schtasks /create /tn zeughaus-firstboot /sc onstart /ru SYSTEM /rl HIGHEST /f `
    /tr 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\zeughaus\firstboot.ps1'

Stop-Transcript
