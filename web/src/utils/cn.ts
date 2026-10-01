// Merge class names; drop falsy values, collapse whitespace.
export function cn(
	...classes: Array<string | false | null | undefined>
): string {
	return classes.filter(Boolean).join(" ").replace(/\s+/g, " ").trim();
}
