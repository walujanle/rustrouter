import { defineStore } from "pinia";

/**
 * Global toast notification system. Centralized feedback for dashboard actions.
 */

export interface Notification {
	id: number;
	type: string;
	message: string;
	title: string | null;
	duration: number;
	dismissible: boolean;
	createdAt: number;
}

let idCounter = 0;

export const useNotificationStore = defineStore("notification", {
	state: () => ({
		notifications: [] as Notification[],
	}),
	actions: {
		addNotification(notification: {
			type?: string;
			message: string;
			title?: string | null;
			duration?: number;
			dismissible?: boolean;
		}): number {
			const id = ++idCounter;
			const entry: Notification = {
				id,
				type: notification.type || "info",
				message: notification.message,
				title: notification.title || null,
				duration: notification.duration ?? 5000,
				dismissible: notification.dismissible ?? true,
				createdAt: Date.now(),
			};
			this.notifications.push(entry);
			if (entry.duration > 0) {
				setTimeout(() => this.removeNotification(id), entry.duration);
			}
			return id;
		},
		removeNotification(id: number) {
			this.notifications = this.notifications.filter((n) => n.id !== id);
		},
		clearAll() {
			this.notifications = [];
		},
		success(message: string, title?: string) {
			return this.addNotification({ type: "success", message, title });
		},
		error(message: string, title?: string) {
			return this.addNotification({
				type: "error",
				message,
				title,
				duration: 8000,
			});
		},
		warning(message: string, title?: string) {
			return this.addNotification({ type: "warning", message, title });
		},
		info(message: string, title?: string) {
			return this.addNotification({ type: "info", message, title });
		},
	},
});
