// Visits every dashboard page in headless Chromium, signed in; records status,
// page errors, console errors, failed requests, blank renders, and a screenshot.
// usage: NODE_PATH=... node pages.mjs <base> <email> <password> <paths.txt> <outdir>
import { chromium } from "playwright-core";
import fs from "node:fs";

const [base, email, password, pathsFile, outDir] = process.argv.slice(2);
const paths = fs.readFileSync(pathsFile, "utf8").split("\n").filter(Boolean);
fs.mkdirSync(outDir, { recursive: true });

const browser = await chromium.launch({ executablePath: "/usr/bin/chromium" });
const page = await (await browser.newContext({ viewport: { width: 1440, height: 1000 } })).newPage();
let events = [];
page.on("pageerror", (e) => events.push(`pageerror: ${e.message}`));
page.on("console", (m) => { if (m.type() === "error" || m.type() === "warning") events.push(`console.${m.type()}: ${m.text().slice(0, 300)}`); });
page.on("response", (r) => { if (r.status() >= 400) events.push(`http ${r.status()} ${r.url()}`); });

await page.goto(`${base}/sign_in`);
await page.fill("input[name=email]", email);
await page.fill("input[name=password]", password);
await Promise.all([page.waitForURL(/dashboard/), page.click("button[type=submit]")]);

const results = [];
for (const line of paths) {
  const [name, path] = line.split(" ");
  events = [];
  let resp;
  const pre = await page.request.get(base + path, { maxRedirects: 0 });
  if (pre.status() >= 400) { const r = { name, path, status: pre.status(), error: (await pre.text()).slice(0, 80) }; results.push(r); console.log(`${name.padEnd(22)} ${r.status} ${r.error}`); continue; }
  try { resp = await page.goto(base + path, { waitUntil: "networkidle" }); }
  catch (e) { const r = { name, path, status: "ERR", error: e.message.split("\n")[0] }; results.push(r); console.log(`${name.padEnd(22)} ERR ${r.error}`); continue; }
  await page.waitForTimeout(300);
  const text = (await page.locator("main").innerText().catch(() => "")) || (await page.locator("body").innerText());
  const shot = `${outDir}/${name}.png`;
  await page.screenshot({ path: shot, fullPage: true });
  const r = { name, path, status: resp?.status(), chars: text.length, empty: (text.match(/Nothing here yet|No data|No .* yet/gi) || []).length, events };
  results.push(r);
  console.log(`${name.padEnd(22)} ${r.status} chars=${String(r.chars).padStart(6)} emptyMarkers=${r.empty} ${events.length ? "\n    " + events.join("\n    ") : ""}`);
}
fs.writeFileSync(`${outDir}/results.json`, JSON.stringify(results, null, 2));
await browser.close();
