// Match a configured CLI base URL against the known endpoints. Only a local
// endpoint counts as "configured".
const stripTrailingSlash = (s: string) => (s || "").replace(/\/+$/, "");

export function matchKnownEndpoint(currentUrl: string): boolean {
	if (!currentUrl) return false;
	const url = stripTrailingSlash(currentUrl);
	return /localhost|127\.0\.0\.1|0\.0\.0\.0/.test(url);
}
