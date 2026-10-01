/** API utility functions for making HTTP requests. */

const DEFAULT_HEADERS = {
	"Content-Type": "application/json",
};

/** Requests that hang longer than this are aborted; the router guard awaits
 * these on every dashboard navigation, so an unresponsive backend must fail
 * rather than leave the SPA blank. */
const DEFAULT_TIMEOUT_MS = 30_000;

type RequestOptions = RequestInit & {
	headers?: Record<string, string>;
	timeoutMs?: number;
	/** Opt-in GET cache lifetime in ms. `0` (the default) disables caching but
	 * still dedups concurrent identical GETs. */
	cacheMs?: number;
};

/**
 * Concurrent identical GETs share one request. Two components mounting in the
 * same tick (a layout and its page, a page and its child) otherwise fire the
 * same request twice and the second response races the first.
 */
const inflight = new Map<string, Promise<unknown>>();

/** Opt-in TTL cache, keyed by URL. Only URLs whose caller passed `cacheMs`. */
const getCache = new Map<string, { expires: number; value: unknown }>();

/**
 * Drop cached GETs. Call after a mutation that a cached URL would reflect, or
 * pass a prefix to drop just that subtree.
 */
export function invalidateCache(urlPrefix = ""): void {
	if (!urlPrefix) {
		getCache.clear();
		return;
	}
	for (const key of getCache.keys()) {
		if (key.startsWith(urlPrefix)) getCache.delete(key);
	}
}

async function handleResponse(response: Response): Promise<unknown> {
	const text = await response.text();
	let data: unknown = null;
	if (text) {
		try {
			data = JSON.parse(text);
		} catch {
			data = { error: text };
		}
	}
	if (!response.ok) {
		const error = new Error(
			(data as { error?: string })?.error || "An error occurred",
		) as Error & { status?: number; data?: unknown };
		error.status = response.status;
		error.data = data;
		throw error;
	}
	return data;
}

async function request(
	url: string,
	options: RequestOptions,
	init: RequestInit,
): Promise<unknown> {
	const { timeoutMs = DEFAULT_TIMEOUT_MS, headers, ...rest } = options;
	const controller = new AbortController();
	const timer = setTimeout(() => controller.abort(), timeoutMs);
	try {
		const response = await fetch(url, {
			...init,
			...rest,
			headers: { ...DEFAULT_HEADERS, ...headers },
			signal: controller.signal,
		});
		return await handleResponse(response);
	} finally {
		clearTimeout(timer);
	}
}

export async function get(
	url: string,
	options: RequestOptions = {},
): Promise<unknown> {
	const { cacheMs = 0, ...rest } = options;

	if (cacheMs > 0) {
		const hit = getCache.get(url);
		if (hit && hit.expires > Date.now()) return hit.value;
	}

	const existing = inflight.get(url);
	if (existing) return existing;

	const pending = request(url, rest, { method: "GET" })
		.then((value) => {
			if (cacheMs > 0)
				getCache.set(url, { expires: Date.now() + cacheMs, value });
			return value;
		})
		.finally(() => {
			inflight.delete(url);
		});

	inflight.set(url, pending);
	return pending;
}

export async function post(
	url: string,
	data: unknown,
	options: RequestOptions = {},
): Promise<unknown> {
	return request(url, options, {
		method: "POST",
		body: JSON.stringify(data),
	});
}

export async function put(
	url: string,
	data: unknown,
	options: RequestOptions = {},
): Promise<unknown> {
	return request(url, options, {
		method: "PUT",
		body: JSON.stringify(data),
	});
}

export async function del(
	url: string,
	options: RequestOptions = {},
): Promise<unknown> {
	return request(url, options, { method: "DELETE" });
}

export async function patch(
	url: string,
	data: unknown,
	options: RequestOptions = {},
): Promise<unknown> {
	return request(url, options, {
		method: "PATCH",
		body: JSON.stringify(data),
	});
}

const api = { get, post, put, del, patch };
export default api;
