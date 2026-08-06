import { once } from "node:events";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import * as acp from "@agentclientprotocol/sdk";
import { describe, expect, it, vi } from "vitest";
import type { AgentSessionRuntime } from "../src/core/agent-session-runtime.js";
import { DefaultResourceLoader } from "../src/core/resource-loader.js";
import { AcpMcpSkillInstaller } from "../src/modes/acp/acp-mcp.js";
import { runAcpModeWithConnection } from "../src/modes/acp/acp-mode.js";
import { InProcessAgentConnection } from "../src/modes/agent-connection/in-process-agent-connection.js";
import { createHarness } from "./suite/harness.js";

function runtimeHostFor(session: unknown): AgentSessionRuntime {
	return {
		session,
		setRebindSession() {},
		setBeforeSessionInvalidate() {},
		async dispose() {},
	} as unknown as AgentSessionRuntime;
}

async function startHttpMcpServer(): Promise<{
	url: string;
	authorization: () => string | undefined;
	close: () => Promise<void>;
}> {
	let authorization: string | undefined;
	const server = createServer(async (request, response) => {
		authorization = request.headers.authorization;
		if (request.method === "DELETE") {
			response.writeHead(200).end();
			return;
		}
		if (request.method !== "POST") {
			response.writeHead(405).end();
			return;
		}
		const chunks: Buffer[] = [];
		for await (const chunk of request) {
			chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
		}
		const message = JSON.parse(Buffer.concat(chunks).toString("utf-8")) as {
			jsonrpc: "2.0";
			id?: string | number;
			method: string;
			params?: { protocolVersion?: string };
		};
		if (message.id === undefined) {
			response.writeHead(202).end();
			return;
		}
		const result =
			message.method === "initialize"
				? {
						protocolVersion: message.params?.protocolVersion ?? "2025-06-18",
						capabilities: { tools: {} },
						serverInfo: { name: "test-tools", version: "1.0.0" },
					}
				: {
						tools: [
							{
								name: "lookup",
								description: "Look up a task by query.",
								inputSchema: {
									type: "object",
									properties: { query: { type: "string" }, limit: { type: "integer" } },
									required: ["query"],
								},
							},
						],
					};
		response.writeHead(200, { "Content-Type": "application/json" });
		response.end(JSON.stringify({ jsonrpc: "2.0", id: message.id, result }));
	});
	server.listen(0, "127.0.0.1");
	await once(server, "listening");
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("test MCP server did not bind TCP");
	return {
		url: `http://127.0.0.1:${address.port}/mcp`,
		authorization: () => authorization,
		close: async () => {
			server.close();
			await once(server, "close");
		},
	};
}

