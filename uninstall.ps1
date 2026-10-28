# Xbox Connect uninstaller — removes everything install.ps1 and the app created.
#   irm https://raw.githubusercontent.com/ic3man5/xbox-connect/main/uninstall.ps1 | iex

function Uninstall-XboxConnect {
    $ErrorActionPreference = 'Stop'
    $dir = Join-Path $env:LOCALAPPDATA 'Programs\XboxConnect'
    $exe = Join-Path $dir 'xbox-connect.exe'

    Get-Process -Name 'xbox-connect' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path -ieq $exe) } |
        ForEach-Object { $_ | Stop-Process -Force; $_.WaitForExit(5000) | Out-Null }

    Remove-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'XboxConnect' -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force 'HKCU:\Software\XboxConnect' -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\XboxConnect' -ErrorAction SilentlyContinue
    Remove-Item -Force (Join-Path ([Environment]::GetFolderPath('Programs')) 'Xbox Connect.lnk') -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA 'XboxConnect') -ErrorAction SilentlyContinue  # logs

    Write-Host 'Xbox Connect has been removed.' -ForegroundColor Green
}

Uninstall-XboxConnect
