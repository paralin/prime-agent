import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { runInNewContext } from "node:vm";

const commit = process.argv[2];
if (!commit) throw new Error("Usage: node scripts/translate-model-catalog.mjs <original-commit>");
const target = new URL("../crates/pa-ai/src/models_fork.json", import.meta.url);

function catalog(ref) {
  const source = execFileSync("git", ["show", `${ref}:packages/ai/src/models.generated.ts`], {
    encoding: "utf8",
    maxBuffer: 32 * 1024 * 1024,
  });
  const code = stripTypeScriptTypes(source).replace("export const MODELS", "const MODELS");
  return runInNewContext(`${code}; MODELS`, Object.create(null), { timeout: 1000 });
}

const before = catalog(`${commit}^`);
const after = catalog(commit);
const previous = existsSync(target) ? JSON.parse(readFileSync(target, "utf8")) : { models: [], removed: [] };
const key = (provider, id) => `${provider}\0${id}`;
const patches = new Map(previous.models.map((patch) => [key(patch.model.provider, patch.model.id), patch]));
const removed = new Map(previous.removed.map((row) => [key(row.provider, row.id), row]));
for (const [provider, models] of Object.entries(after)) {
  for (const [id, model] of Object.entries(models)) {
    const original = before[provider]?.[id];
    const changedFields = [...new Set([...Object.keys(original ?? {}), ...Object.keys(model)])]
      .filter((field) => JSON.stringify(original?.[field]) !== JSON.stringify(model[field]));
    if (changedFields.length === 0) continue;
    const previousPatch = patches.get(key(provider, id));
    patches.set(key(provider, id), {
      model,
      changedFields: [...new Set([...(previousPatch?.changedFields ?? []), ...changedFields])].sort(),
    });
    removed.delete(key(provider, id));
  }
}
for (const [provider, models] of Object.entries(before)) {
  for (const id of Object.keys(models)) {
    if (after[provider]?.[id]) continue;
    patches.delete(key(provider, id));
    removed.set(key(provider, id), { provider, id });
  }
}
const compare = (left, right) => key(left.provider, left.id).localeCompare(key(right.provider, right.id));
const models = [...patches.values()].sort((left, right) => compare(left.model, right.model));
writeFileSync(target, `${JSON.stringify({ models, removed: [...removed.values()].sort(compare) }, null, 2)}\n`);
console.log(`Translated ${models.length} model patches and ${removed.size} removals.`);
