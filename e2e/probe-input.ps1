# 合成入力 (SendInput) が低レベルキーボードフック (WH_KEYBOARD_LL) まで届く環境かを確かめる。
#
# アプリは使わない。このスクリプト自身がフックを刺し、自分でキーを送り、フックに
# 届いた数を数える。ホットキー E2E (hotkey.mjs) のキー送出テストはこの経路が通る
# ことを前提にしているので、CI はこの結果でモードを決める。
#
# # フックは別スレッドに刺す (アプリと同じ形)
#
# アプリのフックは専用スレッドにあり、キーは別のところ (E2E の send-keys.ps1、
# アプリの番犬スレッド) から来る。LL フックの呼び出しはスレッドをまたぐと、
# フックのスレッドへメッセージを送って応答を待つ形になる (LowLevelHooksTimeout で
# 打ち切られ、打ち切られたフックは OS に外される)。
#
# 最初の版は、フックを刺したスレッド自身から SendInput していた。それがランナーで
# reachable と出たのに、アプリのフックは 1 打も受け取れなかった (2026-09-26、
# 番犬の生存確認も E2E のキーも)。同じスレッドなら、スレッドをまたぐ呼び出しを
# 通らずに済む可能性がある。なので判定には「別スレッド」の結果を使い、
# 同じスレッドの結果は参考として並べる。
#
# 出力: 1 行。先頭の語が判定 ("reachable" / "blocked")、以降は判断材料。
#   cross-thread=届いた数/送った数 (sent=送れた数 SendInput の所要時間)  … 判定に使う
#   cross-thread-app-like= … hMod = NULL・最高優先度で刺す (アプリと同じ設定)
#   same-thread=           … フックを刺したスレッドから送る (参考)
#   llhook-timeout=        … HKCU\Control Panel\Desktop\LowLevelHooksTimeout (無ければ unset)
# SendInput の所要時間は、フックの応答待ちで止まった分を含む。打ち切りまで
# 待たされていればその値 (ms) になり、フックが呼ばれずに素通りしていれば 0 に近い。
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
    [DllImport("user32.dll")]
    static extern int GetMessage(out MSG lpMsg, IntPtr hWnd, uint wMsgFilterMin, uint wMsgFilterMax);
    [DllImport("user32.dll")]
    static extern bool PostThreadMessage(uint idThread, uint msg, IntPtr wParam, IntPtr lParam);
    [DllImport("kernel32.dll")]
    static extern uint GetCurrentThreadId();
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentThread();
    [DllImport("kernel32.dll")]
    static extern bool SetThreadPriority(IntPtr hThread, int nPriority);
    [DllImport("user32.dll", SetLastError = true)]
    static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [DllImport("user32.dll", SetLastError = true)]
    static extern IntPtr OpenInputDesktop(uint dwFlags, bool fInherit, uint dwDesiredAccess);
    [DllImport("user32.dll")]
    static extern bool CloseDesktop(IntPtr hDesktop);
    [DllImport("kernel32.dll")]
    static extern uint WTSGetActiveConsoleSessionId();

    const int WH_KEYBOARD_LL = 13;
    const uint KEYEVENTF_KEYUP = 0x0002;
    const uint DESKTOP_READOBJECTS = 0x0001;
    const uint PM_NOREMOVE = 0x0000;
    const uint PM_REMOVE = 0x0001;
    const uint WM_QUIT = 0x0012;
    const int THREAD_PRIORITY_TIME_CRITICAL = 15;
    // 試験ごとに物理キーボードに無いキーを使い分ける (前の試験の取りこぼしと混ざらないように)。
    const ushort VK_F22 = 0x85;
    const ushort VK_F23 = 0x86;
    const ushort VK_F24 = 0x87;

    // いま数えている VK と、フックに届いた数。
    static int target;
    static int seen;
    // デリゲートを静的に持つ。ローカルに置くと GC に回収され、フックが無言で死ぬ。
    static readonly LowLevelKeyboardProc Proc = Hook;

    static IntPtr Hook(int nCode, IntPtr wParam, IntPtr lParam) {
        if (nCode >= 0) {
            var k = (KBDLLHOOKSTRUCT)Marshal.PtrToStructure(lParam, typeof(KBDLLHOOKSTRUCT));
            if (k.vkCode == (uint)Volatile.Read(ref target)) Interlocked.Increment(ref seen);
        }
        return CallNextHookEx(IntPtr.Zero, nCode, wParam, lParam);
    }

    // 押して離す (2 打)。送れた数と、SendInput が返るまでの時間。
    static string Send(ushort vk) {
        var inputs = new INPUT[2];
        inputs[0].type = 1; inputs[0].ki.wVk = vk;
        inputs[1].type = 1; inputs[1].ki.wVk = vk; inputs[1].ki.dwFlags = KEYEVENTF_KEYUP;
        var sw = Stopwatch.StartNew();
        uint sent = SendInput(2, inputs, Marshal.SizeOf(typeof(INPUT)));
        int err = Marshal.GetLastWin32Error();
        long ms = sw.ElapsedMilliseconds;
        return "sent=" + sent + (sent == 0 ? "(err " + err + ")" : "") + " " + ms + "ms";
    }

    static int WaitSeen(bool pump) {
        var sw = Stopwatch.StartNew();
        MSG msg;
        while (sw.ElapsedMilliseconds < 2000 && Volatile.Read(ref seen) < 2) {
            if (pump) PeekMessage(out msg, IntPtr.Zero, 0, 0, PM_REMOVE);
            Thread.Sleep(10);
        }
        return Volatile.Read(ref seen);
    }

    // 別スレッドに刺し、そのスレッドで GetMessage を回す。キーはこのスレッドから送る。
    // appLike: hMod = NULL・THREAD_PRIORITY_TIME_CRITICAL (アプリの hotkey.rs と同じ)。
    static string CrossThread(ushort vk, bool appLike) {
        Volatile.Write(ref target, vk);
        Volatile.Write(ref seen, 0);
        uint tid = 0;
        IntPtr hook = IntPtr.Zero;
        int err = 0;
        var ready = new ManualResetEvent(false);
        var thread = new Thread(() => {
            tid = GetCurrentThreadId();
            if (appLike) SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            hook = SetWindowsHookEx(WH_KEYBOARD_LL, Proc, appLike ? IntPtr.Zero : GetModuleHandle(null), 0);
            err = Marshal.GetLastWin32Error();
            MSG msg;
            // メッセージキューを作ってから知らせる (WM_QUIT を投げて終わらせるため)。
            PeekMessage(out msg, IntPtr.Zero, 0, 0, PM_NOREMOVE);
            ready.Set();
            if (hook == IntPtr.Zero) return;
            while (GetMessage(out msg, IntPtr.Zero, 0, 0) > 0) { }
            UnhookWindowsHookEx(hook);
        });
        thread.IsBackground = true;
        thread.Start();
        ready.WaitOne();
        if (hook == IntPtr.Zero) return "hook-install-failed(err " + err + ")";
        // フックのスレッドが GetMessage で待つところまで進ませる。
        Thread.Sleep(100);
        string sent = Send(vk);
        int got = WaitSeen(false);
        PostThreadMessage(tid, WM_QUIT, IntPtr.Zero, IntPtr.Zero);
        thread.Join(2000);
        return got + "/2 (" + sent + ")";
    }

    // フックを刺したスレッド自身から送る (最初の版の測り方。参考)。
    static string SameThread(ushort vk) {
        Volatile.Write(ref target, vk);
        Volatile.Write(ref seen, 0);
        IntPtr hook = SetWindowsHookEx(WH_KEYBOARD_LL, Proc, GetModuleHandle(null), 0);
        if (hook == IntPtr.Zero) return "hook-install-failed(err " + Marshal.GetLastWin32Error() + ")";
        try {
            string sent = Send(vk);
            return WaitSeen(true) + "/2 (" + sent + ")";
        } finally {
            UnhookWindowsHookEx(hook);
        }
    }

    public static string Run() {
        string context = "session=" + Process.GetCurrentProcess().SessionId
            + " console-session=" + WTSGetActiveConsoleSessionId();
        IntPtr desk = OpenInputDesktop(0, false, DESKTOP_READOBJECTS);
        context += desk == IntPtr.Zero
            ? " input-desktop=inaccessible(err " + Marshal.GetLastWin32Error() + ")"
            : " input-desktop=ok";
        if (desk != IntPtr.Zero) CloseDesktop(desk);

        // 同じスレッドを先に測る。フックの関数がここで直接呼ばれて JIT 済みになる。
        // 別スレッドを先にすると、初回呼び出しの JIT の分だけ応答が遅れ、
        // LowLevelHooksTimeout に掛かって「届かない」と誤判定しうる (アプリは Rust なので
        // この遅れは無い)。
        string same = SameThread(VK_F24);
        string cross = CrossThread(VK_F23, false);
        string appLike = CrossThread(VK_F22, true);
        return (cross.StartsWith("2/2") ? "reachable" : "blocked")
            + " cross-thread=" + cross
            + " cross-thread-app-like=" + appLike
            + " same-thread=" + same
            + " " + context;
    }
}
'@

$timeout = (Get-ItemProperty -Path 'HKCU:\Control Panel\Desktop' -Name LowLevelHooksTimeout -ErrorAction SilentlyContinue).LowLevelHooksTimeout
"$([NoxInputProbe]::Run()) llhook-timeout=$(if ($null -eq $timeout) { 'unset' } else { $timeout })"
