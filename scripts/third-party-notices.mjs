// 配布物に同梱する第三者ライセンス表示 (THIRD_PARTY_NOTICES.txt) を作る。
//
// なぜ要るか: exe には Rust の依存が静的リンクされ、フロントエンドには npm の
// パッケージが埋め込まれる。MIT / BSD / Apache-2.0 などは「バイナリで配るときも
// 著作権表示とライセンス本文を添えること」を求め、MPL-2.0 はさらに「ソースの
// 入手先を知らせること」を求める。このファイルがそれを満たす。
//
// 中身は 2 つ:
//   1. Rust の依存 — cargo-about が各クレートの LICENSE を集める
//      (src-tauri/about.toml / about.hbs)
//   2. フロントエンドに入る npm パッケージ — 本番依存 (package-lock.json で
//      dev でないもの) と、ビルド時に自分のコードを出力へ埋め込む vite
//      (modulepreload のポリフィル。dist/assets を見ると実際に入っている)
//
// 使い方:
//   node scripts/third-party-notices.mjs <出力パス> [--offline]
// 要るもの:
//   cargo-about (cargo install --locked cargo-about --features cli) と npm ci 済みの
//   node_modules。--offline はクレートの取得を先に済ませてあるとき (cargo fetch) に使う。
//   オンラインだと、LICENSE を同梱し忘れたクレートの本文を元のリポジトリから取れる。
//
// 許可していないライセンスの依存が入っていたら cargo-about が失敗する
// (about.toml の accepted)。黙って配布しないため、失敗させたまま止める。
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const REPO = path.resolve(import.meta.dirname, "..");
const [out, ...flags] = process.argv.slice(2);
if (!out) {
  console.error("使い方: node scripts/third-party-notices.mjs <出力パス> [--offline]");
  process.exit(2);
}

/** ビルド時に自分のコードを出力へ埋め込むツール (本番依存ではないが同梱される)。 */
const BUNDLED_BUILD_TOOLS = ["vite"];

const RULE = "=".repeat(80);

function rustSection() {
  const args = ["about", "generate", "--locked", "--fail"];
  if (flags.includes("--offline")) args.push("--offline");
  args.push(
    "--manifest-path",
    path.join("src-tauri", "Cargo.toml"),
    "-c",
    path.join("src-tauri", "about.toml"),
    path.join("src-tauri", "about.hbs"),
  );
  const r = spawnSync("cargo", args, { cwd: REPO, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
  if (r.error) throw new Error(`cargo を起動できない: ${r.error.message}`);
  if (r.status !== 0) {
    console.error(r.stderr);
    throw new Error("cargo-about が失敗した (許可していないライセンスか、本文を読めないクレートがある)");
  }
  return r.stdout.trim();
}

/** LICENSE.spdx しか無いパッケージ向け (@tauri-apps/plugin-opener など)。 */
const MIT_TEXT = `Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.`;

function licenseTexts(dir) {
  const files = fs
    .readdirSync(dir)
    .filter((f) => /^(licen[cs]e|copying|notice)/i.test(f))
    .sort();
  const texts = files
    .filter((f) => !f.endsWith(".spdx"))
    .map((f) => fs.readFileSync(path.join(dir, f), "utf8").trim());
  if (texts.length) return texts;
  // SPDX の記述だけのとき: 宣言されたライセンスに MIT があればそれを選び、
  // SPDX に書かれた著作権表示と MIT の本文で表示を組み立てる。
  const spdx = files.find((f) => f.endsWith(".spdx"));
  if (spdx) {
    const s = fs.readFileSync(path.join(dir, spdx), "utf8");
    const declared = [...s.matchAll(/^PackageLicenseDeclared:\s*(.+)$/gm)].map((m) => m[1].trim());
    const copyright = s.match(/^PackageCopyrightText:\s*(.+)$/m)?.[1].trim();
    if (declared.includes("MIT") && copyright) {
      return [`MIT License (chosen from: ${declared.join(" OR ")})\n\nCopyright (c) ${copyright}\n\n${MIT_TEXT}`];
    }
  }
  throw new Error(`${dir} にライセンス本文が無い`);
}

function npmPackage(name, why) {
  const dir = path.join(REPO, "node_modules", ...name.split("/"));
  const pkg = JSON.parse(fs.readFileSync(path.join(dir, "package.json"), "utf8"));
  let texts = licenseTexts(dir);
  if (name === "vite") {
    // vite の LICENSE.md は後半に「vite 自身が同梱する依存」の表示が続くが、
    // こちらの出力に入るのは vite 自身のコードだけなので本体の節だけを採る。
    texts = texts.map((t) => t.split(/^# Licenses of bundled dependencies/m)[0].trim());
  }
  return [RULE, `${pkg.name} ${pkg.version} (${pkg.license})${why ? ` — ${why}` : ""}`, "", ...texts].join("\n");
}

function npmSection() {
  const lock = JSON.parse(fs.readFileSync(path.join(REPO, "package-lock.json"), "utf8"));
  const prod = Object.entries(lock.packages)
    .filter(([k, v]) => k.startsWith("node_modules/") && !v.dev && !v.devOptional)
    .map(([k]) => k.slice("node_modules/".length).split("/node_modules/").pop())
    .sort();
  return [
    ...[...new Set(prod)].map((name) => npmPackage(name, "")),
    ...BUNDLED_BUILD_TOOLS.map((name) => npmPackage(name, "runtime helpers emitted into the bundle")),
  ].join("\n\n");
}

const version = JSON.parse(fs.readFileSync(path.join(REPO, "package.json"), "utf8")).version;
const text = [
  `nox-voice ${version} — Third-party notices / 第三者ソフトウェアのライセンス表示`,
  "",
  "nox-voice itself is released under the MIT License (see LICENSE).",
  "This file lists the copyright notices and license texts of the third-party",
  "software included in this distribution.",
  "",
  "nox-voice 本体は MIT ライセンスです (LICENSE を参照)。このファイルは、配布物に",
  "含まれる第三者のソフトウェアの著作権表示とライセンス本文です。",
  "",
  "Source code of every Rust crate listed below is available from crates.io",
  "(https://crates.io/crates/<name>/<version>) and from the repository shown next",
  "to it. None of them has been modified.",
  "",
  "",
  "PART 1 — Rust crates (statically linked into nox-voice.exe)",
  "",
  rustSection(),
  "",
  "",
  RULE,
  "PART 2 — JavaScript packages (embedded in the user interface)",
  "",
  npmSection(),
  "",
].join("\n");

fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });
fs.writeFileSync(out, text);
console.log(`${out} を書き出しました (${text.split("\n").length} 行)`);
