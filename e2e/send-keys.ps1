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
param(
    [Parameter(Mandatory = $true)][string]$Steps,
    [int]$FocusPid = 0
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

    public const uint INPUT_KEYBOARD = 1;
    public const uint KEYEVENTF_KEYUP = 0x0002;

    public static uint Send(ushort vk, bool up) {
        INPUT[] inputs = new INPUT[1];
        inputs[0].type = INPUT_KEYBOARD;
        inputs[0].ki.wVk = vk;
        inputs[0].ki.dwFlags = up ? KEYEVENTF_KEYUP : 0;
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

if ($FocusPid -gt 0) {
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
}
Write-Output "done"
