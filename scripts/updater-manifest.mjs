// アプリ内の自動更新 (src-tauri/src/updater.rs) が読む latest.json を作る。
// リリースのワークフローが、インストーラと署名 (.sig) を揃えたあとに走らせる。
//
// 使い方:
//   node scripts/updater-manifest.mjs --dir release-assets --tag v0.7.0 [--repo owner/name]
//   node scripts/updater-manifest.mjs --self-test
//
// --repo を省くと GITHUB_REPOSITORY (Actions が入れる) を使う。出力は <dir>/latest.json。
//
// # なぜ作る前に署名を確かめるのか
//
// アプリは latest.json の signature を tauri.conf.json の pubkey で確かめ、合わなければ
// 更新を捨てる。つまり GitHub の Secret に**別の鍵**を登録してしまった、あるいは
// パスワードだけ違う鍵で署名した、といった事故は、公開してから利用者の手元で
// 「更新に失敗しました」になって初めて分かる。しかも既に配った版は直せない。
// ここで同じ検証 (minisign: Ed25519 + BLAKE2b-512 の前処理、信頼コメントへの
// 全体署名、署名に記録された版) を通してから書き出し、合わなければ公開させない。
//
// # platforms のキー
//
// プラグインは `{os}-{arch}-{インストーラの種類}` を探し、無ければ `{os}-{arch}` を使う
// (tauri-plugin-updater 2.12.0 の updater.rs `get_urls`)。種類はインストーラが exe に
// 書き込む (tauri-bundler の bundle_type)。MSI で入れた人には MSI を、NSIS で入れた人には
// NSIS を返すため両方のキーを書き、種類の分からない exe (手元の cargo build) 向けに
// 種類なしのキーも NSIS で書く。
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const REPO_ROOT = path.resolve(import.meta.dirname, "..");

/** Ed25519 の生の公開鍵 32 バイトを SPKI (DER) に包む接頭辞。 */
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

/** minisign の公開鍵 (tauri.conf.json の pubkey = 公開鍵ファイルの base64) を読む。 */
export function parsePublicKey(pubkeyBase64) {
  const text = Buffer.from(pubkeyBase64, "base64").toString("utf8");
  const line = text.split(/\r?\n/).find((l) => l && !l.startsWith("untrusted comment:"));
  const raw = Buffer.from(line ?? "", "base64");
  if (raw.length !== 42 || raw.subarray(0, 2).toString("latin1") !== "Ed") {
    throw new Error("公開鍵が minisign の形ではない");
  }
  return {
    keyId: raw.subarray(2, 10),
    key: crypto.createPublicKey({
      key: Buffer.concat([ED25519_SPKI_PREFIX, raw.subarray(10)]),
      format: "der",
      type: "spki",
    }),
  };
}

/**
 * .sig ファイルの中身 (minisign の署名ファイルの base64) を検証し、
 * 信頼コメントの key:value を返す。合わなければ例外。
 */
export function verifySignature(sigBase64, data, publicKey) {
  const text = Buffer.from(sigBase64.trim(), "base64").toString("utf8");
  const lines = text.split(/\r?\n/);
  if (lines.length < 4 || !lines[0].startsWith("untrusted comment:")) {
    throw new Error("署名が minisign の形ではない");
  }
  const sig = Buffer.from(lines[1], "base64");
  const trustedPrefix = "trusted comment: ";
  if (sig.length !== 74 || !lines[2].startsWith(trustedPrefix)) {
    throw new Error("署名が minisign の形ではない");
  }
  const alg = sig.subarray(0, 2).toString("latin1");
  if (!sig.subarray(2, 10).equals(publicKey.keyId)) {
    throw new Error("署名した鍵が tauri.conf.json の pubkey と違う (鍵 ID が一致しない)");
  }
  const signature = sig.subarray(10);
  // "ED" は中身を BLAKE2b-512 で要約してから署名する形 (Tauri が作るのはこちら)。
  // "Ed" は中身そのものに署名する古い形。
  const message =
    alg === "ED"
      ? crypto.createHash("blake2b512").update(data).digest()
      : alg === "Ed"
        ? data
        : null;
  if (!message) throw new Error(`未知の署名方式: ${alg}`);
  if (!crypto.verify(null, message, publicKey.key, signature)) {
    throw new Error("署名が中身と合わない");
  }
  const trusted = lines[2].slice(trustedPrefix.length);
  const globalSig = Buffer.from(lines[3], "base64");
  if (!crypto.verify(null, Buffer.concat([signature, Buffer.from(trusted, "utf8")]), publicKey.key, globalSig)) {
    throw new Error("信頼コメントの署名が合わない");
  }
  return Object.fromEntries(
    trusted.split("\t").map((field) => {
      const i = field.indexOf(":");
      return i < 0 ? [field, ""] : [field.slice(0, i), field.slice(i + 1)];
    }),
  );
}