describe("ACP MCP servers", () => {
	it("advertises HTTP support and configures session/new servers in the agent cwd", async () => {
		const harness = await createHarness();
		const connection = new InProcessAgentConnection(runtimeHostFor(harness.session));
		const agentCwd = (await connection.getState()).cwd;
		const configureMcpServers = vi.fn();
		const toAgent = new TransformStream<Uint8Array, Uint8Array>();
		const toClient = new TransformStream<Uint8Array, Uint8Array>();
		const modeDone = runAcpModeWithConnection(connection, {
			stream: acp.ndJsonStream(toClient.writable, toAgent.readable),
			configureMcpServers,
		});
		const handle = acp
			.client({ name: "mcp-test-client" })
			.connect(acp.ndJsonStream(toAgent.writable, toClient.readable));

		const initialized = await handle.agent.request("initialize", {
			protocolVersion: acp.PROTOCOL_VERSION,
			clientCapabilities: {},
		});
		expect(initialized.agentCapabilities?.mcpCapabilities?.http).toBe(true);

		const server: acp.McpServer = {
			type: "http",
			name: "task-tools",
			url: "http://127.0.0.1:8000/mcp",
			headers: [{ name: "Authorization", value: "Bearer task" }],
		};
		const firstSession = await handle.agent.request("session/new", {
			cwd: harness.tempDir,
			mcpServers: [server],
		});
		expect(configureMcpServers).toHaveBeenCalledWith([server], agentCwd);
		await handle.agent.request("session/close", { sessionId: firstSession.sessionId });
		await handle.agent.request("session/new", { cwd: harness.tempDir, mcpServers: [] });
		expect(configureMcpServers).toHaveBeenLastCalledWith([], agentCwd);

		handle.close();
		await toAgent.writable.close().catch(() => undefined);
		await modeDone;
		harness.cleanup();
	}, 30_000);

	it("eagerly discovers HTTP tools and writes one Python skill per tool", async () => {
		const mcp = await startHttpMcpServer();
		const extendTemporarySkills = vi.fn();
		const installer = new AcpMcpSkillInstaller({ extendTemporarySkills });
		try {
			await installer.configure(
				[
					{
						type: "http",
						name: "task-tools",
						url: mcp.url,
						headers: [{ name: "Authorization", value: "Bearer task" }],
					},
				],
				process.cwd(),
			);

			expect(mcp.authorization()).toBe("Bearer task");
			expect(extendTemporarySkills).toHaveBeenCalledOnce();
			const [skillPaths] = extendTemporarySkills.mock.calls[0] as [string[], string];
			expect(skillPaths).toHaveLength(1);
			const skillDir = dirname(skillPaths[0]);
			expect(basename(skillDir)).toBe("task-tools-lookup");
			const skill = readFileSync(skillPaths[0], "utf-8");
			expect(skill).toContain("await task_tools_lookup(...)");
			const source = readFileSync(join(skillDir, "src", "task_tools_lookup", "__init__.py"), "utf-8");
			expect(source).toContain("make_acp_mcp_skill");
			expect(source).toContain("Look up a task by query.");
			expect(source).toContain("Bearer task");
		} finally {
			installer.dispose();
			await mcp.close();
		}
	});

	it("eagerly discovers stdio tools in the session cwd and environment", async () => {
		const root = mkdtempSync(join(tmpdir(), "prime-agent-acp-mcp-stdio-test-"));
		const extendTemporarySkills = vi.fn();
		const installer = new AcpMcpSkillInstaller({ extendTemporarySkills });
		const fixture = fileURLToPath(new URL("./fixtures/acp-mcp-stdio-server.mjs", import.meta.url));
		try {
			await installer.configure(
				[
					{
						name: "local-tools",
						command: process.execPath,
						args: [fixture],
						env: [{ name: "TASK_MARKER", value: "stdio" }],
					},
				],
				root,
			);

			const [skillPaths] = extendTemporarySkills.mock.calls[0] as [string[], string];
			expect(skillPaths).toHaveLength(1);
			expect(basename(dirname(skillPaths[0]))).toBe("local-tools-env-stdio");
			const skill = readFileSync(skillPaths[0], "utf-8");
			expect(skill).toContain(`Discovered in ${realpathSync(root)}`);
			const source = readFileSync(
				join(dirname(skillPaths[0]), "src", "local_tools_env_stdio", "__init__.py"),
				"utf-8",
			);
			expect(source).toContain("TASK_MARKER");
			expect(source).toContain("stdio");
		} finally {
			installer.dispose();
			rmSync(root, { recursive: true, force: true });
		}
	});

	it("rejects MCP tools whose RLM-style Python names collide", async () => {
		const extendTemporarySkills = vi.fn();
		const installer = new AcpMcpSkillInstaller({ extendTemporarySkills }, async (server) => [
			{
				name: server === "a" ? "b-c" : "c",
				description: "collision",
				inputSchema: { type: "object" },
			},
		]);
		await expect(
			installer.configure(
				[
					{ type: "http", name: "a", url: "https://a.test/mcp", headers: [] },
					{ type: "http", name: "a-b", url: "https://b.test/mcp", headers: [] },
				],
				process.cwd(),
			),
		).rejects.toMatchObject({
			data: { reason: expect.stringContaining("normalize to the same Python skill a_b_c") },
		});
		expect(extendTemporarySkills).not.toHaveBeenCalled();
		installer.dispose();
	});

	it("rejects an MCP tool that conflicts with an installed Python skill", async () => {
		const extendTemporarySkills = vi.fn();
		const installer = new AcpMcpSkillInstaller(
			{
				extendTemporarySkills,
				getSkillNames: async () => ["task-tools-lookup"],
			},
			async () => [
				{
					name: "lookup",
					description: "collision",
					inputSchema: { type: "object" },
				},
			],
		);
		await expect(
			installer.configure(
				[{ type: "http", name: "task-tools", url: "https://tools.test/mcp", headers: [] }],
				process.cwd(),
			),
		).rejects.toMatchObject({
			data: { reason: "MCP tool task-tools/lookup conflicts with existing skill task-tools-lookup" },
		});
		expect(extendTemporarySkills).not.toHaveBeenCalled();
		installer.dispose();
	});

	it("replaces MCP skills when a new ACP session supplies a different server set", async () => {
		const resourceRoot = mkdtempSync(join(tmpdir(), "prime-agent-acp-mcp-replace-test-"));
		const resourceLoader = new DefaultResourceLoader({
			cwd: resourceRoot,
			agentDir: resourceRoot,
			bundledSkillsDir: null,
		});
		await resourceLoader.reload();
		const harness = await createHarness({ resourceLoader });
		const installer = new AcpMcpSkillInstaller(harness.session, async (server) => [
			{
				name: "lookup",
				description: `Look up data from ${server}.`,
				inputSchema: { type: "object" },
			},
		]);
		const connection = new InProcessAgentConnection(runtimeHostFor(harness.session));
		try {
			await installer.configure(
				[{ type: "http", name: "first", url: "https://first.test/mcp", headers: [] }],
				harness.tempDir,
			);
			expect((await connection.getResourceSnapshot()).skills.map((skill) => skill.name)).toContain("first-lookup");

			await installer.configure(
				[{ type: "http", name: "second", url: "https://second.test/mcp", headers: [] }],
				harness.tempDir,
			);
			const replaced = (await connection.getResourceSnapshot()).skills.map((skill) => skill.name);
			expect(replaced).not.toContain("first-lookup");
			expect(replaced).toContain("second-lookup");

			await installer.configure([], harness.tempDir);
			expect((await connection.getResourceSnapshot()).skills.map((skill) => skill.name)).not.toContain(
				"second-lookup",
			);
		} finally {
			installer.dispose();
			await connection.dispose();
			harness.cleanup();
			rmSync(resourceRoot, { recursive: true, force: true });
		}
	});

	it(
		"pre-imports eagerly discovered MCP tools with RLM-style signatures",
		{ tags: ["kernel-heavy"], timeout: 180_000 },
		async () => {
			const resourceRoot = mkdtempSync(join(tmpdir(), "prime-agent-acp-mcp-test-"));
			const resourceLoader = new DefaultResourceLoader({
				cwd: resourceRoot,
				agentDir: resourceRoot,
				bundledSkillsDir: null,
			});
			await resourceLoader.reload();
			const harness = await createHarness({ resourceLoader });
			const discoverTools = vi.fn(async (server: string) => [
				{
					name: server === "remote-tools" ? "lookup" : "run_job",
					description: server === "remote-tools" ? "Look up a task." : "Run a local job.",
					inputSchema:
						server === "remote-tools"
							? {
									type: "object" as const,
									properties: { query: { type: "string" }, limit: { type: "integer" } },
									required: ["query"],
								}
							: { type: "object" as const },
				},
			]);
			const installer = new AcpMcpSkillInstaller(harness.session, discoverTools);
			try {
				await installer.configure(
					[
						{
							type: "http",
							name: "remote-tools",
							url: "https://tools.example/mcp",
							headers: [{ name: "X-Task", value: "one" }],
						},
						{
							name: "local-tools",
							command: "/usr/bin/tool-server",
							args: ["--stdio"],
							env: [{ name: "TASK", value: "two" }],
						},
					],
					resourceRoot,
				);

				const connection = new InProcessAgentConnection(runtimeHostFor(harness.session));
				const resources = await connection.getResourceSnapshot();
				const remoteSkill = resources.skills.find((candidate) => candidate.name === "remote-tools-lookup");
				const localSkill = resources.skills.find((candidate) => candidate.name === "local-tools-run-job");
				expect(remoteSkill).toBeDefined();
				expect(localSkill).toBeDefined();
				expect(harness.session.systemPrompt).toContain("remote_tools_lookup");
				expect(harness.session.systemPrompt).toContain("local_tools_run_job");

				const source = readFileSync(
					join(dirname(remoteSkill!.filePath), "src", "remote_tools_lookup", "__init__.py"),
					"utf-8",
				);
				expect(source).toContain("https://tools.example/mcp");
				expect(source).toContain("X-Task");
				const localSource = readFileSync(
					join(dirname(localSkill!.filePath), "src", "local_tools_run_job", "__init__.py"),
					"utf-8",
				);
				expect(localSource).toContain("/usr/bin/tool-server");
				expect(localSource).toContain("TASK");
				const pyproject = readFileSync(join(dirname(remoteSkill!.filePath), "pyproject.toml"), "utf-8");
				expect(pyproject).not.toContain("prime-agent-runtime");

				const ipython = harness.session.agent.state.tools.find((tool) => tool.name === "ipython");
				expect(ipython).toBeDefined();
				const imported = await ipython!.execute("acp-mcp-import", {
					code: [
						"import inspect",
						"print(remote_tools_lookup.__name__)",
						"print(inspect.signature(remote_tools_lookup))",
						"print(remote_tools_lookup.__doc__)",
					].join("\n"),
				});
				const output = imported.content
					.filter((item): item is { type: "text"; text: string } => item.type === "text")
					.map((item) => item.text)
					.join("");
				expect(output).toContain("remote_tools_lookup");
				expect(output).toContain("query: str");
				expect(output).toContain("limit: int = None");
				expect(output).toContain("Look up a task.");

				const skillDirectory = dirname(remoteSkill!.filePath);
				await harness.session.disposeAsync();
				installer.dispose();
				expect(existsSync(skillDirectory)).toBe(false);
			} finally {
				await harness.session.disposeAsync();
				installer.dispose();
				harness.cleanup();
				rmSync(resourceRoot, { recursive: true, force: true });
			}
		},
	);
});
