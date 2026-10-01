// CLI tool descriptors — the three writers this app installs: claude, codex,
// hermes.

export interface CliToolModel {
	id: string;
	name: string;
	alias: string;
	envKey?: string;
	defaultValue?: string;
}

export interface CliToolRole {
	id: string;
	label: string;
}

export interface CliTool {
	id: string;
	name: string;
	image: string;
	color: string;
	description: string;
	configType: string;
	requiresExternalUrl?: boolean;
	envVars?: Record<string, string>;
	modelAliases?: string[];
	settingsFile?: string;
	defaultModels?: CliToolModel[];
	roles?: CliToolRole[];
}

export const CLI_TOOLS: Record<string, CliTool> = {
	claude: {
		id: "claude",
		name: "Claude Code",
		image: "/providers/claude.png",
		color: "#D97757",
		description: "Anthropic Claude Code CLI",
		configType: "env",
		envVars: {
			baseUrl: "ANTHROPIC_BASE_URL",
			model: "ANTHROPIC_MODEL",
			opusModel: "ANTHROPIC_DEFAULT_OPUS_MODEL",
			sonnetModel: "ANTHROPIC_DEFAULT_SONNET_MODEL",
			fableModel: "ANTHROPIC_DEFAULT_FABLE_MODEL",
			haikuModel: "ANTHROPIC_DEFAULT_HAIKU_MODEL",
		},
		modelAliases: ["default", "sonnet", "opus", "fable", "haiku", "opusplan"],
		settingsFile: "~/.claude/settings.json",
		defaultModels: [
			{
				id: "fable",
				name: "Claude Fable",
				alias: "fable",
				envKey: "ANTHROPIC_DEFAULT_FABLE_MODEL",
				defaultValue: "",
			},
			{
				id: "opus",
				name: "Claude Opus",
				alias: "opus",
				envKey: "ANTHROPIC_DEFAULT_OPUS_MODEL",
				defaultValue: "",
			},
			{
				id: "sonnet",
				name: "Claude Sonnet",
				alias: "sonnet",
				envKey: "ANTHROPIC_DEFAULT_SONNET_MODEL",
				defaultValue: "",
			},
			{
				id: "haiku",
				name: "Claude Haiku",
				alias: "haiku",
				envKey: "ANTHROPIC_DEFAULT_HAIKU_MODEL",
				defaultValue: "",
			},
		],
	},
	codex: {
		id: "codex",
		name: "OpenAI Codex CLI / App",
		image: "/providers/codex.png",
		color: "#10A37F",
		description: "OpenAI Codex CLI",
		configType: "custom",
	},
	hermes: {
		id: "hermes",
		name: "Hermes Agent",
		image: "/providers/hermes.png",
		color: "#8B5CF6",
		description: "Nous Research self-improving AI agent",
		configType: "custom",
		// Model slots Hermes supports besides the default ("model:" block).
		// "default" is not listed — the card renders it as the main model picker.
		roles: [
			{ id: "delegation", label: "Delegation (subagents)" },
			{ id: "vision", label: "Vision" },
			{ id: "web_extract", label: "Web Extract" },
			{ id: "compression", label: "Compression" },
			{ id: "title_generation", label: "Title Generation" },
			{ id: "approval", label: "Approval" },
			{ id: "skills_hub", label: "Skills Hub" },
			{ id: "mcp", label: "MCP" },
			{ id: "memory_query_rewrite", label: "Memory Query Rewrite" },
			{ id: "background_review", label: "Background Review" },
			{ id: "curator", label: "Curator" },
			{ id: "monitor", label: "Monitor" },
		],
	},
};