function findOne(dir, suffix, required) {
  const hits = fs.readdirSync(dir).filter((n) => n.endsWith(suffix));
  if (hits.length > 1) throw new Error(`${dir} に *${suffix} が複数ある: ${hits.join(", ")}`);
  if (hits.length === 0 && required) throw new Error(`${dir} に *${suffix} が無い`);
  return hits[0];
}

/** インストーラ 1 つを検証し、platforms の 1 項目を作る。 */
function platformEntry({ dir, file, version, tag, repo, publicKey }) {
  const sigPath = path.join(dir, `${file}.sig`);
  if (!fs.existsSync(sigPath)) {
    throw new Error(`${file}.sig が無い (bundle.createUpdaterArtifacts と署名鍵の環境変数を確かめること)`);
  }
  const signature = fs.readFileSync(sigPath, "utf8").trim();
  const fields = verifySignature(signature, fs.readFileSync(path.join(dir, file)), publicKey);
  // アプリは署名に記録された版と latest.json の版を突き合わせる (tauri.conf.json の
  // requireSignedVersion)。記録が無い・食い違うと更新を捨てる。
  if (fields.version === undefined) {
    throw new Error(`${file} の署名に版が記録されていない (requireSignedVersion のアプリは受け付けない)`);
  }
  if (fields.version !== version) {
    throw new Error(`${file} の署名は版 ${fields.version} のもの (リリースは ${version})`);
  }
  if (fields.file !== undefined && fields.file !== file) {
    throw new Error(`${file}.sig は別のファイル (${fields.file}) の署名`);
  }
  return {
    signature,
    url: `https://github.com/${repo}/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(file)}`,
  };
}

/** latest.json の中身を組み立てる (書き出しはしない)。 */
export function buildManifest({ dir, tag, repo, pubkey, now = new Date() }) {
  if (!/^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(tag ?? "")) {
    throw new Error(`タグが vX.Y.Z の形ではない: ${tag}`);
  }
  if (!/^[\w.-]+\/[\w.-]+$/.test(repo ?? "")) throw new Error(`リポジトリが owner/name の形ではない: ${repo}`);
  const version = tag.slice(1);
  const publicKey = parsePublicKey(pubkey);
  const common = { dir, version, tag, repo, publicKey };

  const nsis = platformEntry({ ...common, file: findOne(dir, "-setup.exe", true) });
  const platforms = { "windows-x86_64-nsis": nsis };
  const msiFile = findOne(dir, ".msi", false);
  if (msiFile) platforms["windows-x86_64-msi"] = platformEntry({ ...common, file: msiFile });
  // 種類の分からない exe 向け (この節の冒頭の doc)。
  platforms["windows-x86_64"] = nsis;

  return {
    version,
    // RFC 3339。ミリ秒は付けない (付けても読めるが、付ける意味が無い)。
    pub_date: now.toISOString().replace(/\.\d{3}Z$/, "Z"),
    platforms,
  };
}

function readPubkey() {
  const conf = JSON.parse(fs.readFileSync(path.join(REPO_ROOT, "src-tauri/tauri.conf.json"), "utf8"));
  const pubkey = conf.plugins?.updater?.pubkey;
  if (!pubkey) throw new Error("tauri.conf.json に plugins.updater.pubkey が無い");
  return pubkey;
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--self-test") args.selfTest = true;
    else if (a.startsWith("--")) args[a.slice(2)] = argv[++i];
    else throw new Error(`知らない引数: ${a}`);
  }
  return args;
}

// ---- 自己テスト: その場で鍵を作り、Tauri と同じ形の署名で正常系と異常系を通す ----

function minisignFixture() {
  const { publicKey, privateKey } = crypto.generateKeyPairSync("ed25519");
  const keyId = crypto.randomBytes(8);
  const rawPk = publicKey.export({ format: "der", type: "spki" }).subarray(ED25519_SPKI_PREFIX.length);
  const pubText = `untrusted comment: minisign public key: TEST\n${Buffer.concat([Buffer.from("Ed"), keyId, rawPk]).toString("base64")}\n`;
  const sign = (data, trusted, key = privateKey, id = keyId) => {
    const signature = crypto.sign(null, crypto.createHash("blake2b512").update(data).digest(), key);
    const global = crypto.sign(null, Buffer.concat([signature, Buffer.from(trusted)]), key);
    const text = [
      "untrusted comment: signature from tauri secret key",
      Buffer.concat([Buffer.from("ED"), id, signature]).toString("base64"),
      `trusted comment: ${trusted}`,
      global.toString("base64"),
      "",
    ].join("\n");
    return Buffer.from(text).toString("base64");
  };
  return { pubkey: Buffer.from(pubText).toString("base64"), sign };
}

