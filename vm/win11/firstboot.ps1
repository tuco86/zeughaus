# Scheduled task "geselle-firstboot", registered by setup.ps1: runs as SYSTEM
# at every boot until sshd runs and the evaluation license is activated,
# then removes itself. Log: C:\geselle\firstboot.log
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\geselle\firstboot.log -Append

# User-mode NAT gateway is 10.0.2.2; DHCP can lag boot by seconds.
$deadline = (Get-Date).AddMinutes(5)
while (-not (Test-Connection -ComputerName 10.0.2.2 -Count 1 -Quiet) -and (Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
}

if (-not (Get-Service -Name sshd -ErrorAction SilentlyContinue)) {
    Start-Process -Wait -FilePath msiexec.exe -ArgumentList '/i', 'C:\geselle\OpenSSH-Win64.msi', '/qn'
}
Set-Service -Name sshd -StartupType Automatic
Start-Service -Name sshd
if (-not (Get-NetFirewallRule -Name sshd -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name sshd -DisplayName 'OpenSSH Server' -Enabled True -Direction Inbound `
        -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
}

# Enterprise Evaluation media installs with its grace period already spent;
# online activation grants the 90 evaluation days. Reinstall (or slmgr
# /rearm, twice at most) when they run out.
cscript //nologo C:\Windows\System32\slmgr.vbs /ato
$licensed = (Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationID='55c92734-d682-4d71-983e-d6ec3f16059f'").LicenseStatus -contains 1

if ((Get-Service -Name sshd).Status -eq 'Running' -and $licensed) {
    schtasks /delete /tn geselle-firstboot /f
    Write-Output 'firstboot complete'
}

Stop-Transcript
