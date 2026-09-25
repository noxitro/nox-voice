// アプリの版は 5 か所に書かれている。1 か所でも揃っていないと、
// インストーラの表示と実体が食い違う (実際に起きる事故)。
//
//   package.json / package-lock.json (2 か所) / src-tauri/Cargo.toml /
//   src-tauri/Cargo.lock / src-tauri/tauri.conf.json
//
// 使い方:
//   node scripts/version.mjs check              5 か所が揃っているか (CI が毎回走らせる)
//   node scripts/version.mjs check --tag v1.2.3 さらにタグと一致するか (リリースが走らせる)
//   node scripts/version.mjs set 1.2.3          5 か所をまとめて書き換える
//
// 書き換えは JSON を読んで書き戻す。この 3 ファイルは 2 スペース字下げ +
// 末尾改行で、読んで書き戻しても 1 バイトも変わらないことを確かめてある
// (整形の差分を混ぜないため)。Cargo の 2 ファイルは該当行だけを置き換える。
import fs from "node:fs";
import path from "node:path";

const REPO = path.resolve(import.meta.dirname, "..");
const file = (rel) => path.join(REPO, rel);

/** semver の major.minor.patch と、任意のプレリリース (例: 0.6.0-beta.1)。 */
const VERSION_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

function readJson(rel) {
  return JSON.parse(fs.readFileSync(file(rel), "utf8"));
}
function writeJson(rel, value) {
  fs.writeFileSync(file(rel), JSON.stringify(value, null, 2) + "\n");
}

/** Cargo.toml の [package] 節にある version 行。 */
const CARGO_TOML_RE = /(\[package\][^[]*?\nversion = ")([^"]+)(")/;
/** Cargo.lock の nox-voice 自身の項目にある version 行。 */
const CARGO_LOCK_RE = /(\[\[package\]\]\nname = "nox-voice"\nversion = ")([^"]+)(")/;

function readAll() {
  const pkg = readJson("package.json");
  const lock = readJson("package-lock.json");
  const conf = readJson("src-tauri/tauri.conf.json");
  const toml = fs.readFileSync(file("src-tauri/Cargo.toml"), "utf8").match(CARGO_TOML_RE);
  const cargoLock = fs.readFileSync(file("src-tauri/Cargo.lock"), "utf8").match(CARGO_LOCK_RE);
  return [
    ["package.json", pkg.version],
    ["package-lock.json (version)", lock.version],
    ['package-lock.json (packages[""])', lock.packages?.[""]?.version],
    ["src-tauri/Cargo.toml", toml?.[2]],
    ["src-tauri/Cargo.lock", cargoLock?.[2]],
    ["src-tauri/tauri.conf.json", conf.version],
  ];
}

function check(tag) {
  const rows = readAll();
  const expected = tag ? tag.replace(/^v/, "") : rows[0][1];
  let ok = true;
  for (const [where, v] of rows) {
    const good = v === expected;
    ok &&= good;
    console.log(`${good ? "ok  " : "NG  "} ${v ?? "(見つからない)"}  ${where}`);
  }
  if (tag && !VERSION_RE.test(expected)) {
    console.error(`タグ ${tag} は vX.Y.Z (任意で -プレリリース) の形ではない`);
    ok = false;
  }
  if (!ok) {
    console.error(
      tag
        ? `\n版がタグ ${tag} と一致しない。node scripts/version.mjs set ${expected} で揃えてからタグを打ち直すこと。`
        : "\n版が揃っていない。node scripts/version.mjs set <版> で揃えること。",
    );
    process.exit(1);
  }
  console.log(`\n5 か所とも ${expected}`);
}

function set(version) {
  if (!VERSION_RE.test(version ?? "")) {
    console.error(`版の形が不正: ${version} (例: 1.2.3 / 1.3.0-beta.1)`);
    process.exit(1);
  }
  const pkg = readJson("package.json");
  pkg.version = version;
  writeJson("package.json", pkg);

  const lock = readJson("package-lock.json");
  lock.version = version;
  lock.packages[""].version = version;
  writeJson("package-lock.json", lock);

  const conf = readJson("src-tauri/tauri.conf.json");
  conf.version = version;
  writeJson("src-tauri/tauri.conf.json", conf);

  for (const [rel, re] of [
    ["src-tauri/Cargo.toml", CARGO_TOML_RE],
    ["src-tauri/Cargo.lock", CARGO_LOCK_RE],
  ]) {
    const text = fs.readFileSync(file(rel), "utf8");
    if (!re.test(text)) throw new Error(`${rel} に版の行が見つからない`);
    fs.writeFileSync(file(rel), text.replace(re, `$1${version}$3`));
  }
  check();
}

const [cmd, arg, tag] = process.argv.slice(2);
if (cmd === "check") check(arg === "--tag" ? tag : undefined);
else if (cmd === "set") set(arg);
else {
  console.error("使い方: node scripts/version.mjs check [--tag vX.Y.Z] | set X.Y.Z");
  process.exit(2);
}
