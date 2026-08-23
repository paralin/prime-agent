import { closeSync, createReadStream, openSync, readSync } from "node:fs";

export function readFirstLineSync(filePath: string, maxBytes = 64 * 1024): string | undefined {
	const fd = openSync(filePath, "r");
	const chunks: Buffer[] = [];
	let position = 0;

	try {
		const buffer = Buffer.alloc(1024);
		while (position < maxBytes) {
			const bytesToRead = Math.min(buffer.length, maxBytes - position);
			const bytesRead = readSync(fd, buffer, 0, bytesToRead, position);
			if (bytesRead === 0) {
				break;
			}

			const chunk = buffer.subarray(0, bytesRead);
			const newlineIndex = chunk.indexOf(0x0a);
			if (newlineIndex !== -1) {
				chunks.push(Buffer.from(chunk.subarray(0, newlineIndex)));
				return Buffer.concat(chunks).toString("utf8").replace(/\r$/, "");
			}

			chunks.push(Buffer.from(chunk));
			position += bytesRead;
		}
	} finally {
		closeSync(fd);
	}

	if (chunks.length === 0) {
		return undefined;
	}
	return Buffer.concat(chunks).toString("utf8").replace(/\r$/, "");
}

export function readLineContainingSync(filePath: string, fragments: readonly string[]): string | undefined {
	const fd = openSync(filePath, "r");
	const needles = fragments.map((fragment) => Buffer.from(fragment));
	let pending = Buffer.alloc(0);

	try {
		const buffer = Buffer.alloc(64 * 1024);
		while (true) {
			const bytesRead = readSync(fd, buffer, 0, buffer.length, null);
			if (bytesRead === 0) break;
			const chunk =
				pending.length > 0
					? Buffer.concat([pending, buffer.subarray(0, bytesRead)])
					: buffer.subarray(0, bytesRead);
			let start = 0;
			while (start < chunk.length) {
				const end = chunk.indexOf(0x0a, start);
				if (end === -1) break;
				const line = chunk.subarray(start, end);
				if (needles.every((needle) => line.includes(needle))) return line.toString("utf8").replace(/\r$/, "");
				start = end + 1;
			}
			pending = Buffer.from(chunk.subarray(start));
		}
		if (pending.length > 0 && needles.every((needle) => pending.includes(needle))) {
			return pending.toString("utf8").replace(/\r$/, "");
		}
		return undefined;
	} finally {
		closeSync(fd);
	}
}

export async function* readLinesAsBuffers(
	filePath: string,
	options: { start?: number; end?: number } = {},
): AsyncGenerator<Buffer> {
	const pendingParts: Buffer[] = [];
	let pendingBytes = 0;
	for await (const chunk of createReadStream(filePath, options)) {
		const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
		let start = 0;
		while (start < buffer.length) {
			const end = buffer.indexOf(0x0a, start);
			if (end === -1) {
				const part = buffer.subarray(start);
				pendingParts.push(part);
				pendingBytes += part.length;
				break;
			}
			if (pendingParts.length > 0) {
				const part = buffer.subarray(start, end);
				pendingParts.push(part);
				const line = Buffer.concat(pendingParts, pendingBytes + part.length);
				pendingParts.length = 0;
				pendingBytes = 0;
				yield line;
			} else {
				yield buffer.subarray(start, end);
			}
			start = end + 1;
		}
	}
	if (pendingParts.length > 0) {
		const line = Buffer.concat(pendingParts, pendingBytes);
		pendingParts.length = 0;
		pendingBytes = 0;
		yield line;
	}
}
