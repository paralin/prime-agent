import { DEFAULT_AGENT_MESSAGE_MAX_CHARS } from "./agent-messages.js";
import type { HostRequestHandler } from "./kernel/index.js";

export const EXTERNAL_EVENT_HOST_REQUEST_TYPE = "session.external_event.emit";
export const EXTERNAL_EVENT_CUSTOM_TYPE = "external_event";
export const EXTERNAL_EVENT_PREVIEW_LABEL = "External event";
export const EXTERNAL_EVENT_QUEUE_KEY_PREFIX = "external-event:";

export const EXTERNAL_EVENT_MAX_NAME_CHARS = 128;
export const EXTERNAL_EVENT_MAX_ID_CHARS = 512;
export const EXTERNAL_EVENT_MAX_TEXT_CHARS = DEFAULT_AGENT_MESSAGE_MAX_CHARS;
export const EXTERNAL_EVENT_MAX_RETAINED_IDS = 1_024;
export const EXTERNAL_EVENT_MAX_PENDING = 128;

export type ExternalEventDeliveryPolicy = "followUp" | "steer";
export type ExternalEventDeliveryStatus = "coalesced" | "delivered" | "queued";

export interface ExternalEventInput {
	name: string;
	eventId: string;
	text: string;
	deliveryPolicy: ExternalEventDeliveryPolicy;
}

/** Details carried by the `external_event` custom message admitted for one event. */
export interface ExternalEventDetails {
	name: string;
	eventId: string;
}

export interface ExternalEventReceipt {
	accepted: true;
	deliveryStatus: ExternalEventDeliveryStatus;
	name: string;
	eventId: string;
}

/** Coalescing key for one producer event identity while its session action is pending. */
export function externalEventQueueKey(name: string, eventId: string): string {
	return `${EXTERNAL_EVENT_QUEUE_KEY_PREFIX}${JSON.stringify([name, eventId])}`;
}

export function isExternalEventQueueKey(queueKey: string | undefined): boolean {
	return queueKey?.startsWith(EXTERNAL_EVENT_QUEUE_KEY_PREFIX) ?? false;
}

/**
 * Retains admitted producer event IDs so a repeated emit of the same event is
 * dropped rather than re-admitted. Retention is bounded and oldest-first, and
 * ends with the owning session.
 */
export class ExternalEventRegistry {
	private readonly retained = new Set<string>();
	private disposed = false;

	admit(name: string, eventId: string): boolean {
		if (this.disposed) throw new Error("Cannot admit an external event after its session was disposed.");
		const key = this.key(name, eventId);
		if (this.retained.has(key)) return false;
		this.retained.add(key);
		while (this.retained.size > EXTERNAL_EVENT_MAX_RETAINED_IDS) {
			const oldest = this.retained.values().next().value;
			if (oldest === undefined) break;
			this.retained.delete(oldest);
		}
		return true;
	}

	forget(name: string, eventId: string): void {
		this.retained.delete(this.key(name, eventId));
	}

	dispose(): void {
		this.disposed = true;
		this.retained.clear();
	}

	get size(): number {
		return this.retained.size;
	}

	private key(name: string, eventId: string): string {
		return JSON.stringify([name, eventId]);
	}
}

function boundedField(payload: Record<string, unknown>, field: string, maxChars: number): string {
	const value = payload[field];
	if (typeof value !== "string") {
		throw new Error(`${EXTERNAL_EVENT_HOST_REQUEST_TYPE} ${field} must be a string`);
	}
	if (!value.trim()) throw new Error(`${EXTERNAL_EVENT_HOST_REQUEST_TYPE} ${field} cannot be empty`);
	if (value.length > maxChars) {
		throw new Error(`${EXTERNAL_EVENT_HOST_REQUEST_TYPE} ${field} is too long: maximum is ${maxChars} characters`);
	}
	return value;
}

function deliveryPolicy(payload: Record<string, unknown>): ExternalEventDeliveryPolicy {
	const value = payload.delivery_policy;
	if (value !== "followUp" && value !== "steer") {
		throw new Error(`${EXTERNAL_EVENT_HOST_REQUEST_TYPE} delivery_policy must be "followUp" or "steer"`);
	}
	return value;
}

/**
 * Build the typed host request that admits one bounded external event. `emit` owns
 * session admission; a failed emit releases the retained ID so the producer can retry.
 */
export function createExternalEventHostHandler(
	registry: ExternalEventRegistry,
	emit: (input: ExternalEventInput) => Promise<ExternalEventDeliveryStatus>,
): HostRequestHandler {
	return async (payload) => {
		const name = boundedField(payload, "name", EXTERNAL_EVENT_MAX_NAME_CHARS).trim();
		const eventId = boundedField(payload, "event_id", EXTERNAL_EVENT_MAX_ID_CHARS).trim();
		// Text stays verbatim: an operator event carrying leading slashes or whitespace
		// must reach the session exactly as the producer sent it.
		const text = boundedField(payload, "text", EXTERNAL_EVENT_MAX_TEXT_CHARS);
		const policy = deliveryPolicy(payload);
		if (!registry.admit(name, eventId)) {
			return { accepted: true, deliveryStatus: "coalesced", name, eventId } satisfies ExternalEventReceipt;
		}
		try {
			const deliveryStatus = await emit({ name, eventId, text, deliveryPolicy: policy });
			return { accepted: true, deliveryStatus, name, eventId } satisfies ExternalEventReceipt;
		} catch (error) {
			registry.forget(name, eventId);
			throw error;
		}
	};
}
