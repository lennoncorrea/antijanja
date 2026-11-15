// Gera o instalador assinado e o latest.json que o auto-update lê.
//
//   npm run release                 -> build + latest.json (não publica nada)
//   npm run release -- --skip-build -> só refaz o latest.json do build existente
//   npm run release -- --publish    -> também cria a release vX.Y.Z no GitHub (gh CLI)
//
// A chave privada fica em ~/.tauri/antijanja.key (fora do projeto). Sem ela não dá para
// assinar e os apps instalados recusam a atualização. Guarde uma cópia em lugar seguro.
import { execSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = "lennoncorrea/antijanja";
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = new Set(process.argv.slice(2));
const conf = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8"));
const version = conf.version;
const exeName = `${conf.productName}_${version}_x64-setup.exe`;

if (!args.has("--skip-build")) {
  const key = process.env.TAURI_SIGNING_PRIVATE_KEY || join(homedir(), ".tauri", "antijanja.key");
  if (!process.env.TAURI_SIGNING_PRIVATE_KEY && !existsSync(key)) {
    console.error(`Chave de assinatura não encontrada em ${key}.`);
    process.exit(1);
  }
  execSync("npm run build", {
    cwd: root,
    stdio: "inherit",
    env: {
      ...process.env,
      TAURI_SIGNING_PRIVATE_KEY: key,
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? "",
    },
  });
}

const targetDirs = [process.env.CARGO_TARGET_DIR, join(root, "src-tauri", "target"), join(root, "src-tauri", "target-release")].filter(Boolean);
const bundleDir = targetDirs.map((d) => join(d, "release", "bundle", "nsis")).find((d) => existsSync(join(d, exeName)));
if (!bundleDir) {
  console.error(`Não achei ${exeName}. Rode sem --skip-build.`);
  process.exit(1);
}
const exe = join(bundleDir, exeName);
const sigPath = `${exe}.sig`;
if (!existsSync(sigPath)) {
  console.error(`Falta ${sigPath}: o build não foi assinado (createUpdaterArtifacts + chave).`);
  process.exit(1);
}

const latest = {
  version,
  notes: `AntiJanja ${version}`,
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": {
      signature: readFileSync(sigPath, "utf8").trim(),
      url: `https://github.com/${REPO}/releases/download/v${version}/${exeName}`,
    },
  },
};
const latestPath = join(bundleDir, "latest.json");
writeFileSync(latestPath, JSON.stringify(latest, null, 2) + "\n");
console.log(`\nPronto:\n  ${exe}\n  ${latestPath}`);

const publish = `gh release create v${version} "${exe}" "${latestPath}" --repo ${REPO} --title "AntiJanja ${version}" --notes "AntiJanja ${version}"`;
if (args.has("--publish")) {
  execSync(publish, { stdio: "inherit" });
} else {
  console.log(`\nPara publicar (os apps instalados atualizam ao abrir):\n  ${publish}`);
}
