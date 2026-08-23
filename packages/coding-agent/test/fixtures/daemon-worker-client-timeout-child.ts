import { DaemonWorkerClient } from "../../src/modes/daemon/daemon-worker-client.js";

// Supervisor-equivalent rejection policy: any rejection that escapes request
// handling kills this process, exactly like the daemon supervisor hook.
process.on("unhandledRejection", () => {
	process.stdout.write("CHILD_UNHANDLED_REJECTION\n");
	process.exit(1);
});

async function main(): Promise<void> {
	const client = new DaemonWorkerClient("/tmp/daemon-worker-client-process-gate.sock");
	let blockSend = true;
	(client as unknown as { socket: unknown }).socket = { destroyed: false, destroy: () => {} };
	(client as unknown as { channel: unknown }).channel = {
		async send(header: { requestId: string }): Promise<void> {
			if (blockSend) return new Promise<void>(() => {});
			setImmediate(() => {
				(client as unknown as { handleFrame(frame: unknown): void }).handleFrame({
					header: { kind: "outbound", outboundType: "response", requestId: header.requestId },
					payload: Buffer.from(
						JSON.stringify({
							type: "response",
							command: "list",
							id: header.requestId,
							success: true,
							data: { ok: true },
						}),
					),
				});
			});
		},
		close: () => {},
	};

	// Phase 1: the send stays backpressured past the response timeout. The
	// caller itself must observe the timeout while the process survives.
	try {
		await client.request({ type: "list" }, 30);
		process.stdout.write("PHASE1_RESOLVED\n");
		return;
	} catch (error) {
		const timedOut = (error as Error).message === "Timed out waiting for daemon worker response to list";
		process.stdout.write(`PHASE1_TIMEOUT=${timedOut}\n`);
	}

	// Phase 2: the same client instance keeps working once the transport moves.
	blockSend = false;
	const response = await client.request({ type: "list" }, 2000);
	const ok =
		response.success && (response.data as { ok?: boolean } | undefined)?.ok === true;
	process.stdout.write(`PHASE2_OK=${ok}\n`);
	process.stdout.write("ALIVE\n");
}

void main();
