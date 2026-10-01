import { onBeforeUnmount, ref } from "vue";

/** Copy-to-clipboard with a per-target "copied" flag that resets after `resetDelay` ms. */
export function useCopyToClipboard(resetDelay = 2000) {
	const copied = ref<string | null>(null);
	let timer: ReturnType<typeof setTimeout> | null = null;

	async function write(text: string) {
		if (navigator?.clipboard?.writeText) {
			await navigator.clipboard.writeText(text);
			return;
		}
		const textarea = document.createElement("textarea");
		textarea.value = text;
		textarea.style.position = "fixed";
		textarea.style.opacity = "0";
		document.body.appendChild(textarea);
		textarea.select();
		document.execCommand("copy");
		document.body.removeChild(textarea);
	}

	function copy(text: string, id = "default") {
		write(text).catch(() => {});
		copied.value = id;
		if (timer) clearTimeout(timer);
		timer = setTimeout(() => {
			copied.value = null;
		}, resetDelay);
	}

	onBeforeUnmount(() => {
		if (timer) clearTimeout(timer);
	});

	return { copied, copy };
}
