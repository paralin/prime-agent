import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

const server = new Server(
	{ name: "acp-mcp-stdio-fixture", version: "1.0.0" },
	{ capabilities: { tools: {} } },
);

server.setRequestHandler(ListToolsRequestSchema, async () => ({
	tools: [
		{
			name: `env_${process.env.TASK_MARKER ?? "missing"}`,
			description: `Discovered in ${process.cwd()}`,
			inputSchema: { type: "object", properties: {} },
		},
	],
}));

await server.connect(new StdioServerTransport());
