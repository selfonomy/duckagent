param(
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\duckagent\bin",
    [switch]$NoModifyPath,
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "selfonomy/duckagent"
$App = "duck"

if ($Help) {
    @"
DuckAgent installer

Usage:
  irm https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.ps1 | iex
"@ | Write-Host
    exit 0
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\duckagent\bin"
}

function Write-Step {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Normalize-Version {
    param([string]$RawVersion)

    if ([string]::IsNullOrWhiteSpace($RawVersion) -or $RawVersion -eq "latest") {
        return "latest"
    }
    if ($RawVersion.StartsWith("v")) {
        return $RawVersion.Substring(1)
    }
    return $RawVersion
}

function Get-Target {
    $isWindows = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    )
    if (-not $isWindows) {
        throw "scripts/install.ps1 only supports Windows. Use scripts/install.sh on Linux or macOS."
    }

    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    switch ($arch) {
        "X64" { return "x86_64-pc-windows-msvc" }
        "Arm64" { return "aarch64-pc-windows-msvc" }
        default { throw "Unsupported Windows CPU architecture: $arch" }
    }
}

function Get-DownloadBaseUrl {
    $normalized = Normalize-Version -RawVersion $Version
    if ($normalized -eq "latest") {
        return "https://github.com/$Repo/releases/latest/download"
    }
    return "https://github.com/$Repo/releases/download/v$normalized"
}

function Invoke-Download {
    param(
        [string]$Url,
        [string]$OutputPath
    )

    Invoke-WebRequest `
        -Uri $Url `
        -OutFile $OutputPath `
        -UseBasicParsing `
        -Headers @{ "User-Agent" = "duck-installer" }
}

function Read-ExpectedSha256 {
    param([string]$ChecksumPath)

    $text = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
    $match = [regex]::Match($text, "^([0-9a-fA-F]{64})\b")
    if (-not $match.Success) {
        throw "Invalid checksum file: $ChecksumPath"
    }
    return $match.Groups[1].Value.ToLowerInvariant()
}

function Test-ArchiveDigest {
    param(
        [string]$ArchivePath,
        [string]$ExpectedDigest
    )

    $actual = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $ExpectedDigest) {
        throw "Checksum mismatch for $ArchivePath. Expected $ExpectedDigest but got $actual."
    }
}

function Path-Contains {
    param(
        [string]$PathValue,
        [string]$Entry
    )

    if ([string]::IsNullOrWhiteSpace($PathValue)) {
        return $false
    }

    $needle = $Entry.TrimEnd("\")
    foreach ($segment in $PathValue.Split(";", [System.StringSplitOptions]::RemoveEmptyEntries)) {
        if ($segment.TrimEnd("\") -ieq $needle) {
            return $true
        }
    }

    return $false
}

function Add-ToUserPathIfNeeded {
    if (Path-Contains -PathValue $env:Path -Entry $InstallDir) {
        return
    }

    if ($NoModifyPath) {
        Write-Host "Add $InstallDir to PATH before running duck."
        return
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not (Path-Contains -PathValue $userPath -Entry $InstallDir)) {
        if ([string]::IsNullOrWhiteSpace($userPath)) {
            $newUserPath = $InstallDir
        } else {
            $newUserPath = "$userPath;$InstallDir"
        }
        [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
        Write-Host "Added $InstallDir to your user PATH."
    }

    $env:Path = "$env:Path;$InstallDir"
}

$target = Get-Target
$asset = "$App-$target.zip"
$baseUrl = Get-DownloadBaseUrl

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("duck-install-" + [guid]::NewGuid().ToString("N"))
$extractDir = Join-Path $tempRoot "extract"
$archive = Join-Path $tempRoot $asset
$checksum = Join-Path $tempRoot "$asset.sha256"

try {
    New-Item -ItemType Directory -Force -Path $extractDir | Out-Null

    Write-Step "Downloading $asset"
    Invoke-Download -Url "$baseUrl/$asset" -OutputPath $archive
    Invoke-Download -Url "$baseUrl/$asset.sha256" -OutputPath $checksum

    $expected = Read-ExpectedSha256 -ChecksumPath $checksum
    Test-ArchiveDigest -ArchivePath $archive -ExpectedDigest $expected

    Expand-Archive -LiteralPath $archive -DestinationPath $extractDir -Force
    $binary = Get-ChildItem -LiteralPath $extractDir -Recurse -File -Filter "$App.exe" |
        Select-Object -First 1
    if ($null -eq $binary) {
        throw "Archive did not contain $App.exe."
    }

    Write-Step "Installing duck to $InstallDir"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $destination = Join-Path $InstallDir "$App.exe"
    $temporaryDestination = Join-Path $InstallDir ".$App.tmp.$PID.exe"
    Copy-Item -LiteralPath $binary.FullName -Destination $temporaryDestination -Force
    if (Test-Path -LiteralPath $destination -PathType Leaf) {
        Remove-Item -LiteralPath $destination -Force
    }
    Move-Item -LiteralPath $temporaryDestination -Destination $destination -Force

    try {
        $versionOutput = & $destination --version
        Write-Host "Installed $versionOutput at $destination."
    } catch {
        Write-Host "Installed duck at $destination."
    }

    Add-ToUserPathIfNeeded
} finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
