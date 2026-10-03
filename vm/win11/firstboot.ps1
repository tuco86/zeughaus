# Scheduled task "zeughaus-firstboot", registered by setup.ps1: runs as SYSTEM
# at every boot until sshd runs and Windows is activated, then removes
# itself. Log: C:\zeughaus\firstboot.log
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\zeughaus\firstboot.log -Append

# User-mode NAT gateway is 10.0.2.2; DHCP can lag boot by seconds.
$deadline = (Get-Date).AddMinutes(5)
while (-not (Test-Connection -ComputerName 10.0.2.2 -Count 1 -Quiet) -and (Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 3
}

if (-not (Get-Service -Name sshd -ErrorAction SilentlyContinue)) {
    Start-Process -Wait -FilePath msiexec.exe -ArgumentList '/i', 'C:\zeughaus\OpenSSH-Win64.msi', '/qn'
}
Set-Service -Name sshd -StartupType Automatic
Start-Service -Name sshd
if (-not (Get-NetFirewallRule -Name sshd -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name sshd -DisplayName 'OpenSSH Server' -Enabled True -Direction Inbound `
        -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
}

# The product key from the answer file is installed during Setup; online
# activation turns it into a licensed system. Retried at every boot until it
# succeeds.
cscript //nologo C:\Windows\System32\slmgr.vbs /ato
$licensed = (Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationID='55c92734-d682-4d71-983e-d6ec3f16059f'").LicenseStatus -contains 1

if ((Get-Service -Name sshd).Status -eq 'Running' -and $licensed) {
    schtasks /delete /tn zeughaus-firstboot /f
    Write-Output 'firstboot complete'
}

Stop-Transcript
