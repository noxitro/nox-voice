# 実キー相当の合成入力を送る E2E ヘルパー。
#
# WH_KEYBOARD_LL は SendInput 由来のキーに LLKHF_INJECTED を立てる。
# nox-voice の hook は「自プロセスの dwExtraInfo マーカー」だけを弾くので、
# ここから送るキー (dwExtraInfo = 0) は実キーと同じ経路を通る。
#
# 使い方:
#   pwsh -File send-keys.ps1 -Steps "down:0xA2,down:0x20,sleep:600,up:0x20,up:0xA2"
#   pwsh -File send-keys.ps1 -Steps "..." -FocusPid 1234
#
# -FocusPid を渡すと、送出前にそのプロセスのウィンドウを前景へ持ってくる。
# 合成キーはフックだけでなく**前景アプリにも届く**ので、これを省くと
# ユーザーが作業中のウィンドウへキーが流れ込む (E2E がユーザーの作業を壊す)。
#
# -Sink を渡すと、このスクリプト自身が空のウィンドウを出して前景にし、キーは
# そこへ流す。「nox-voice 以外のアプリが前景のときに押す」(実際の使われ方) を
# 作るためのもの。終わったらウィンドウは閉じる。
param(
    [Parameter(Mandatory = $true)][string]$Steps,
    [int]$FocusPid = 0,
    [switch]$Sink
)

$ErrorActionPreference = 'Stop'

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class NoxE2EInput {
    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT {
        public ushort wVk; public ushort wScan; public uint dwFlags;
        public uint time; public IntPtr dwExtraInfo;
    }
    [StructLayout(LayoutKind.Explicit, Size = 40)]
    public struct INPUT {
        [FieldOffset(0)] public uint type;
        [FieldOffset(8)] public KEYBDINPUT ki;
    }

    [DllImport("user32.dll", SetLastError = true)]
    public static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint uCode, uint uMapType);

    public const uint INPUT_KEYBOARD = 1;
    public const uint KEYEVENTF_EXTENDEDKEY = 0x0001;
    public const uint KEYEVENTF_KEYUP = 0x0002;
    public const uint MAPVK_VK_TO_VSC_EX = 4;

    // スキャンコードも付けて送る (物理キーと同じ形)。付けないと WebView2 の
    // ページには KeyboardEvent.code が空のキーとして届く (Chromium は code を
    // スキャンコードから決める)。拡張キー (右 Alt など) は拡張フラグも立てる。
    public static uint Send(ushort vk, bool up) {
        uint sc = MapVirtualKey(vk, MAPVK_VK_TO_VSC_EX);
        uint flags = up ? KEYEVENTF_KEYUP : 0;
        uint prefix = sc & 0xFF00;
        if (prefix == 0xE000 || prefix == 0xE100) flags |= KEYEVENTF_EXTENDEDKEY;
        INPUT[] inputs = new INPUT[1];
        inputs[0].type = INPUT_KEYBOARD;
        inputs[0].ki.wVk = vk;
        inputs[0].ki.wScan = (ushort)(sc & 0xFF);
        inputs[0].ki.dwFlags = flags;
        inputs[0].ki.dwExtraInfo = IntPtr.Zero;   // 自プロセス印は載せない = 実キー相当
        return SendInput(1, inputs, Marshal.SizeOf(typeof(INPUT)));
    }

    // 前景を「奪って」から返す。単発の SetForegroundWindow では足りないので
    // AttachThreadInput を挟む (フォアグラウンドロック対策)。
    public static bool Focus(IntPtr hWnd) {
        IntPtr fg = GetForegroundWindow();
        if (fg == hWnd) return true;
        uint dummy;
        uint fgThread = GetWindowThreadProcessId(fg, out dummy);
        uint me = GetCurrentThreadId();
        bool attached = fgThread != 0 && fgThread != me && AttachThreadInput(me, fgThread, true);
        try { SetForegroundWindow(hWnd); }
        finally { if (attached) AttachThreadInput(me, fgThread, false); }
        return GetForegroundWindow() == hWnd;
    }
}
'@

$sinkForm = $null
if ($Sink) {
    Add-Type -AssemblyName System.Windows.Forms
    $sinkForm = New-Object System.Windows.Forms.Form
    $sinkForm.Text = 'nox-voice E2E (キーの受け皿)'
    $sinkForm.ShowInTaskbar = $false
    $sinkForm.TopMost = $true
    $sinkForm.StartPosition = 'Manual'
    $sinkForm.SetBounds(0, 0, 320, 120)
    $sinkForm.Show()
    [System.Windows.Forms.Application]::DoEvents()
    $ok = [NoxE2EInput]::Focus($sinkForm.Handle)
    [System.Windows.Forms.Application]::DoEvents()
    Write-Output "focus: sink hwnd=0x$($sinkForm.Handle.ToString('X')) ok=$ok"
    Start-Sleep -Milliseconds 150
} elseif ($FocusPid -gt 0) {
    $proc = Get-Process -Id $FocusPid -ErrorAction SilentlyContinue
    if ($proc -and $proc.MainWindowHandle -ne [IntPtr]::Zero) {
        $ok = [NoxE2EInput]::Focus($proc.MainWindowHandle)
        Write-Output "focus: hwnd=0x$($proc.MainWindowHandle.ToString('X')) ok=$ok"
        Start-Sleep -Milliseconds 150
    } else {
        # 前景を取れないのは「失敗」ではなく前提の不成立。呼び出し側が判定する。
        Write-Output "focus: SKIP (メインウィンドウが見つからない pid=$FocusPid)"
    }
}

foreach ($step in $Steps.Split(',')) {
    $parts = $step.Trim().Split(':')
    $kind = $parts[0]
    $arg = $parts[1]
    switch ($kind) {
        'down' {
            $vk = [Convert]::ToUInt16($arg, 16)
            $n = [NoxE2EInput]::Send($vk, $false)
            if ($n -ne 1) { throw "SendInput(down 0x$arg) が 0 を返した (入力がブロックされている)" }
            Write-Output "down 0x$arg"
        }
        'up' {
            $vk = [Convert]::ToUInt16($arg, 16)
            $n = [NoxE2EInput]::Send($vk, $true)
            if ($n -ne 1) { throw "SendInput(up 0x$arg) が 0 を返した" }
            Write-Output "up   0x$arg"
        }
        'sleep' { Start-Sleep -Milliseconds ([int]$arg) }
        default { throw "未知のステップ: $step" }
    }
    Start-Sleep -Milliseconds 30
    # 受け皿のウィンドウは応答させておく (応答しないウィンドウは OS に前景から外されうる)。
    if ($sinkForm) { [System.Windows.Forms.Application]::DoEvents() }
}
if ($sinkForm) {
    $sinkForm.Close()
    $sinkForm.Dispose()
}
Write-Output "done"