function selfTest() {
  const assert = (cond, msg) => {
    if (!cond) throw new Error(`自己テスト失敗: ${msg}`);
  };
  const throws = (fn, re, msg) => {
    try {
      fn();
    } catch (e) {
      assert(re.test(e.message), `${msg}: 想定外のエラー ${e.message}`);
      return;
    }
    assert(false, `${msg}: 例外にならなかった`);
  };

  const { pubkey, sign } = minisignFixture();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nox-updater-manifest-"));
  try {
    const exe = "nox-voice_1.2.3_x64-setup.exe";
    const msi = "nox-voice_1.2.3_x64_en-US.msi";
    const write = (name, body, version = "1.2.3") => {
      fs.writeFileSync(path.join(dir, name), body);
      const trusted = `timestamp:1700000000\tfile:${name}` + (version ? `\tversion:${version}` : "");
      fs.writeFileSync(path.join(dir, `${name}.sig`), sign(Buffer.from(body), trusted));
    };
    write(exe, "nsis-body");
    write(msi, "msi-body");
    fs.writeFileSync(path.join(dir, "THIRD_PARTY_NOTICES.txt"), "x");

    const now = new Date("2026-10-18T01:02:03.456Z");
    const m = buildManifest({ dir, tag: "v1.2.3", repo: "noxitro/nox-voice", pubkey, now });
    assert(m.version === "1.2.3", "版の先頭の v を外す");
    assert(m.pub_date === "2026-10-18T01:02:03Z", `pub_date: ${m.pub_date}`);
    assert(
      JSON.stringify(Object.keys(m.platforms).sort()) ===
        JSON.stringify(["windows-x86_64", "windows-x86_64-msi", "windows-x86_64-nsis"]),
      `platforms のキー: ${Object.keys(m.platforms)}`,
    );
    assert(
      m.platforms["windows-x86_64-nsis"].url ===
        `https://github.com/noxitro/nox-voice/releases/download/v1.2.3/${exe}`,
      `url: ${m.platforms["windows-x86_64-nsis"].url}`,
    );
    assert(m.platforms["windows-x86_64"].url === m.platforms["windows-x86_64-nsis"].url, "種類なしは NSIS");
    assert(m.platforms["windows-x86_64-msi"].url.endsWith(`/${msi}`), "MSI の url");
    assert(
      m.platforms["windows-x86_64-nsis"].signature === fs.readFileSync(path.join(dir, `${exe}.sig`), "utf8").trim(),
      "signature は .sig の中身そのもの",
    );

    // MSI が無いとき (プレリリース) は NSIS だけ
    fs.renameSync(path.join(dir, msi), path.join(dir, "msi.bak"));
    const nsisOnly = buildManifest({ dir, tag: "v1.2.3", repo: "noxitro/nox-voice", pubkey, now });
    assert(!("windows-x86_64-msi" in nsisOnly.platforms), "MSI が無ければ msi のキーを書かない");
    fs.renameSync(path.join(dir, "msi.bak"), path.join(dir, msi));

    // 中身の改ざん
    fs.writeFileSync(path.join(dir, exe), "tampered");
    throws(() => buildManifest({ dir, tag: "v1.2.3", repo: "a/b", pubkey }), /中身と合わない/, "改ざん");
    // 署名に記録された版がタグと違う
    write(exe, "nsis-body", "1.2.2");
    throws(() => buildManifest({ dir, tag: "v1.2.3", repo: "a/b", pubkey }), /版 1\.2\.2/, "版の食い違い");
    // 版の記録が無い古い形の署名
    write(exe, "nsis-body", null);
    throws(() => buildManifest({ dir, tag: "v1.2.3", repo: "a/b", pubkey }), /版が記録されていない/, "版の記録無し");
    // 別の鍵で署名した (Secret の登録間違い)
    write(exe, "nsis-body");
    const other = minisignFixture();
    throws(() => buildManifest({ dir, tag: "v1.2.3", repo: "a/b", pubkey: other.pubkey }), /鍵 ID/, "別の鍵");
    // .sig が無い (署名されずにビルドされた)
    fs.rmSync(path.join(dir, `${exe}.sig`));
    throws(() => buildManifest({ dir, tag: "v1.2.3", repo: "a/b", pubkey }), /\.sig が無い/, ".sig 無し");
    // タグの形
    throws(() => buildManifest({ dir, tag: "1.2.3", repo: "a/b", pubkey }), /タグ/, "v の無いタグ");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }

  // 本物の公開鍵 (tauri.conf.json) が読めること
  parsePublicKey(readPubkey());
  console.log("自己テスト: ok");
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.selfTest) return selfTest();
  if (!args.dir || !args.tag) {
    console.error("使い方: node scripts/updater-manifest.mjs --dir <dir> --tag vX.Y.Z [--repo owner/name] | --self-test");
    process.exit(2);
  }
  const manifest = buildManifest({
    dir: args.dir,
    tag: args.tag,
    repo: args.repo ?? process.env.GITHUB_REPOSITORY,
    pubkey: readPubkey(),
  });
  const out = path.join(args.dir, "latest.json");
  fs.writeFileSync(out, JSON.stringify(manifest, null, 2) + "\n");
  console.log(`${out} を書いた (版 ${manifest.version} / ${Object.keys(manifest.platforms).join(", ")})`);
}

try {
  main();
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
