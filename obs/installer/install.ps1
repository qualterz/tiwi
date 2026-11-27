& {
  $ErrorActionPreference = "Stop"
  $ProgressPreference = "SilentlyContinue"
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Unsupported OS architecture: 64-bit Windows is required."
  }

  if (Get-Process -Name "obs64" -ErrorAction SilentlyContinue) {
    throw "OBS Studio is currently running. Please close OBS Studio and re-run this installer."
  }

  $repo = "qualterz/tiwi"
  $tag = if ($null -ne $env:TIWI_VERSION) { $env:TIWI_VERSION.Trim() } else { "" }
  if ([string]::IsNullOrEmpty($tag)) {
    $req = [Net.HttpWebRequest]::Create("https://github.com/$repo/releases/latest")
    $req.Method = "HEAD"
    $req.AllowAutoRedirect = $false
    $req.UserAgent = "tiwi-installer"
    $resp = $req.GetResponse()
    try {
      $location = $resp.Headers["Location"]
    } finally {
      $resp.Close()
    }
    if ($location -match "/releases/tag/([^/?#]+)") {
      $tag = $Matches[1]
    }
  }
  if ([string]::IsNullOrEmpty($tag)) {
    throw "Could not determine latest Tiwi release."
  }
  if (-not $tag.StartsWith("v")) {
    $tag = "v$tag"
  }

  $version = $tag -replace "^v", ""
  $asset = "tiwi-obs-$version-windows-x64.zip"
  $baseUrl = "https://github.com/$repo/releases/download/$tag"

  function Install-TiwiPlugin {
    param([string]$Src, [string]$Dst)
    $parent = Split-Path -Parent $Dst
    New-Item -ItemType Directory -Path $parent -Force | Out-Null

    $staging = "$Dst.staging"
    $backup = "$Dst.backup"
    if (Test-Path $staging) { Remove-Item -Path $staging -Recurse -Force }
    if (Test-Path $backup) { Remove-Item -Path $backup -Recurse -Force }

    Copy-Item -Path $Src -Destination $staging -Recurse -Force
    $backedUp = $false
    try {
      if (Test-Path $Dst) {
        Move-Item -Path $Dst -Destination $backup -Force
        $backedUp = $true
      }
      Move-Item -Path $staging -Destination $Dst -Force
    } catch {
      if ($backedUp -and -not (Test-Path $Dst) -and (Test-Path $backup)) {
        Move-Item -Path $backup -Destination $Dst -Force -ErrorAction SilentlyContinue
      }
      if (Test-Path $staging) {
        Remove-Item -Path $staging -Recurse -Force -ErrorAction SilentlyContinue
      }
      throw
    }

    if ($backedUp -and (Test-Path $backup)) {
      Remove-Item -Path $backup -Recurse -Force -ErrorAction SilentlyContinue
    }
  }

  $tmpDir = Join-Path ([IO.Path]::GetTempPath()) ("tiwi-install-" + [Guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

  try {
    $zipPath = Join-Path $tmpDir $asset
    $sumsPath = Join-Path $tmpDir "SHA256SUMS.txt"

    Write-Host "Downloading $asset ($tag)..."
    Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $zipPath -UseBasicParsing
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS.txt" -OutFile $sumsPath -UseBasicParsing

    $expectedHash = $null
    $pattern = "^\s*([0-9a-fA-F]{64})\s+\*?$([Regex]::Escape($asset))\s*$"
    foreach ($line in (Get-Content -Path $sumsPath)) {
      if ($line -match $pattern) {
        $expectedHash = $Matches[1].ToLowerInvariant()
        break
      }
    }
    if (-not $expectedHash) {
      throw "Could not find checksum for $asset in SHA256SUMS.txt."
    }

    $actualHash = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
      throw "SHA256 verification failed for $asset (expected $expectedHash, got $actualHash)."
    }

    $extractDir = Join-Path $tmpDir "extract"
    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

    $sourceDir = Join-Path $extractDir "tiwi-obs"
    if (-not (Test-Path (Join-Path $sourceDir "tiwi-obs.dll"))) {
      throw "Extracted archive does not contain tiwi-obs\tiwi-obs.dll."
    }

    $dest = Join-Path $env:ProgramData "obs-studio\plugins\tiwi-obs"
    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
      [Security.Principal.WindowsBuiltInRole]::Administrator
    )

    try {
      Install-TiwiPlugin -Src $sourceDir -Dst $dest
    } catch {
      $ex = $_.Exception
      $isAccessDenied = ($ex -is [System.UnauthorizedAccessException]) -or (
        ($ex -is [System.IO.IOException]) -and ($ex.HResult -eq -2147024891)
      )
      if ($isAdmin -or -not $isAccessDenied) {
        throw
      }

      Write-Host "Administrator privileges required to install to $dest. Requesting elevation..."
      $errFile = Join-Path $tmpDir "elevated-error.txt"
      $qSrc = $sourceDir.Replace("'", "''")
      $qDst = $dest.Replace("'", "''")
      $qErr = $errFile.Replace("'", "''")
      $elevatedCmd = @"
`$ErrorActionPreference = 'Stop'
function Install-TiwiPlugin {
${function:Install-TiwiPlugin}
}
try {
  Install-TiwiPlugin -Src '$qSrc' -Dst '$qDst'
} catch {
  `$_.Exception.Message | Set-Content -Path '$qErr' -Encoding UTF8
  exit 1
}
"@
      $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($elevatedCmd))
      $proc = Start-Process -FilePath "powershell.exe" -Verb RunAs -Wait -PassThru -ArgumentList @(
        "-NoProfile", "-ExecutionPolicy", "Bypass", "-EncodedCommand", $encoded
      )
      if ($proc.ExitCode -ne 0) {
        $detail = if (Test-Path $errFile) { (Get-Content -Path $errFile -Raw).Trim() } else { "" }
        if ($detail) {
          throw "Elevated installation failed: $detail"
        }
        throw "Elevated installation failed with exit code $($proc.ExitCode)."
      }
    }

    if (-not (Test-Path (Join-Path $dest "tiwi-obs.dll"))) {
      throw "Installation verification failed: $dest\tiwi-obs.dll not found."
    }

    Write-Host "Installed Tiwi for OBS Studio ($tag) to $dest. Open OBS to start streaming."
  } finally {
    if (Test-Path $tmpDir) {
      Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
  }
}
