# One-time setup of a Windows host for a zeughaus runner, run elevated over
# ssh as the account the runner will run as (deploy/setup-windows.sh
# --setup). Idempotent. The toolchain is vm/win11/toolchain.ps1, run next to
# it with -CacheRoot set to the same root.
#
# Everything lives under $Root, readable by this account and SYSTEM only:
# bin\ (the installed binaries), state\ (the runner's state directory), src\
# (deploy\windows\bootstrap.ps1, copied there by setup-windows.sh), cargo\
# and rustup\.
param(
    [string]$Root = 'C:\zeughaus',
    # The runner's weida endpoint, an address on the WireGuard interface.
    [Parameter(Mandatory = $true)][string]$FeedAddr
)
$ErrorActionPreference = 'Stop'

# By SID for icacls: under ssh, USERDOMAIN can be the workgroup.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$account = $identity.Name
$sid = $identity.User.Value
# The root: owned by this account, no inherited entries, nobody else in it.
New-Item -ItemType Directory -Force -Path $Root | Out-Null
foreach ($dir in 'bin', 'state', 'src') {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root $dir) | Out-Null
}
& icacls $Root /setowner "*$sid" /T /C /Q | Out-Null
& icacls $Root /inheritance:r /grant:r "*${sid}:(OI)(CI)F" '*S-1-5-18:(OI)(CI)F' /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw "icacls $Root exited $LASTEXITCODE" }

# ssh commands from the deploy script and the CI's tooling are PowerShell, as
# in the Windows VM.
New-Item -Path HKLM:\SOFTWARE\OpenSSH -Force | Out-Null
New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -PropertyType String -Force `
    -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" | Out-Null

# Editors and the CI's relay reach the runner over WireGuard only.
$exe = Join-Path $Root 'bin\zeughaus-runner.exe'
Get-NetFirewallRule -DisplayName 'zeughaus-runner' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName 'zeughaus-runner' -Direction Inbound -Action Allow -Program $exe `
    -RemoteAddress 10.8.0.0/24 -Profile Any | Out-Null

# The runner as a task at boot, under this account without a stored password
# (S4U: no network credentials, which the runner does not use). Task
# Scheduler restarts a task that failed to launch, not one whose program
# exited, and the runner exits when the WireGuard address is not up yet: the
# wrapper loops. It keeps one previous log beyond 10 MB.
$state = Join-Path $Root 'state'
$log = Join-Path $state 'runner.log'
$wrapper = Join-Path $Root 'bin\runner.cmd'
Set-Content -Path $wrapper -Encoding ascii -Value @(
    '@echo off',
    ':loop',
    "for %%F in (`"$log`") do if %%~zF GTR 10485760 move /y `"$log`" `"$log.1`" >nul",
    "`"$exe`" --state-dir `"$state`" --feed-addr $FeedAddr >> `"$log`" 2>&1",
    'ping -n 6 127.0.0.1 >nul',
    'goto loop'
)
$action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\cmd.exe" -Argument "/d /c `"$wrapper`""
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId $account -LogonType S4U -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -StartWhenAvailable
Register-ScheduledTask -TaskName 'zeughaus-runner' -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force | Out-Null

Write-Output "set up $Root for $account, endpoint $FeedAddr"
