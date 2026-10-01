// Bulk-add API-key planner.
//
// The backend upserts apikey connections BY NAME, so a colliding name
// overwrites an existing key instead of inserting a new one. This planner
// gap-fills the smallest free "<base> <n>" against both existing connection
// names and names already assigned earlier in the same batch.
//
// ponytail: only numeric-suffix collision is handled. A user who manually types
// an exact existing non-numbered custom name still hits the backend upsert, but
// bulk auto-naming always appends " <n>", so this path is unreachable from the
// bulk modal.

type ParsedLine = {
	baseName: string;
	apiKey: string;
};

function parseLine(line: string): ParsedLine | null {
	const parts = line.split("|");

	if (parts.length >= 2) {
		const baseName = parts[0].trim();
		const apiKey = parts.slice(1).join("|").trim();
		return { baseName: baseName || "Key", apiKey };
	}

	const apiKey = parts[0].trim();
	return { baseName: "Key", apiKey };
}

export function planBulkAdd(
	lines: string[],
	existingNames: string[] | null | undefined,
): Array<Record<string, any>> {
	const safeExisting = Array.isArray(existingNames) ? existingNames : [];
	const used = new Set(
		safeExisting.map((n) => (typeof n === "string" ? n.toLowerCase() : "")),
	);

	const out: Array<Record<string, any>> = [];
	for (const raw of lines) {
		const line = typeof raw === "string" ? raw.trim() : "";
		if (!line) continue;

		const parsed = parseLine(line);
		if (!parsed?.apiKey) continue;

		const base = parsed.baseName;

		// Gap-fill from 1: smallest free "<base> <n>" not in `used`.
		let idx = 1;
		let name: string;
		for (;;) {
			name = `${base} ${idx}`;
			if (!used.has(name.toLowerCase())) break;
			idx += 1;
		}
		used.add(name.toLowerCase());

		out.push({
			name,
			apiKey: parsed.apiKey,
			skipped: false,
		});
	}
	return out;
}
