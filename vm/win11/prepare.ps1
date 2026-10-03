# Run by the zeughaus-ci runner after every boot of the VM (copied into the
# guest first, so it can change without rebuilding the golden image).
# Brings the persistent cache disk up as W:, clears the previous run's
# leftovers and makes sure Rust is installed on it.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

Get-Disk | Where-Object IsOffline | Set-Disk -IsOffline $false

$volume = Get-Volume | Where-Object FileSystemLabel -eq 'CICACHE' | Select-Object -First 1
if (-not $volume) {
    $raw = Get-Disk | Where-Object PartitionStyle -eq 'RAW' | Select-Object -First 1
    if (-not $raw) { throw 'no cache disk' }
    Initialize-Disk -Number $raw.Number -PartitionStyle GPT
    New-Partition -DiskNumber $raw.Number -UseMaximumSize -DriveLetter W | Out-Null
    Format-Volume -DriveLetter W -FileSystem NTFS -NewFileSystemLabel CICACHE -Confirm:$false | Out-Null
}
else {
    $letter = $volume.DriveLetter
    if ($letter -ne [char]'W') {
        $partition = Get-Partition | Where-Object { $_.AccessPaths -contains $volume.Path } | Select-Object -First 1
        if (-not $partition) { throw 'cache volume has no partition' }
        Set-Partition -DiskNumber $partition.DiskNumber -PartitionNumber $partition.PartitionNumber -NewDriveLetter W
    }
}

Remove-Item -Path 'W:\ci\runs\*' -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path 'W:\ci\runs' -Force | Out-Null
New-Item -ItemType Directory -Path 'W:\work' -Force | Out-Null

# Build outputs are the first thing to go when the disk fills up.
$drive = Get-Volume -DriveLetter W
if ($drive.SizeRemaining -lt 0.2 * $drive.Size) {
    Get-ChildItem -Path 'W:\work\*\*\target' -Directory -ErrorAction SilentlyContinue |
        Remove-Item -Recurse -Force
}

# With discard=unmap this hands the freed blocks back to cache.qcow2.
# Housekeeping only: a volume Windows refuses to retrim (StorageWMI 40004
# on a freshly formatted one) must not keep the machine from coming up.
try {
    Optimize-Volume -DriveLetter W -ReTrim
}
catch {
    Write-Warning "retrim of W: failed: $_"
}

if (-not (Test-Path 'W:\cargo\bin\rustup.exe')) {
    $env:CARGO_HOME = 'W:\cargo'
    $env:RUSTUP_HOME = 'W:\rustup'
    & C:\zeughaus\rustup-init.exe -y --no-modify-path --default-toolchain stable --profile minimal
    if ($LASTEXITCODE -ne 0) { throw "rustup-init exited $LASTEXITCODE" }
}

Write-Output 'prepared'
