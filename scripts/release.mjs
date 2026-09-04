#!/usr/bin/env node
/**
 * Release script for pi-mono
 *
 * Usage:
 *   node scripts/release.mjs <major|minor|patch>
 *   node scripts/release.mjs <x.y.z>
 *   node scripts/release.mjs <target> --dry-run   (preview changelog updates only)
 *
 * Steps:
 * 1. Check for uncommitted changes
 * 2. Bump version via npm run version:xxx or set an explicit version
 * 3. Rename each CHANGELOG.md [Unreleased] section to [version] - date
 * 4. Commit and tag
 * 5. Publish to npm
 */

import { execSync } from "child_process";
import { readFileSync, writeFileSync, readdirSync, existsSync } from "fs";
import { join } from "path";

const DRY_RUN = process.argv.includes("--dry-run");
const RELEASE_TARGET = process.argv.slice(2).find((arg) => !arg.startsWith("--"));
const BUMP_TYPES = new Set(["major", "minor", "patch"]);
const SEMVER_RE = /^\d+\.\d+\.\d+$/;

if (!RELEASE_TARGET || (!BUMP_TYPES.has(RELEASE_TARGET) && !SEMVER_RE.test(RELEASE_TARGET))) {
	console.error("Usage: node scripts/release.mjs <major|minor|patch|x.y.z> [--dry-run]");
	process.exit(1);
}

function run(cmd, options = {}) {
	console.log(`$ ${cmd}`);
	try {
		return execSync(cmd, { encoding: "utf-8", stdio: options.silent ? "pipe" : "inherit", ...options });
	} catch (e) {
		if (!options.ignoreError) {
			console.error(`Command failed: ${cmd}`);
			process.exit(1);
		}
		return null;
	}
}

function getVersion() {
	const pkg = JSON.parse(readFileSync("packages/ai/package.json", "utf-8"));
	return pkg.version;
}

function compareVersions(a, b) {
	const aParts = a.split(".").map(Number);
	const bParts = b.split(".").map(Number);

	for (let i = 0; i < 3; i++) {
		const diff = (aParts[i] || 0) - (bParts[i] || 0);
		if (diff !== 0) {
			return diff;
		}
	}

	return 0;
}

function shellQuote(value) {
	return `'${value.replace(/'/g, `'\\''`)}'`;
}

function stageChangedFiles() {
	const output = run("git ls-files -m -o -d --exclude-standard", { silent: true });
	const paths = [...new Set((output || "").split("\n").map((line) => line.trim()).filter(Boolean))];
	if (paths.length === 0) {
		return;
	}

	run(`git add -- ${paths.map(shellQuote).join(" ")}`);
}

function bumpOrSetVersion(target) {
	const currentVersion = getVersion();

	if (BUMP_TYPES.has(target)) {
		console.log(`Bumping version (${target})...`);
		run(`npm run version:${target}`);
		return getVersion();
	}

	if (compareVersions(target, currentVersion) <= 0) {
		console.error(`Error: explicit version ${target} must be greater than current version ${currentVersion}.`);
		process.exit(1);
	}

	console.log(`Setting explicit version (${target})...`);
	run(
		`npm version ${target} -ws --no-git-tag-version && node scripts/sync-versions.js && npx shx rm -rf node_modules packages/*/node_modules package-lock.json && npm install`,
	);
	return getVersion();
}

function getChangelogs() {
	const packagesDir = "packages";
	const packages = readdirSync(packagesDir);
	return packages
		.map((pkg) => join(packagesDir, pkg, "CHANGELOG.md"))
		.filter((path) => existsSync(path));
}

function updateChangelogsForRelease(version) {
	const date = new Date().toISOString().split("T")[0];
	const changelogs = getChangelogs();
	const unreleasedHeader = "## [Unreleased]";
	const releaseHeader = `## [${version}] - ${date}`;

	for (const changelog of changelogs) {
		const content = readFileSync(changelog, "utf-8");
		if (!content.includes(unreleasedHeader)) {
			console.log(`  Skipping ${changelog}: no unreleased section`);
			continue;
		}
		const updated = content.replace(unreleasedHeader, releaseHeader);

		if (DRY_RUN) {
			console.log(`\n--- ${changelog} ---`);
			const escapedVersion = version.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
			const sectionRe = new RegExp(`## \\[${escapedVersion}\\][\\s\\S]*?(?=\\n## \\[|$)`);
			console.log((updated.match(sectionRe) || ["(no release section)"])[0]);
		} else {
			writeFileSync(changelog, updated);
			console.log(`  Updated ${changelog}`);
		}
	}
}

function previewVersion(target) {
	if (!BUMP_TYPES.has(target)) {
		return target;
	}
	const [major, minor, patch] = getVersion().split(".").map(Number);
	if (target === "major") return `${major + 1}.0.0`;
	if (target === "minor") return `${major}.${minor + 1}.0`;
	return `${major}.${minor}.${patch + 1}`;
}

console.log("\n=== Release Script ===\n");

if (DRY_RUN) {
	const version = previewVersion(RELEASE_TARGET);
	console.log(`Dry run for v${version}: previewing changelog updates, no files are written.`);
	updateChangelogsForRelease(version);
	console.log("\n=== Dry run complete (no changes made) ===");
	process.exit(0);
}

console.log("Checking for uncommitted changes...");
const status = run("git status --porcelain", { silent: true });
if (status && status.trim()) {
	console.error("Error: Uncommitted changes detected. Commit or stash first.");
	console.error(status);
	process.exit(1);
}
console.log("  Working directory clean\n");

const version = bumpOrSetVersion(RELEASE_TARGET);
console.log(`  New version: ${version}\n`);

console.log("Updating CHANGELOG.md files...");
updateChangelogsForRelease(version);
console.log();

console.log("Committing and tagging...");
stageChangedFiles();
run(`git commit -m "Release v${version}"`);
run(`git tag v${version}`);
console.log();

console.log("Publishing to npm...");
run("npm run publish");
console.log();

console.log("Pushing to remote...");
run("git push origin main");
run(`git push origin v${version}`);
console.log();

console.log(`=== Released v${version} ===`);
