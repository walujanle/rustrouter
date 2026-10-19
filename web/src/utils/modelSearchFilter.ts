/**
 * Model and combo search with negation and regex support.
 *
 * Four query shapes are supported, in this order:
 * 1. `/pattern/flags` or `/pattern/` — a full regular expression.
 * 2. `/pattern` — a leading-slash expression that is still being typed.
 * 3. `term -term !term` — space-separated terms; `-` and `!` negate.
 * 4. Anything else — a case-insensitive substring match.
 */

export interface ParsedSearchQuery {
	raw: string;
	isEmpty: boolean;
	isRegex: boolean;
	regex: RegExp | null;
	positiveTerms: string[];
	negativeTerms: string[];
}

/** The empty query: matches everything. */
function emptyQuery(): ParsedSearchQuery {
	return {
		raw: "",
		isEmpty: true,
		isRegex: false,
		regex: null,
		positiveTerms: [],
		negativeTerms: [],
	};
}

export function parseSearchQuery(
	rawQuery: string | null | undefined,
): ParsedSearchQuery {
	if (!rawQuery || typeof rawQuery !== "string") return emptyQuery();

	const trimmed = rawQuery.trim();
	if (!trimmed) return emptyQuery();

	if (trimmed.startsWith("/")) {
		const fullRegexMatch = trimmed.match(/^\/(.+)\/([a-z]*)$/i);
		if (fullRegexMatch) {
			try {
				const regex = new RegExp(fullRegexMatch[1], fullRegexMatch[2] || "i");
				return {
					raw: trimmed,
					isEmpty: false,
					isRegex: true,
					regex,
					positiveTerms: [],
					negativeTerms: [],
				};
			} catch {
				// Invalid flags or syntax: fall through to literal terms.
			}
		} else if (trimmed.length > 1) {
			// Live typing, e.g. `/claude.*`. A still-unclosed group throws and
			// falls through instead of breaking the picker.
			try {
				const regex = new RegExp(trimmed.slice(1), "i");
				return {
					raw: trimmed,
					isEmpty: false,
					isRegex: true,
					regex,
					positiveTerms: [],
					negativeTerms: [],
				};
			} catch {
				// Not yet valid.
			}
		}
	}

	const positiveTerms: string[] = [];
	const negativeTerms: string[] = [];
	for (const token of trimmed.split(/\s+/)) {
		if (!token) continue;
		if ((token.startsWith("!") || token.startsWith("-")) && token.length > 1) {
			negativeTerms.push(token.slice(1).toLowerCase());
		} else {
			positiveTerms.push(token.toLowerCase());
		}
	}

	return {
		raw: trimmed,
		isEmpty: positiveTerms.length === 0 && negativeTerms.length === 0,
		isRegex: false,
		regex: null,
		positiveTerms,
		negativeTerms,
	};
}

/** The strings a target contributes to matching. */
function candidatesOf(target: unknown): string[] {
	if (typeof target === "string") return [target];
	if (Array.isArray(target))
		return target.filter((t): t is string => typeof t === "string");
	if (target && typeof target === "object") {
		const record = target as Record<string, unknown>;
		return [
			record.name,
			record.id,
			record.value,
			record.alias,
			record.provider,
		].filter((t): t is string => typeof t === "string");
	}
	return [];
}

export function matchesSearch(
	target: string | string[] | Record<string, any> | null | undefined,
	query?: string | ParsedSearchQuery | null,
): boolean {
	const parsed =
		typeof query === "string" || !query
			? parseSearchQuery(query)
			: (query as ParsedSearchQuery);
	if (parsed.isEmpty) return true;

	const candidates = candidatesOf(target);
	if (candidates.length === 0) return false;

	const regex = parsed.isRegex ? parsed.regex : null;
	if (regex) {
		const fullText = candidates.join(" ");
		// A negative-lookahead only means something against the joined string;
		// testing it per candidate would always succeed.
		if (/\(\?!|\(\?<!/.test(parsed.raw)) return regex.test(fullText);
		return candidates.some((text) => regex.test(text)) || regex.test(fullText);
	}

	const lower = candidates.map((c) => c.toLowerCase());

	// Any negative hit rejects the target.
	for (const neg of parsed.negativeTerms) {
		if (lower.some((c) => c.includes(neg))) return false;
	}

	// Every positive term must hit at least one candidate.
	for (const pos of parsed.positiveTerms) {
		if (!lower.some((c) => c.includes(pos))) return false;
	}

	return true;
}

export function filterModelsByQuery<T>(
	items: T[],
	query?: string | ParsedSearchQuery | null,
): T[] {
	if (!Array.isArray(items) || items.length === 0) return [];
	const parsed =
		typeof query === "string" || !query
			? parseSearchQuery(query)
			: (query as ParsedSearchQuery);
	if (parsed.isEmpty) return items;
	return items.filter((item) =>
		matchesSearch(item as Record<string, any>, parsed),
	);
}
