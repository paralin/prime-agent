import { describe, expect, onTestFinished, test } from "vitest";
import { DaemonWorkerClient } from "./../src/modes/daemon/daemon-worker-client.js";

// Reproduction for the supervisor crash loop: DaemonWorkerClient.requestWire
// created the response promise and its rejection timer, then awaited
// channel.send before returning. When send backpressured longer than the
// response timeout, the timer rejected a promise that had no handler yet, so
// the rejection surfaced as an unhandledRejection before any caller could
// observe it. The daemon supervisor installs an unhandledRejection hook that
// exits the process, so each occurrence killed the live supervisor.
describe("DaemonWorkerClient.requestWire timeout ordering", () => {
	test("times out through the returned promise without an unhandled rejection", async () => {
		const client = new DaemonWorkerClient("/tmp/daemon-worker-client-send-window-test.sock");
		// Never-settling send stands in for a backpressured worker socket.
		(client as unknown as { socket: unknown }).socket = { destroyed: false, destroy: () => {} };
		(client as unknown as { channel: unknown }).channel = {
			send: () => new Promise<void>(() => {}),
			close: () => {},
		};

		const unhandled: unknown[] = [];
		const capture = (reason: unknown): void => {
			unhandled.push(reason);
		};
		process.on("unhandledRejection", capture);
		onTestFinished(() => {
			process.off("unhandledRejection", capture);
			client.close();
		});

		const request = client.request({ type: "list" }, 25);
		// Attach like a real caller would: handlers exist before the timeout can fire.
		const outcome = request.then(
			() => "resolved",
			(error: Error) => error.message,
		);
		await new Promise((resolve) => setTimeout(resolve, 100));

		expect(unhandled).toEqual([]);
		expect(outcome).resolves.toBe("Timed out waiting for daemon worker response to list");
	});

	test("a completed send never settles the request before the worker responds", async () => {
		const client = new DaemonWorkerClient("/tmp/daemon-worker-client-send-window-test.sock");
		(client as unknown as { socket: unknown }).socket = { destroyed: false, destroy: () => {} };
		let releaseSend: (() => void) | undefined;
		(client as unknown as { channel: unknown }).channel = {
			send: () => new Promise<void>((resolve) => (releaseSend = resolve)),
			close: () => {},
		};

		const unhandled: unknown[] = [];
		const capture = (reason: unknown): void => {
			unhandled.push(reason);
		};
		process.on("unhandledRejection", capture);
		onTestFinished(() => {
			process.off("unhandledRejection", capture);
			client.close();
		});

		const request = client.request({ type: "list" }, 30);
		const settled = request.then(
			() => "resolved",
			(error: Error) => error.message,
		);

		releaseSend?.();
		await new Promise((resolve) => setTimeout(resolve, 10));
		// Send finished but no worker response arrived: the request stays bounded
		// by its timer instead of settling early with an undefined response.
		const pendingAfterSend = await Promise.race([
			settled.then(() => "settled" as const),
			new Promise((resolve) => setTimeout(() => resolve("pending" as const), 5)),
		]);
		expect(pendingAfterSend).toBe("pending");

		expect(await settled).toBe("Timed out waiting for daemon worker response to list");
	});
});
