# 合成入力 (SendInput) が低レベルキーボードフック (WH_KEYBOARD_LL) まで届く環境かを確かめる。
#
# アプリは使わない。このスクリプト自身がフックを刺し、自分で F24 を送り、
# フックに届いた数を数える。ホットキー E2E (hotkey.mjs) のキー送出テストは
# この経路が通ることを前提にしているので、CI はこの結果でモードを決める。
#
# GitHub Actions の Windows ランナーでは、アプリの番犬 (生存確認のキー) が自分の
# 送ったキーを観測できないことを実際に確かめている (2026-09-26 のログ)。
# ランナーの側が変われば、ここが reachable になり、E2E は自動で全項目を測る。
#
# 出力: 1 行。先頭の語が判定 ("reachable" / "blocked")、以降は判断材料。
# 終了コードは常に 0 (判定は出力で渡す)。
#
# 使い方: pwsh -NoProfile -File e2e/probe-input.ps1
$ErrorActionPreference = 'Stop'

Add-Type @'
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Threading;

public static class NoxInputProbe {
    delegate IntPtr LowLevelKeyboardProc(int nCode, IntPtr wParam, IntPtr lParam);

    [StructLayout(LayoutKind.Sequential)]
    struct KBDLLHOOKSTRUCT { public uint vkCode; public uint scanCode; public uint flags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)]
    struct MSG { public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public int ptX; public int ptY; }
    [StructLayout(LayoutKind.Sequential)]
    struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit, Size = 40)]
    struct INPUT { [FieldOffset(0)] public uint type; [FieldOffset(8)] public KEYBDINPUT ki; }

    [DllImport("user32.dll", SetLastError = true)]
    static extern IntPtr SetWindowsHookEx(int idHook, LowLevelKeyboardProc lpfn, IntPtr hMod, uint dwThreadId);
    [DllImport("user32.dll")]
    static extern bool UnhookWindowsHookEx(IntPtr hhk);
    [DllImport("user32.dll")]
    static extern IntPtr CallNextHookEx(IntPtr hhk, int nCode, IntPtr wParam, IntPtr lParam);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr GetModuleHandle(string lpModuleName);
    [DllImport("user32.dll")]
    static extern bool PeekMessage(out MSG lpMsg, IntPtr hWnd, uint wMsgFilterMin, uint wMsgFilterMax, uint wRemoveMsg);
    [DllImport("user32.dll", SetLastError = true)]
    static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [DllImport("user32.dll", SetLastError = true)]
    static extern IntPtr OpenInputDesktop(uint dwFlags, bool fInherit, uint dwDesiredAccess);
    [DllImport("user32.dll")]
    static extern bool CloseDesktop(IntPtr hDesktop);
    [DllImport("kernel32.dll")]
    static extern uint WTSGetActiveConsoleSessionId();

    const int WH_KEYBOARD_LL = 13;
    const ushort VK_F24 = 0x87;
    const uint KEYEVENTF_KEYUP = 0x0002;
    const uint DESKTOP_READOBJECTS = 0x0001;
    const uint PM_REMOVE = 0x0001;

    static int seen;
    // デリゲートを静的に持つ。ローカルに置くと GC に回収され、フックが無言で死ぬ。
    static readonly LowLevelKeyboardProc Proc = Hook;

    static IntPtr Hook(int nCode, IntPtr wParam, IntPtr lParam) {
        if (nCode >= 0) {
            var k = (KBDLLHOOKSTRUCT)Marshal.PtrToStructure(lParam, typeof(KBDLLHOOKSTRUCT));
            if (k.vkCode == VK_F24) seen++;
        }
        return CallNextHookEx(IntPtr.Zero, nCode, wParam, lParam);
    }

    public static string Run() {
        string context = "session=" + Process.GetCurrentProcess().SessionId
            + " console-session=" + WTSGetActiveConsoleSessionId();
        IntPtr desk = OpenInputDesktop(0, false, DESKTOP_READOBJECTS);
        context += desk == IntPtr.Zero
            ? " input-desktop=inaccessible(err " + Marshal.GetLastWin32Error() + ")"
            : " input-desktop=ok";
        if (desk != IntPtr.Zero) CloseDesktop(desk);

        IntPtr hook = SetWindowsHookEx(WH_KEYBOARD_LL, Proc, GetModuleHandle(null), 0);
        if (hook == IntPtr.Zero) {
            return "blocked hook-install-failed(err " + Marshal.GetLastWin32Error() + ") " + context;
        }
        try {
            var inputs = new INPUT[2];
            inputs[0].type = 1; inputs[0].ki.wVk = VK_F24;
            inputs[1].type = 1; inputs[1].ki.wVk = VK_F24; inputs[1].ki.dwFlags = KEYEVENTF_KEYUP;
            uint sent = SendInput(2, inputs, Marshal.SizeOf(typeof(INPUT)));
            int err = Marshal.GetLastWin32Error();
            // フックはこのスレッドのメッセージループで呼ばれるので、回しながら待つ。
            var sw = Stopwatch.StartNew();
            MSG msg;
            while (sw.ElapsedMilliseconds < 2000 && seen < 2) {
                PeekMessage(out msg, IntPtr.Zero, 0, 0, PM_REMOVE);
                Thread.Sleep(10);
            }
            return (seen >= 2 ? "reachable" : "blocked")
                + " sent=" + sent + (sent == 0 ? "(err " + err + ")" : "")
                + " seen=" + seen + " " + context;
        } finally {
            UnhookWindowsHookEx(hook);
        }
    }
}
'@

[NoxInputProbe]::Run()
