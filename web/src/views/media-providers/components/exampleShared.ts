// Example request/response configs for the media kinds this build serves:
// `embedding`, `webSearch` and `webFetch`.

export interface ExampleField {
	key: string;
	label: string;
	type: "select" | "text" | "number";
	default: string | number;
	options?: string[];
	min?: number;
	max?: number;
	placeholder?: string;
}

export interface ExampleConfig {
	inputLabel: string;
	inputPlaceholder: string;
	defaultInput: string;
	bodyKey: string;
	defaultResponse: string;
	extraFields?: ExampleField[];
	extraBody?: Record<string, unknown>;
}

export const KIND_EXAMPLE_CONFIG: Record<string, ExampleConfig> = {
	webSearch: {
		inputLabel: "Query",
		inputPlaceholder: "What is the latest news about AI?",
		defaultInput: "What is the latest news about AI?",
		bodyKey: "query",
		defaultResponse: `{\n  "results": [\n    { "title": "...", "url": "...", "snippet": "..." }\n  ]\n}`,
		extraFields: [
			{
				key: "search_type",
				label: "Type",
				type: "select",
				default: "web",
				options: ["web", "news"],
			},
			{
				key: "max_results",
				label: "Max results",
				type: "number",
				default: 5,
				min: 1,
				max: 100,
			},
			{ key: "country", label: "Country", type: "text", default: "" },
			{ key: "language", label: "Language", type: "text", default: "" },
		],
	},
	webFetch: {
		inputLabel: "URL",
		inputPlaceholder: "https://example.com",
		defaultInput: "https://example.com",
		bodyKey: "url",
		defaultResponse: `{\n  "content": "...",\n  "title": "...",\n  "url": "..."\n}`,
		extraFields: [
			{
				key: "format",
				label: "Format",
				type: "select",
				default: "markdown",
				options: ["markdown", "text", "html"],
			},
			{
				key: "max_characters",
				label: "Max chars",
				type: "number",
				default: 0,
				min: 0,
			},
		],
	},
	systemone: {
		inputLabel: "State",
		inputPlaceholder: "Situation, support ticket, or text to evaluate",
		defaultInput:
			"My payments have failed for three days and I am losing sales. Please help now.",
		bodyKey: "state",
		extraBody: {
			questions: {
				is_urgent: {
					type: "noul",
					instructions: "Does this request require urgent attention?",
				},
			},
		},
		defaultResponse: `{\n  "model": "jev-1.13",\n  "answers": {\n    "is_urgent": { "type": "noul", "noul": 0.99 }\n  },\n  "usage": { "input_tokens": 312, "output_tokens": 48 }\n}`,
	},
};
