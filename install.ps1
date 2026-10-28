# Xbox Connect installer — per-user, no admin.
#   irm https://raw.githubusercontent.com/ic3man5/xbox-connect/main/install.ps1 | iex
# Pin a version:  $env:XBOX_CONNECT_VERSION = 'v0.1.0'; irm ... | iex
#
# Everything lives in a function invoked on the last line, so a truncated download runs nothing.

function Install-XboxConnect {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest is very slow with progress on PS 5.1
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Warning 'Running as administrator is not needed; the app would also start elevated. Consider a normal PowerShell window.'
    }

    $repo = 'ic3man5/xbox-connect'
    $asset = 'xbox-connect.exe'
    $base = if ($env:XBOX_CONNECT_VERSION) {
        "https://github.com/$repo/releases/download/$($env:XBOX_CONNECT_VERSION)"
    } else {
        "https://github.com/$repo/releases/latest/download"
    }

    $dir = Join-Path $env:LOCALAPPDATA 'Programs\XboxConnect'
    $exe = Join-Path $dir $asset
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("xbox-connect-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $dir, $tmp | Out-Null

    try {
        Write-Host "Downloading $asset from $base ..."
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile "$tmp\$asset"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile "$tmp\$asset.sha256"

        # The checksum guards against a corrupted or truncated download. It is published in the
        # same release, so it cannot protect against a compromised repository.
        $expected = ((Get-Content "$tmp\$asset.sha256" -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 "$tmp\$asset").Hash.ToLowerInvariant()
        if ($expected -ne $actual) { throw "Checksum mismatch (expected $expected, got $actual). Aborting." }

        # Stop only our own copy, then wait for the file lock to clear.
        Get-Process -Name 'xbox-connect' -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and ($_.Path -ieq $exe) } |
            ForEach-Object { $_ | Stop-Process -Force; $_.WaitForExit(5000) | Out-Null }

        $moved = $false
        for ($i = 0; $i -lt 20 -and -not $moved; $i++) {
            try { Move-Item -Force "$tmp\$asset" $exe; $moved = $true } catch { Start-Sleep -Milliseconds 250 }
        }
        if (-not $moved) { throw "Could not replace $exe (is it still running or locked?)" }

        # Local uninstaller, so Add/Remove Programs never runs code fetched at uninstall time.
        Set-Content -Path (Join-Path $dir 'uninstall.ps1') -Encoding UTF8 -Value @'
$ErrorActionPreference = 'Continue'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\XboxConnect'
$exe = Join-Path $dir 'xbox-connect.exe'
Get-Process -Name 'xbox-connect' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and ($_.Path -ieq $exe) } |
    ForEach-Object { $_ | Stop-Process -Force; $_.WaitForExit(5000) | Out-Null }
Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'XboxConnect' -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force 'HKCU:\Software\XboxConnect' -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\XboxConnect' -ErrorAction SilentlyContinue
Remove-Item -Force (Join-Path ([Environment]::GetFolderPath('Programs')) 'Xbox Connect.lnk') -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'XboxConnect') -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
Write-Host 'Xbox Connect has been removed.'
'@

        # Start Menu shortcut.
        $lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'Xbox Connect.lnk'
        $shell = New-Object -ComObject WScript.Shell
        $sc = $shell.CreateShortcut($lnk)
        $sc.TargetPath = $exe
        $sc.WorkingDirectory = $dir
        $sc.Description = 'Keeps Xbox controllers connected over Bluetooth'
        $sc.Save()

        # Start with Windows (same value format the app writes itself).
        $run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
        Set-ItemProperty -Path $run -Name 'XboxConnect' -Value "`"$exe`" --minimized"

        # Add/Remove Programs entry pointing at the local uninstaller.
        $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
        $un = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\XboxConnect'
        New-Item -Force -Path $un | Out-Null
        Set-ItemProperty -Path $un -Name DisplayName -Value 'Xbox Connect'
        Set-ItemProperty -Path $un -Name Publisher -Value 'ic3man5'
        Set-ItemProperty -Path $un -Name DisplayIcon -Value $exe
        Set-ItemProperty -Path $un -Name InstallLocation -Value $dir
        Set-ItemProperty -Path $un -Name UninstallString -Value "`"$ps`" -NoProfile -ExecutionPolicy Bypass -File `"$dir\uninstall.ps1`""
        Set-ItemProperty -Path $un -Name NoModify -Value 1 -Type DWord
        Set-ItemProperty -Path $un -Name NoRepair -Value 1 -Type DWord
        if ($env:XBOX_CONNECT_VERSION) { Set-ItemProperty -Path $un -Name DisplayVersion -Value $env:XBOX_CONNECT_VERSION.TrimStart('v') }

        Start-Process $exe
        Write-Host "Installed to $exe and started. It lives in the system tray." -ForegroundColor Green
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

Install-XboxConnect
