// CDP (WebView2 のデバッグポート) に繋がらなかったとき、「どこで止まったか」を出す。
//
// 実機 E2E は設定画面の webview に CDP で入る。繋がらない理由は何通りもあり
// (ポートが開いていない / 開いているが目当てのページが無い / WebSocket で弾かれる /
// 生き残りの WebView2 に相乗りして引数が効いていない)、「接続できない」の一言では
// 次の手が打てない。CI のランナーは後から覗けないので、落ちたその場で材料を残す。
import { spawnSync } from "node:child_process";

/** @param {number} port  @param {unknown} lastError 接続ループで最後に捕まえた例外 */
export async function describeCdpFailure(port, lastError) {
  const lines = [];
  if (lastError) {
    const e = /** @type {any} */ (lastError);
    lines.push(`最後のエラー: ${e?.message ?? e}${e?.cause ? ` (cause: ${e.cause.code ?? e.cause.message ?? e.cause})` : ""}`);
  }
  try {
    const version = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
    lines.push(`ポート ${port}: 応答あり (${version.Browser ?? "?"})`);
    const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
    lines.push(
      `ターゲット: ${list.map((t) => `${t.type} ${t.url}${t.webSocketDebuggerUrl ? "" : " (ws なし)"}`).join(" | ") || "(なし)"}`,
    );
  } catch (e) {
    lines.push(`ポート ${port}: 応答なし (${/** @type {any} */ (e)?.cause?.code ?? /** @type {any} */ (e)?.message})`);
  }
  // WebView2 のブラウザプロセス (--type= を持たないもの) の引数。ここに
  // --remote-debugging-port が無ければ、環境変数が効いていないか、別の起動に相乗りしている。
  const ps = spawnSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-Command",
      "$w = Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | " +
        "Where-Object { $_.CommandLine -notlike '*--type=*' }; " +
        "if (-not $w) { 'WebView2 ブラウザプロセス: (無し)' } else { $w | ForEach-Object { " +
        "$p = $_.ProcessId; $ports = (Get-NetTCPConnection -State Listen -OwningProcess $p -ErrorAction SilentlyContinue | " +
        "ForEach-Object { $_.LocalAddress + ':' + $_.LocalPort }) -join ', '; " +
        "\"WebView2 ブラウザプロセス pid=$p listen=[$ports]`n    $($_.CommandLine)\" } }",
    ],
    { encoding: "utf8" },
  );
  const browsers = (ps.stdout || ps.stderr || "(プロセス一覧を取れない)").trim();
  lines.push(browsers);
  // 管理者権限 (High IL) で動くホストでは、WebView2 は WEBVIEW2_* の環境変数と HKCU の
  // 上書きを無視し、HKLM のポリシーとコードで渡した引数だけを使う (Microsoft の
  // "Develop secure WebView2 apps")。GitHub Actions の Windows ランナーがこれに当たる。
  const elevated = spawnSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-Command",
      "([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent())" +
        ".IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)",
    ],
    { encoding: "utf8" },
  ).stdout?.trim();
  lines.push(`管理者権限で実行中: ${elevated || "(不明)"}`);
  if (elevated === "True" && !browsers.includes("--remote-debugging-port")) {
    lines.push(
      "→ 昇格したプロセスでは WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS が無視される。" +
        "HKLM\\SOFTWARE\\Policies\\Microsoft\\Edge\\WebView2\\AdditionalBrowserArguments に " +
        `"nox-voice.exe" = "--remote-debugging-port=${port}" を置き、NOX_E2E_CDP_PORT=${port} で走らせること`,
    );
  }
  return lines.map((l) => `    ${l}`).join("\n");
}
