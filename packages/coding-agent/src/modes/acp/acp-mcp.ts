import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { McpServer } from "@agentclientprotocol/sdk";
import { RequestError } from "@agentclientprotocol/sdk";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { getDefaultEnvironment, StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";

interface TemporarySkillHost {
	extendTemporarySkills(skillPaths: string[], source: string): Promise<void> | void;
	getSkillNames?(): Promise<string[]>;
}

type AcpMcpServerConfig =
	| { type: "http"; url: string; headers: Record<string, string> }
	| { type: "stdio"; command: string; args: string[]; env: Record<string, string> };

type AcpMcpTool = Pick<Tool, "name" | "description" | "inputSchema">;
type DiscoverTools = (server: string, config: AcpMcpServerConfig, cwd: string) => Promise<AcpMcpTool[]>;

interface DiscoveredSkill {
	server: string;
	config: AcpMcpServerConfig;
	tool: AcpMcpTool;
	importName: string;
}

function resolveServers(servers: readonly McpServer[]): Record<string, AcpMcpServerConfig> {
	const resolved = Object.create(null) as Record<string, AcpMcpServerConfig>;
	for (const server of servers) {
		if (Object.hasOwn(resolved, server.name)) {
			throw RequestError.invalidParams({ reason: `duplicate MCP server name: ${server.name}` });
		}
		if (!server.name) {
			throw RequestError.invalidParams({ reason: "MCP server names must not be empty" });
		}
		if ("command" in server) {
			if (!server.command) {
				throw RequestError.invalidParams({ reason: `MCP server ${server.name} has no stdio command` });
			}
			resolved[server.name] = {
				type: "stdio",
				command: server.command,
				args: [...server.args],
				env: Object.fromEntries(server.env.map((entry) => [entry.name, entry.value])),
			};
			continue;
		}
		if (server.type === "http") {
			if (!server.url) {
				throw RequestError.invalidParams({ reason: `MCP server ${server.name} has no HTTP URL` });
			}
			resolved[server.name] = {
				type: "http",
				url: server.url,
				headers: Object.fromEntries(server.headers.map((header) => [header.name, header.value])),
			};
			continue;
		}
		throw RequestError.invalidParams({
			reason: `MCP server ${server.name} uses unsupported ${server.type} transport`,
		});
	}
	return resolved;
}

function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

async function discoverServerTools(server: string, config: AcpMcpServerConfig, cwd: string): Promise<AcpMcpTool[]> {
	const client = new Client({ name: "prime-agent-acp", version: "1.0.0" });
	try {
		const transport =
			config.type === "http"
				? new StreamableHTTPClientTransport(new URL(config.url), {
						requestInit: { headers: config.headers },
					})
				: new StdioClientTransport({
						command: config.command,
						args: config.args,
						env: { ...getDefaultEnvironment(), ...config.env },
						cwd,
					});
		await client.connect(transport);
		const result = await client.listTools();
		return result.tools.map((tool) => ({
			name: tool.name,
			description: tool.description,
			inputSchema: tool.inputSchema,
		}));
	} catch (error) {
		throw RequestError.invalidParams({
			reason: `failed to discover tools from MCP server ${server}: ${errorText(error)}`,
		});
	} finally {
		await client.close().catch(() => undefined);
	}
}

function skillImportName(server: string, tool: string): string {
	const normalized = `${server}_${tool}`.replace(/\W/g, "_");
	return /^\d/.test(normalized) ? `_${normalized}` : normalized;
}

function skillDirectoryName(importName: string): string {
	return importName.replaceAll("_", "-");
}

function pythonModule(server: string, config: AcpMcpServerConfig, tool: AcpMcpTool): string {
	const encodedConfig = JSON.stringify(JSON.stringify(config));
	const encodedTool = JSON.stringify(JSON.stringify(tool));
	return `"""ACP MCP tool generated for this session."""

import json

from rlm import make_acp_mcp_skill

run = make_acp_mcp_skill(
    ${JSON.stringify(server)},
    json.loads(${encodedConfig}),
    json.loads(${encodedTool}),
)
`;
}

function skillMarkdown(skill: DiscoveredSkill): string {
	const summary = skill.tool.description?.split("\n", 1)[0] || `Call ${skill.tool.name} on ${skill.server}.`;
	const description = `MCP tool ${skill.server}/${skill.tool.name}: ${summary}`.slice(0, 1024);
	const skillName = skillDirectoryName(skill.importName);
	return `---
name: ${JSON.stringify(skillName)}
description: ${JSON.stringify(description)}
---

# ${skill.importName}

This session-scoped skill calls MCP tool ${JSON.stringify(skill.tool.name)} on server
${JSON.stringify(skill.server)}. It is pre-imported in IPython and callable directly:

\`\`\`python
await ${skill.importName}(...)
\`\`\`

Use \`help(${skill.importName})\` or \`inspect.signature(${skill.importName})\` for its
description and schema-derived keyword arguments.
`;
}

function pyproject(skill: DiscoveredSkill): string {
	const digest = createHash("sha256")
		.update(JSON.stringify([skill.server, skill.config, skill.tool]))
		.digest("hex")
		.slice(0, 16);
	return `[project]
name = "prime-agent-acp-mcp-${digest}"
version = "0.0.0"
requires-python = ">=3.10"
dependencies = ["mcp>=1.28,<2", "httpx>=0.27,<1"]

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.hatch.build.targets.wheel]
packages = ["src/${skill.importName}"]
`;
}

export class AcpMcpSkillInstaller {
	private root: string | undefined;
	private installedSkillNames = new Set<string>();

	constructor(
		private readonly host: TemporarySkillHost,
		private readonly discoverTools: DiscoverTools = discoverServerTools,
	) {}

	async configure(servers: readonly McpServer[], cwd: string): Promise<void> {
		const config = resolveServers(servers);
		const discovered: DiscoveredSkill[] = [];
		const names = new Map<string, { server: string; tool: string }>();
		for (const [server, spec] of Object.entries(config)) {
			const tools = await this.discoverTools(server, spec, cwd);
			for (const tool of tools) {
				if (!tool.name) {
					throw RequestError.invalidParams({ reason: `MCP server ${server} returned a tool with an empty name` });
				}
				const importName = skillImportName(server, tool.name);
				const prior = names.get(importName);
				if (prior) {
					throw RequestError.invalidParams({
						reason:
							`MCP tools ${prior.server}/${prior.tool} and ${server}/${tool.name} ` +
							`normalize to the same Python skill ${importName}`,
					});
				}
				names.set(importName, { server, tool: tool.name });
				discovered.push({ server, config: spec, tool, importName });
			}
		}

		const existingSkillNames = new Set((await this.host.getSkillNames?.()) ?? []);
		for (const installedName of this.installedSkillNames) existingSkillNames.delete(installedName);
		for (const skill of discovered) {
			const skillName = skillDirectoryName(skill.importName);
			if (existingSkillNames.has(skillName)) {
				throw RequestError.invalidParams({
					reason: `MCP tool ${skill.server}/${skill.tool.name} conflicts with existing skill ${skillName}`,
				});
			}
		}
		const nextRoot = discovered.length > 0 ? mkdtempSync(join(tmpdir(), "prime-agent-acp-mcp-")) : undefined;
		const skillPaths: string[] = [];
		for (const skill of discovered) {
			const skillDir = join(nextRoot!, skillDirectoryName(skill.importName));
			const packageDir = join(skillDir, "src", skill.importName);
			mkdirSync(packageDir, { recursive: true });
			const skillPath = join(skillDir, "SKILL.md");
			writeFileSync(skillPath, skillMarkdown(skill), { encoding: "utf-8", mode: 0o600 });
			writeFileSync(join(skillDir, "pyproject.toml"), pyproject(skill), { encoding: "utf-8", mode: 0o600 });
			writeFileSync(join(packageDir, "__init__.py"), pythonModule(skill.server, skill.config, skill.tool), {
				encoding: "utf-8",
				mode: 0o600,
			});
			skillPaths.push(skillPath);
		}
		try {
			await this.host.extendTemporarySkills(skillPaths, "acp:mcp");
		} catch (error) {
			if (nextRoot) rmSync(nextRoot, { recursive: true, force: true });
			throw error;
		}
		if (this.root) rmSync(this.root, { recursive: true, force: true });
		this.root = nextRoot;
		this.installedSkillNames = new Set(discovered.map((skill) => skillDirectoryName(skill.importName)));
	}

	dispose(): void {
		if (!this.root) return;
		rmSync(this.root, { recursive: true, force: true });
		this.root = undefined;
		this.installedSkillNames.clear();
	}
}
