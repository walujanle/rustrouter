// Agent Skills metadata — single source of truth for /dashboard/skills page.
// Each skill = 1 URL served by this app (`GET /api/skills/{id}/SKILL.md`) that
// the user copies and pastes to any AI agent. The URL is built from the browser
// origin, so it always points at the server the dashboard is open on rather than
// at an upstream GitHub repo this project does not own.

export interface Skill {
	id: string;
	name: string;
	description: string;
	endpoint?: string | null;
	icon: string;
	isEntry?: boolean;
}

export const SKILLS: Skill[] = [
	{
		id: "9router",
		name: "9Router (Entry)",
		description:
			"Setup + index of all capabilities. Start here — covers base URL, auth, model discovery, and links to every capability skill.",
		endpoint: null,
		icon: "hub",
		isEntry: true,
	},
	{
		id: "9router-chat",
		name: "Chat",
		description:
			"Chat / code-gen via OpenAI or Anthropic format with streaming.",
		endpoint: "/v1/chat/completions",
		icon: "chat",
	},
	{
		id: "9router-embeddings",
		name: "Embeddings",
		description:
			"Vectors for RAG / semantic search via Mistral, NVIDIA or OpenRouter.",
		endpoint: "/v1/embeddings",
		icon: "scatter_plot",
	},
	{
		id: "9router-web-search",
		name: "Web Search",
		description: "Web search via Tavily, Exa, Brave Search, Linkup or You.com.",
		endpoint: "/v1/search",
		icon: "search",
	},
	{
		id: "9router-web-fetch",
		name: "Web Fetch",
		description: "URL to markdown / text / HTML via Firecrawl, Tavily or Exa.",
		endpoint: "/v1/web/fetch",
		icon: "language",
	},
];

/** The origin the dashboard is served on, with a dev fallback. */
export function skillOrigin(): string {
	if (typeof window !== "undefined") return window.location.origin;
	return "http://localhost:20129";
}

/** The app-served markdown URL for one skill. */
export function getSkillUrl(id: string): string {
	return `${skillOrigin()}/api/skills/${id}/SKILL.md`;
}
