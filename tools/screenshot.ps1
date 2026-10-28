# Captures the "Xbox Connect" window to a PNG (used for UI review) without bringing it to the
# front or touching other windows: PrintWindow renders the window itself, even when covered.
param([string]$Out = "$PSScriptRoot\..\target\screenshot.png", [int]$Width = 0, [int]$Height = 0)

Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Win {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
    // The app's main window has winit's class name; skip helper windows of the same process.
    public static IntPtr FindMain(uint pid) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            var c = new System.Text.StringBuilder(64); GetClassName(h, c, 64);
            if (p == pid && c.ToString() == "Window Class") { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
'@
[Win]::SetProcessDPIAware() | Out-Null
$h = [IntPtr]::Zero
foreach ($p in Get-Process xbox-connect -ErrorAction SilentlyContinue) { $h = [Win]::FindMain([uint32]$p.Id); if ($h -ne [IntPtr]::Zero) { break } }
if ($h -eq [IntPtr]::Zero) { throw 'Window not found' }
if ($Width -gt 0 -and $Height -gt 0) {
    # Resize only: 0x0002 SWP_NOMOVE | 0x0004 SWP_NOZORDER | 0x0010 SWP_NOACTIVATE.
    [Win]::SetWindowPos($h, [IntPtr]::Zero, 0, 0, $Width, $Height, 0x0016) | Out-Null
    Start-Sleep -Milliseconds 900
}
Start-Sleep -Milliseconds 300
$r = New-Object Win+RECT
[Win]::GetWindowRect($h, [ref]$r) | Out-Null
$w = $r.R - $r.L; $hgt = $r.B - $r.T
$bmp = New-Object System.Drawing.Bitmap $w, $hgt
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
[Win]::PrintWindow($h, $hdc, 2) | Out-Null   # 2 = PW_RENDERFULLCONTENT (works for GPU-rendered windows)
$g.ReleaseHdc($hdc)
$bmp.Save((Resolve-Path -LiteralPath (Split-Path $Out)).Path + '\' + (Split-Path $Out -Leaf), [System.Drawing.Imaging.ImageFormat]::Png)
"Saved $Out ($w x $hgt)"
