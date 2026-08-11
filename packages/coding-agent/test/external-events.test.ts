import { describe, expect, it, vi } from "vitest";
import {
	createExternalEventHostHandler,
	EXTERNAL_EVENT_MAX_RETAINED_IDS,
	EXTERNAL_EVENT_MAX_TEXT_CHARS,
	ExternalEventRegistry,
} from "../src/core/external-events.js";

describe("external event host registry", () => {
	it("coalesces by producer name and stable event ID", async () => {
		const registry = new ExternalEventRegistry();
		const emit = vi.fn(async () => "queued" as const);
		const handler = createExternalEventHostHandler(registry, emit);
		const payload = {
			name: "matrix",
			event_id: "$event operator",
			text: "operator message",
			delivery_policy: "steer",
		};

		await expect(handler(payload)).resolves.toMatchObject({ deliveryStatus: "queued" });
		await expect(handler({ ...payload, text: "duplicate" })).resolves.toMatchObject({
			deliveryStatus: "coalesced",
		});
		await expect(handler({ ...payload, name: "other" })).resolves.toMatchObject({ deliveryStatus: "queued" });
		await expect(handler({ ...payload, name: "matrix $event", event_id: "operator" })).resolves.toMatchObject({
			deliveryStatus: "queued",
		});
		expect(emit).toHaveBeenCalledTimes(3);
	});

	it("rejects malformed or oversized inputs before admission", async () => {
		const registry = new ExternalEventRegistry();
		const emit = vi.fn(async () => "delivered" as const);
		const handler = createExternalEventHostHandler(registry, emit);
		const payload = { name: "watch", event_id: "event", text: "message", delivery_policy: "followUp" };

		await expect(handler({ ...payload, name: " " })).rejects.toThrow("name cannot be empty");
		await expect(handler({ ...payload, text: "x".repeat(EXTERNAL_EVENT_MAX_TEXT_CHARS + 1) })).rejects.toThrow(
			"text is too long",
		);
		await expect(handler({ ...payload, delivery_policy: "next" })).rejects.toThrow("delivery_policy must be");
		expect(emit).not.toHaveBeenCalled();
	});

	it("bounds retained IDs and rejects admission after disposal", () => {
		const registry = new ExternalEventRegistry();
		for (let index = 0; index <= EXTERNAL_EVENT_MAX_RETAINED_IDS; index++) {
			expect(registry.admit("watch", String(index))).toBe(true);
		}
		expect(registry.size).toBe(EXTERNAL_EVENT_MAX_RETAINED_IDS);
		expect(registry.admit("watch", "0")).toBe(true);
		registry.dispose();
		expect(registry.size).toBe(0);
		expect(() => registry.admit("watch", "later")).toThrow("session was disposed");
	});

	it("forgets failed admissions so callers can retry", async () => {
		const registry = new ExternalEventRegistry();
		const emit = vi.fn().mockRejectedValueOnce(new Error("busy")).mockResolvedValueOnce("delivered");
		const handler = createExternalEventHostHandler(registry, emit);
		const payload = { name: "watch", event_id: "retry", text: "event", delivery_policy: "followUp" };

		await expect(handler(payload)).rejects.toThrow("busy");
		await expect(handler(payload)).resolves.toMatchObject({ deliveryStatus: "delivered" });
		expect(emit).toHaveBeenCalledTimes(2);
	});
});
