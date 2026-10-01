/**
 * Shared overlay stack for modals and drawers.
 *
 * Every open overlay registers itself; body scroll only unlocks when the last
 * one closes, and Escape/Tab only reach the topmost. Without this a nested
 * overlay restores scrolling underneath its parent and one Escape closes both.
 */

type Overlay = {
	token: symbol;
	onEscape: () => void;
	container: () => HTMLElement | null;
};

const stack: Overlay[] = [];

function focusables(root: HTMLElement | null): HTMLElement[] {
	if (!root) return [];
	return Array.from(
		root.querySelectorAll<HTMLElement>(
			'a[href], button:not([disabled]), textarea:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])',
		),
	).filter((el) => el.offsetParent !== null);
}

function onKeydown(e: KeyboardEvent) {
	const top = stack[stack.length - 1];
	if (!top) return;
	if (e.key === "Escape") {
		top.onEscape();
		return;
	}
	if (e.key !== "Tab") return;
	const items = focusables(top.container());
	if (items.length === 0) {
		e.preventDefault();
		return;
	}
	const first = items[0];
	const last = items[items.length - 1];
	const active = document.activeElement;
	if (e.shiftKey && (active === first || active === top.container())) {
		e.preventDefault();
		last.focus();
	} else if (!e.shiftKey && active === last) {
		e.preventDefault();
		first.focus();
	}
}

export function pushOverlay(overlay: Omit<Overlay, "token">): symbol {
	const token = Symbol("overlay");
	stack.push({ ...overlay, token });
	document.body.style.overflow = "hidden";
	document.addEventListener("keydown", onKeydown);
	return token;
}

export function popOverlay(token: symbol) {
	const index = stack.findIndex((o) => o.token === token);
	if (index === -1) return;
	stack.splice(index, 1);
	if (stack.length === 0) {
		document.removeEventListener("keydown", onKeydown);
		document.body.style.overflow = "";
	}
}
