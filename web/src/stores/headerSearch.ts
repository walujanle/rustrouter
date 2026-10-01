import { defineStore } from "pinia";

/**
 * Reusable search input in the Header. Pages register a placeholder on mount,
 * read the query, and unregister on unmount.
 */
export const useHeaderSearchStore = defineStore("headerSearch", {
	state: () => ({
		query: "",
		placeholder: "",
		visible: false,
	}),
	actions: {
		setQuery(query: string) {
			this.query = query;
		},
		register(placeholder = "Search...") {
			this.visible = true;
			this.placeholder = placeholder;
			this.query = "";
		},
		unregister() {
			this.visible = false;
			this.placeholder = "";
			this.query = "";
		},
	},
});
