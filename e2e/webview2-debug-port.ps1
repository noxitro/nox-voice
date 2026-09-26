# 管理者権限で実機 E2E を走らせるときに、WebView2 のデバッグポートを HKLM のポリシーで渡す。
#
# 昇格したホスト (High IL) では、WebView2 は WEBVIEW2_* の環境変数 (ハーネスが付ける
# WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS) と HKCU の上書きを無視し、HKLM のポリシーと
# コードで渡した引数だけを読む (Microsoft "Develop secure WebView2 apps")。
# GitHub Actions の Windows ランナーはジョブを管理者権限で走らせるのでこれに当たる。
# 値の名前は exe 名で、値はアプリ自身の引数 (wry の --disable-features=...) の後ろに足される。
#
# **使い終わったら必ず -Remove で消すこと。** 残すと普段使いのアプリまで localhost に
# デバッグポートを開き、同じ PC の任意のプロセスからアプリの画面を操作できる状態になる。
#
# 使い方 (管理者の PowerShell で):
#   ./e2e/webview2-debug-port.ps1 -Port 9555    # 置く
#   ./e2e/webview2-debug-port.ps1 -Remove       # 消す
param(
    [int]$Port = 0,
    [switch]$Remove
)
$ErrorActionPreference = 'Stop'
$key = 'HKLM:\SOFTWARE\Policies\Microsoft\Edge\WebView2\AdditionalBrowserArguments'
$name = 'nox-voice.exe'

if ($Remove) {
    Remove-ItemProperty -Path $key -Name $name -ErrorAction SilentlyContinue
    "WebView2 のデバッグポートのポリシーを消しました ($name)"
    exit 0
}
if ($Port -le 0) { throw '-Port か -Remove を指定すること' }
New-Item -Path $key -Force | Out-Null
New-ItemProperty -Path $key -Name $name -PropertyType String -Force -Value "--remote-debugging-port=$Port" | Out-Null
"WebView2 のデバッグポートをポリシーで渡しました: $name = --remote-debugging-port=$Port"
