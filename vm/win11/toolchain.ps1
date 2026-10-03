# Installs the build toolchain into the golden image. Run over ssh by
# `vm.sh toolchain` with the base image booted (PERSIST=1). Rust itself is
# not installed here: CARGO_HOME and RUSTUP_HOME live on the cache disk
# (W:), which does not exist in the golden image; prepare.ps1 runs
# rustup-init on first use. Log: C:\zeughaus\toolchain.log
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Start-Transcript -Path C:\zeughaus\toolchain.log -Force

$dl = 'C:\zeughaus\dl'
New-Item -ItemType Directory -Path $dl -Force | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13

function Get-Download([string]$Url, [string]$Name) {
    $path = Join-Path $dl $Name
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $path
    return $path
}

# URL of the release asset of a GitHub repository whose name matches $Pattern.
function Get-ReleaseAsset([string]$Repo, [string]$Pattern) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'zeughaus-ci' }
    $asset = $release.assets | Where-Object { $_.name -match $Pattern } | Select-Object -First 1
    if (-not $asset) { throw "no asset matching $Pattern in $Repo" }
    return $asset
}

function Invoke-Installer([string]$File, [string[]]$Arguments, [int[]]$Ok = @(0)) {
    $p = Start-Process -FilePath $File -ArgumentList $Arguments -Wait -PassThru
    if ($Ok -notcontains $p.ExitCode) { throw "$File exited $($p.ExitCode)" }
}

# Visual Studio 2022 Build Tools: MSVC, Windows SDK. 3010 is "reboot required".
$vs = Get-Download 'https://aka.ms/vs/17/release/vs_BuildTools.exe' 'vs_BuildTools.exe'
Invoke-Installer $vs @('--quiet', '--wait', '--norestart', '--nocache',
    '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended') @(0, 3010)

# Git for Windows.
$asset = Get-ReleaseAsset 'git-for-windows/git' '^Git-.*-64-bit\.exe$'
$git = Get-Download $asset.browser_download_url $asset.name
Invoke-Installer $git @('/VERYSILENT', '/NORESTART', '/SP-')
$gitExe = 'C:\Program Files\Git\cmd\git.exe'
& $gitExe config --system core.autocrlf false
if ($LASTEXITCODE -ne 0) { throw 'git config core.autocrlf failed' }
& $gitExe config --system core.longpaths true
if ($LASTEXITCODE -ne 0) { throw 'git config core.longpaths failed' }

# CMake.
$asset = Get-ReleaseAsset 'Kitware/CMake' '^cmake-.*-windows-x86_64\.msi$'
$cmake = Get-Download $asset.browser_download_url $asset.name
Invoke-Installer 'msiexec.exe' @('/i', "`"$cmake`"", '/qn', 'ADD_CMAKE_TO_PATH=System')

# LLVM: libclang for bindgen.
$asset = Get-ReleaseAsset 'llvm/llvm-project' '^LLVM-.*-win64\.exe$'
$llvm = Get-Download $asset.browser_download_url $asset.name
Invoke-Installer $llvm @('/S')
[Environment]::SetEnvironmentVariable('LIBCLANG_PATH', 'C:\Program Files\LLVM\bin', 'Machine')

# rustup-init, kept for prepare.ps1.
Invoke-WebRequest -UseBasicParsing -OutFile C:\zeughaus\rustup-init.exe `
    -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'

# Rust state lives on the cache disk, so it survives the throwaway overlay.
[Environment]::SetEnvironmentVariable('CARGO_HOME', 'W:\cargo', 'Machine')
[Environment]::SetEnvironmentVariable('RUSTUP_HOME', 'W:\rustup', 'Machine')
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';') -notcontains 'W:\cargo\bin') {
    [Environment]::SetEnvironmentVariable('Path', "W:\cargo\bin;$machinePath", 'Machine')
}

# Long paths for cargo target directories.
reg add HKLM\SYSTEM\CurrentControlSet\Control\FileSystem /v LongPathsEnabled /t REG_DWORD /d 1 /f
if ($LASTEXITCODE -ne 0) { throw 'reg add LongPathsEnabled failed' }

# Defender and the indexer only slow builds down.
Add-MpPreference -ExclusionPath 'W:\'
foreach ($name in 'cargo.exe', 'rustc.exe', 'link.exe', 'cl.exe') {
    Add-MpPreference -ExclusionProcess $name
}
Set-Service WSearch -StartupType Disabled

# Versions. The machine Path changed above, so look the tools up directly.
& $gitExe --version
& 'C:\Program Files\CMake\bin\cmake.exe' --version | Select-Object -First 1
& 'C:\Program Files\LLVM\bin\clang.exe' --version | Select-Object -First 1
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
& $vswhere -latest -products '*' -property installationVersion
Write-Output 'toolchain complete'

Stop-Transcript
