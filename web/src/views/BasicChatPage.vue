<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";

import Badge from "@/components/ui/UiBadge.vue";
import Button from "@/components/ui/UiButton.vue";
import { useModels } from "@/constants/models";
import {
	isAnthropicCompatibleProvider,
	isOpenAICompatibleProvider,
} from "@/constants/providers";

const STORAGE_KEYS = {
	sessions: "basic-chat.sessions",
	activeSessionId: "basic-chat.activeSessionId",
	activeProviderId: "basic-chat.activeProviderId",
	draft: "basic-chat.draft",
};

interface ChatAttachment {
	id: string;
	name: string;
	type: string;
	size?: number;
	dataUrl: string;
}

interface ChatMessage {
	id: string;
	role: string;
	content: unknown;
	attachments?: ChatAttachment[];
	createdAt: string;
	status?: string;
}

interface ChatSession {
	id: string;
	title: string;
	providerId: string;
	providerName: string;
	modelId: string;
	modelName: string;
	createdAt: string;
	updatedAt: string;
	messages: ChatMessage[];
}

interface ChatModel {
	id: string;
	requestModel: string;
	name: string;
	providerId: string;
	providerName: string;
	source: string;
}

interface ProviderGroup {
	providerId: string;
	providerName: string;
	providerType: string;
	connections: Record<string, any>[];
	models: ChatModel[];
}

function createId(): string {
	if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
	return `chat_${Date.now()}_${Math.random().toString(16).slice(2)}`;
}

function safeParse<T>(value: string | null, fallback: T): T {
	try {
		return JSON.parse(value as string) as T;
	} catch {
		return fallback;
	}
}

function textValue(value: unknown): string {
	if (typeof value === "string") return value;
	if (value == null) return "";
	if (Array.isArray(value)) return value.map(textValue).filter(Boolean).join(" ");
	if (typeof value === "object") {
		const record = value as Record<string, unknown>;
		if (typeof record.message === "string") return record.message;
		if (typeof record.error === "string") return record.error;
		try {
			return JSON.stringify(value);
		} catch {
			return String(value);
		}
	}
	return String(value);
}

function humanize(value: string | null | undefined = ""): string {
	return (
		String(value)
			.replace(/[-_]/g, " ")
			.replace(/\b\w/g, (char) => char.toUpperCase())
			.trim() || "Unknown"
	);
}

function formatRelativeTime(value?: string | null): string {
	if (!value) return "Now";
	const time = new Date(value).getTime();
	if (Number.isNaN(time)) return "Now";
	const diffMinutes = Math.max(1, Math.round((Date.now() - time) / 60000));
	if (diffMinutes < 60) return `${diffMinutes}m`;
	const diffHours = Math.round(diffMinutes / 60);
	if (diffHours < 24) return `${diffHours}h`;
	return `${Math.round(diffHours / 24)}d`;
}

function makeSessionTitle(text: unknown = ""): string {
	const normalized = textValue(text).replace(/\s+/g, " ").trim();
	if (!normalized) return "New chat";
	return normalized.length > 52 ? `${normalized.slice(0, 52).trimEnd()}…` : normalized;
}

function buildUserContent(
	message: ChatMessage,
): string | Array<Record<string, unknown>> {
	const text = textValue(message.content).trim();
	const attachments = Array.isArray(message.attachments) ? message.attachments : [];

	if (attachments.length === 0) return text;

	const content: Array<Record<string, unknown>> = [];
	if (text) content.push({ type: "text", text });

	for (const attachment of attachments) {
		if (attachment?.dataUrl) {
			content.push({ type: "image_url", image_url: { url: attachment.dataUrl } });
		}
	}

	return content.length > 0 ? content : text;
}

function readAssistantText(chunk: unknown): string {
	if (!chunk || typeof chunk !== "object") return "";
	const record = chunk as Record<string, any>;
	const choice = record.choices?.[0];
	const delta = choice?.delta || {};
	const pieces = [delta.content, choice?.message?.content, record.output_text, record.text]
		.map(textValue)
		.filter(Boolean);
	return pieces[0] || "";
}

async function fileToDataUrl(file: File): Promise<string> {
	return await new Promise((resolve, reject) => {
		const reader = new FileReader();
		reader.onload = () => resolve(String(reader.result || ""));
		reader.onerror = () => reject(reader.error || new Error("Failed to read file"));
		reader.readAsDataURL(file);
	});
}

function cloneSession(session: ChatSession): ChatSession {
	return {
		...session,
		messages: Array.isArray(session.messages)
			? session.messages.map((message) => ({ ...message }))
			: [],
	};
}

function getProviderLabel(connection: Record<string, any> | null | undefined): string {
	return connection?.name || humanize(connection?.provider || connection?.id || "provider");
}

function normalizeStaticModel(
	model: Record<string, any>,
	connection: Record<string, any>,
): ChatModel | null {
	if (!model?.id) return null;
	return {
		id: `${connection.provider}/${model.id}`,
		requestModel: `${connection.provider}/${model.id}`,
		name: model.name || model.id,
		providerId: connection.provider,
		providerName: getProviderLabel(connection),
		source: "static",
	};
}

function normalizeLiveModel(model: unknown, connection: Record<string, any>): ChatModel | null {
	const rawId =
		typeof model === "string"
			? model
			: (model as Record<string, any>)?.id ||
				(model as Record<string, any>)?.name ||
				(model as Record<string, any>)?.model ||
				"";
	if (!rawId) return null;

	const displayName =
		typeof model === "string"
			? model
			: (model as Record<string, any>)?.name ||
				(model as Record<string, any>)?.displayName ||
				rawId;

	let requestModel = rawId;
	const isCompatible =
		isOpenAICompatibleProvider(connection.provider) ||
		isAnthropicCompatibleProvider(connection.provider);
	if (isCompatible && !rawId.includes("/")) {
		requestModel = `${connection.provider}/${rawId}`;
	}

	return {
		id: requestModel,
		requestModel,
		name: displayName,
		providerId: connection.provider,
		providerName: getProviderLabel(connection),
		source: "live",
	};
}

function parseProviderModelsPayload(data: Record<string, any>): unknown[] {
	if (Array.isArray(data?.models)) return data.models;
	if (Array.isArray(data?.data)) return data.data;
	if (Array.isArray(data?.results)) return data.results;
	if (Array.isArray(data)) return data;
	return [];
}

function dedupeModels(models: ChatModel[]): ChatModel[] {
	const map = new Map<string, ChatModel>();
	for (const model of models) {
		if (!model?.id) continue;
		if (!map.has(model.id)) map.set(model.id, model);
	}
	return Array.from(map.values());
}

function readStoredSessions(): ChatSession[] {
	if (typeof window === "undefined") return [];
	try {
		const saved = safeParse<unknown>(globalThis.localStorage.getItem(STORAGE_KEYS.sessions), []);
		return Array.isArray(saved)
			? (saved as ChatSession[]).map((session) => ({
					...session,
					messages: Array.isArray(session.messages) ? session.messages : [],
				}))
			: [];
	} catch {
		return [];
	}
}

function readStoredValue(key: string): string {
	if (typeof window === "undefined") return "";
	return globalThis.localStorage.getItem(key) || "";
}

const { getModelsByProviderId } = useModels();

const providerGroups = ref<ProviderGroup[]>([]);
const loadingData = ref(true);
const loadError = ref("");
const sessions = ref<ChatSession[]>(readStoredSessions());
const activeSessionId = ref(readStoredValue(STORAGE_KEYS.activeSessionId));
const activeProviderId = ref(readStoredValue(STORAGE_KEYS.activeProviderId));
const activeModelId = ref("");
const draft = ref(readStoredValue(STORAGE_KEYS.draft));
const attachments = ref<ChatAttachment[]>([]);
const isSending = ref(false);
const streamingMessageId = ref("");
const streamingText = ref("");
const isHydrated = ref(false);
const modelMenuOpen = ref(false);
const historyOpen = ref(false);
const fileInputRef = ref<HTMLInputElement | null>(null);
const modelMenuRef = ref<HTMLElement | null>(null);
const historyMenuRef = ref<HTMLElement | null>(null);
let abortController: AbortController | null = null;
let initialized = false;
let loadCancelled = false;

const modelIndex = computed(() => {
	const map = new Map<string, ChatModel>();
	for (const group of providerGroups.value) {
		for (const model of group.models) {
			map.set(model.id, {
				...model,
				providerId: group.providerId,
				providerName: group.providerName,
			});
		}
	}
	return map;
});

const activeProviderGroup = computed<ProviderGroup | null>(() => {
	return (
		providerGroups.value.find((group) => group.providerId === activeProviderId.value) ||
		providerGroups.value[0] ||
		null
	);
});

const activeModel = computed<ChatModel | null>(() => {
	if (activeModelId.value && modelIndex.value.has(activeModelId.value)) {
		return modelIndex.value.get(activeModelId.value) ?? null;
	}
	if (activeSessionId.value) {
		const session = sessions.value.find((item) => item.id === activeSessionId.value);
		if (session?.modelId && modelIndex.value.has(session.modelId)) {
			return modelIndex.value.get(session.modelId) ?? null;
		}
	}
	return activeProviderGroup.value?.models?.[0] || null;
});

const currentSession = computed(
	() => sessions.value.find((session) => session.id === activeSessionId.value) || null,
);
const currentMessages = computed(() => currentSession.value?.messages || []);
const sessionItems = computed(() =>
	[...sessions.value].sort(
		(a, b) => new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
	),
);
const canSend = computed(
	() =>
		!isSending.value &&
		!!activeModel.value &&
		(draft.value.trim().length > 0 || attachments.value.length > 0),
);

const modelLabel = computed(() => (activeModel.value ? `${activeModel.value.name}` : "Select model"));
const modelSubLabel = computed(() =>
	activeModel.value ? activeModel.value.requestModel : "Choose from connected providers",
);

function isStreamingMessage(message: ChatMessage): boolean {
	return (
		message.role === "assistant" &&
		message.id === streamingMessageId.value &&
		message.status === "streaming"
	);
}

function messageContent(message: ChatMessage): string {
	return textValue(message.content) || (message.role === "assistant" ? streamingText.value : "");
}

function latestMessagePreview(session: ChatSession): string {
	const latest =
		[...(session.messages || [])].reverse().find((message) => message.role === "user") ||
		session.messages?.[0];
	return textValue(latest?.content) || "Empty chat";
}

async function loadData() {
	loadingData.value = true;
	loadError.value = "";

	try {
		const providersRes = await fetch("/api/providers", { cache: "no-store" });
		const providersData = (await providersRes.json().catch(() => ({}))) as Record<string, any>;
		const connections = Array.isArray(providersData.connections)
			? providersData.connections.filter(
					(connection: Record<string, any>) => connection?.isActive !== false,
				)
			: [];

		if (connections.length === 0) {
			if (!loadCancelled) {
				providerGroups.value = [];
				loadError.value = "No providers connected yet.";
			}
			return;
		}

		const providerMap = new Map<string, ProviderGroup>();

		for (const connection of connections) {
			const providerId = connection.provider || connection.id;
			const providerName = getProviderLabel(connection);
			const providerType = isOpenAICompatibleProvider(providerId)
				? "openai-compatible"
				: isAnthropicCompatibleProvider(providerId)
					? "anthropic-compatible"
					: providerId;

			if (!providerMap.has(providerId)) {
				providerMap.set(providerId, {
					providerId,
					providerName,
					providerType,
					connections: [],
					models: [],
				});
			}

			const group = providerMap.get(providerId) as ProviderGroup;
			group.providerName = group.providerName || providerName;
			group.providerType = group.providerType || providerType;
			group.connections.push(connection);

			const staticModels = getModelsByProviderId(providerId)
				.map((model) => normalizeStaticModel(model, connection))
				.filter(Boolean) as ChatModel[];
			group.models.push(...staticModels);
		}

		const liveResults = await Promise.all(
			connections.map(async (connection: Record<string, any>) => {
				try {
					const response = await fetch(`/api/providers/${connection.id}/models`, {
						cache: "no-store",
					});
					const data = (await response.json().catch(() => ({}))) as Record<string, any>;
					if (!response.ok) return { connection, models: [] as ChatModel[] };
					const models = parseProviderModelsPayload(data)
						.map((model) => normalizeLiveModel(model, connection))
						.filter(Boolean) as ChatModel[];
					return { connection, models };
				} catch {
					return { connection, models: [] as ChatModel[] };
				}
			}),
		);

		for (const result of liveResults) {
			const providerId = result.connection.provider || result.connection.id;
			const group = providerMap.get(providerId);
			if (!group) continue;
			group.models.push(...result.models);
		}

		const normalized = Array.from(providerMap.values())
			.map((group) => ({
				...group,
				models: dedupeModels(group.models).sort((a, b) => a.name.localeCompare(b.name)),
			}))
			.filter((group) => group.models.length > 0)
			.sort((a, b) => a.providerName.localeCompare(b.providerName));

		if (!loadCancelled) {
			providerGroups.value = normalized;
			if (normalized.length === 0) {
				loadError.value = "Providers connected but no models available.";
			}
		}
	} catch (error) {
		if (!loadCancelled) {
			loadError.value = textValue((error as Error)?.message) || "Failed to load providers/models.";
			providerGroups.value = [];
		}
	} finally {
		if (!loadCancelled) loadingData.value = false;
	}
}

function handleClickOutside(event: MouseEvent) {
	const target = event.target as Node;
	if (modelMenuRef.value && !modelMenuRef.value.contains(target)) {
		modelMenuOpen.value = false;
	}
	if (historyMenuRef.value && !historyMenuRef.value.contains(target)) {
		historyOpen.value = false;
	}
}

onMounted(() => {
	isHydrated.value = true;
	loadData();
	document.addEventListener("mousedown", handleClickOutside);
});

onBeforeUnmount(() => {
	loadCancelled = true;
	document.removeEventListener("mousedown", handleClickOutside);
});

watch([isHydrated, sessions, activeSessionId, activeProviderId, draft], () => {
	if (!isHydrated.value) return;
	try {
		globalThis.localStorage.setItem(STORAGE_KEYS.sessions, JSON.stringify(sessions.value));
		globalThis.localStorage.setItem(STORAGE_KEYS.activeSessionId, activeSessionId.value);
		globalThis.localStorage.setItem(STORAGE_KEYS.activeProviderId, activeProviderId.value);
		globalThis.localStorage.setItem(STORAGE_KEYS.draft, draft.value);
	} catch {
		// Ignore storage errors.
	}
});

watch(
	[
		isHydrated,
		loadingData,
		providerGroups,
		modelIndex,
		sessions,
		activeSessionId,
		activeProviderId,
		activeModelId,
	],
	() => {
		if (!isHydrated.value || loadingData.value || initialized) return;
		if (providerGroups.value.length === 0) return;

		const savedProvider =
			providerGroups.value.find((group) => group.providerId === activeProviderId.value) ||
			providerGroups.value[0];
		const savedModel =
			activeModelId.value && modelIndex.value.has(activeModelId.value)
				? (modelIndex.value.get(activeModelId.value) as ChatModel)
				: savedProvider.models[0];

		if (sessions.value.length > 0) {
			const session =
				sessions.value.find((item) => item.id === activeSessionId.value) || sessions.value[0];
			const sessionModel =
				session?.modelId && modelIndex.value.has(session.modelId)
					? (modelIndex.value.get(session.modelId) as ChatModel)
					: savedModel;
			initialized = true;
			activeSessionId.value = session.id;
			activeProviderId.value = sessionModel?.providerId || savedProvider.providerId;
			activeModelId.value = sessionModel?.id || savedModel.id;
			return;
		}

		const session: ChatSession = {
			id: createId(),
			title: "New chat",
			providerId: savedProvider.providerId,
			providerName: savedProvider.providerName,
			modelId: savedModel.id,
			modelName: savedModel.name,
			createdAt: new Date().toISOString(),
			updatedAt: new Date().toISOString(),
			messages: [],
		};

		initialized = true;
		sessions.value = [session];
		activeSessionId.value = session.id;
		activeProviderId.value = savedProvider.providerId;
		activeModelId.value = savedModel.id;
	},
);

function updateSession(sessionId: string, updater: (session: ChatSession) => ChatSession) {
	sessions.value = sessions.value.map((session) =>
		session.id === sessionId ? updater(cloneSession(session)) : session,
	);
}

function ensureSessionForModel(model: ChatModel | null): ChatSession | null {
	if (!model) return null;
	return {
		id: createId(),
		title: "New chat",
		providerId: model.providerId,
		providerName: model.providerName,
		modelId: model.id,
		modelName: model.name,
		createdAt: new Date().toISOString(),
		updatedAt: new Date().toISOString(),
		messages: [],
	};
}

function handleSelectSession(sessionId: string) {
	const session = sessions.value.find((item) => item.id === sessionId);
	if (!session) return;
	activeSessionId.value = sessionId;
	activeProviderId.value = session.providerId || activeProviderId.value;
	activeModelId.value = session.modelId || activeModelId.value;
	historyOpen.value = false;
}

function handleDeleteCurrentChat() {
	if (!activeSessionId.value) return;
	const nextSessions = sessions.value.filter((session) => session.id !== activeSessionId.value);
	const fallback = nextSessions[0] || null;
	sessions.value = nextSessions;
	if (fallback) {
		activeSessionId.value = fallback.id;
		activeProviderId.value = fallback.providerId;
		activeModelId.value = fallback.modelId;
	} else {
		activeSessionId.value = "";
		activeProviderId.value = "";
		activeModelId.value = "";
	}
}

function handleSelectModel(modelId: string) {
	const model = modelIndex.value.get(modelId);
	if (!model) return;

	const current = sessions.value.find((session) => session.id === activeSessionId.value);
	if (current && current.messages.length > 0) {
		const session = ensureSessionForModel(model);
		if (!session) return;
		sessions.value = [session, ...sessions.value];
		activeSessionId.value = session.id;
	} else if (current) {
		sessions.value = sessions.value.map((item) =>
			item.id === current.id
				? {
						...item,
						providerId: model.providerId,
						providerName: model.providerName,
						modelId: model.id,
						modelName: model.name,
					}
				: item,
		);
		activeSessionId.value = current.id;
	} else {
		const session = ensureSessionForModel(model);
		if (!session) return;
		sessions.value = [session, ...sessions.value];
		activeSessionId.value = session.id;
	}

	activeProviderId.value = model.providerId;
	activeModelId.value = model.id;
	modelMenuOpen.value = false;
}

async function handleAttachFiles(event: Event) {
	const input = event.target as HTMLInputElement;
	const files = Array.from(input.files || []);
	if (files.length === 0) return;

	const images = files.filter((file) => file.type.startsWith("image/"));
	if (images.length === 0) {
		input.value = "";
		return;
	}

	const converted = await Promise.all(
		images.map(async (file) => ({
			id: createId(),
			name: file.name,
			type: file.type,
			size: file.size,
			dataUrl: await fileToDataUrl(file),
		})),
	);

	attachments.value = [...attachments.value, ...converted];
	input.value = "";
}

function removeAttachment(attachmentId: string) {
	attachments.value = attachments.value.filter(
		(attachment) => attachment.id !== attachmentId,
	);
}

function handleStop() {
	abortController?.abort();
}

function finalizeSessionTitle(sessionId: string, titleSeed: string) {
	const title = makeSessionTitle(titleSeed);
	updateSession(sessionId, (session) => ({
		...session,
		title: session.title === "New chat" ? title : session.title,
		updatedAt: new Date().toISOString(),
	}));
}

async function sendMessage() {
	const model = activeModel.value || activeProviderGroup.value?.models?.[0] || null;
	if (!model) return;

	const userText = draft.value.trim();
	if (!userText && attachments.value.length === 0) return;

	let sessionId = activeSessionId.value;
	let session = sessions.value.find((item) => item.id === sessionId);
	if (!session) {
		const created = ensureSessionForModel(model);
		if (!created) return;
		sessionId = created.id;
		sessions.value = [created, ...sessions.value];
		activeSessionId.value = sessionId;
		session = created;
	}

	const userMessage: ChatMessage = {
		id: createId(),
		role: "user",
		content: userText,
		attachments: attachments.value.map((attachment) => ({
			id: attachment.id,
			name: attachment.name,
			type: attachment.type,
			dataUrl: attachment.dataUrl,
		})),
		createdAt: new Date().toISOString(),
	};

	const assistantMessageId = createId();
	const assistantMessage: ChatMessage = {
		id: assistantMessageId,
		role: "assistant",
		content: "",
		createdAt: new Date().toISOString(),
		status: "streaming",
	};

	const nextMessages = [...(session.messages || []), userMessage, assistantMessage];
	sessions.value = sessions.value.map((item) =>
		item.id === sessionId
			? {
					...item,
					providerId: model.providerId,
					providerName: model.providerName,
					modelId: model.id,
					modelName: model.name,
					messages: nextMessages,
					updatedAt: new Date().toISOString(),
					title: item.title === "New chat" ? makeSessionTitle(userText) : item.title,
				}
			: item,
	);
	draft.value = "";
	attachments.value = [];
	isSending.value = true;
	streamingMessageId.value = assistantMessageId;
	streamingText.value = "";
	abortController?.abort();
	abortController = new AbortController();

	const requestMessages = nextMessages
		.filter((message) => !(message.role === "assistant" && message.id === assistantMessageId))
		.map((message) => ({
			role: message.role,
			content: message.role === "user" ? buildUserContent(message) : message.content,
		}));

	try {
		const response = await fetch("/api/v1/chat/completions", {
			method: "POST",
			headers: {
				"Content-Type": "application/json",
				Accept: "text/event-stream",
			},
			body: JSON.stringify({
				model: model.requestModel || model.id,
				messages: requestMessages,
				stream: true,
			}),
			signal: abortController.signal,
		});

		if (!response.ok) {
			const errorData = (await response.json().catch(() => ({}))) as Record<string, any>;
			throw new Error(
				textValue(errorData.error || errorData.message || `Request failed (${response.status})`),
			);
		}

		const reader = response.body?.getReader();
		if (!reader) {
			const data = (await response.json().catch(() => ({}))) as Record<string, any>;
			const fallbackText = textValue(
				data?.choices?.[0]?.message?.content ||
					data?.output_text ||
					data?.error ||
					data?.message ||
					"",
			);
			updateSession(sessionId, (currentSession) => ({
				...currentSession,
				messages: currentSession.messages.map((message) =>
					message.id === assistantMessageId
						? { ...message, content: fallbackText, status: "done" }
						: message,
				),
				updatedAt: new Date().toISOString(),
			}));
			return;
		}

		const decoder = new TextDecoder();
		let buffer = "";
		let assistantText = "";

		while (true) {
			const { value, done } = await reader.read();
			if (done) break;

			buffer += decoder.decode(value, { stream: true });
			const lines = buffer.split(/\r?\n/);
			buffer = lines.pop() || "";

			for (const line of lines) {
				const trimmed = line.trim();
				if (!trimmed.startsWith("data:")) continue;

				const payload = trimmed.slice(5).trim();
				if (!payload || payload === "[DONE]") continue;

				try {
					const chunk = JSON.parse(payload);
					const text = readAssistantText(chunk);
					if (!text) continue;

					assistantText += text;
					streamingText.value = assistantText;
					updateSession(sessionId, (currentSession) => ({
						...currentSession,
						messages: currentSession.messages.map((message) =>
							message.id === assistantMessageId
								? { ...message, content: assistantText, status: "streaming" }
								: message,
						),
						updatedAt: new Date().toISOString(),
					}));
				} catch {
					// Ignore malformed chunks.
				}
			}
		}

		updateSession(sessionId, (currentSession) => ({
			...currentSession,
			messages: currentSession.messages.map((message) =>
				message.id === assistantMessageId
					? { ...message, content: assistantText || message.content, status: "done" }
					: message,
			),
			updatedAt: new Date().toISOString(),
		}));
		finalizeSessionTitle(sessionId, userText);
	} catch (error) {
		if ((error as Error).name !== "AbortError") {
			const errorText = textValue((error as Error)?.message || error);
			updateSession(sessionId, (currentSession) => ({
				...currentSession,
				messages: currentSession.messages.map((message) =>
					message.id === assistantMessageId
						? {
								...message,
								content: message.content || `Error: ${errorText}`,
								status: "error",
							}
						: message,
				),
				updatedAt: new Date().toISOString(),
			}));
			loadError.value = errorText || "Failed to send message.";
		}
	} finally {
		isSending.value = false;
		streamingMessageId.value = "";
		streamingText.value = "";
		abortController = null;
	}
}

function onDraftInput(event: Event) {
	draft.value = (event.target as HTMLTextAreaElement).value;
}

function handleKeyDown(event: KeyboardEvent) {
	if (event.key === "Enter" && !event.shiftKey) {
		event.preventDefault();
		if (canSend.value) sendMessage();
	}
}
</script>

<template>
  <div class="relative flex-1 flex flex-col h-full min-h-0 min-w-0 bg-[#212121] text-white overflow-hidden">
    <div class="relative mx-auto flex flex-1 h-full min-h-0 w-full max-w-4xl flex-col">
      <div class="flex shrink-0 items-center justify-between gap-3 px-4 py-3 lg:px-6">
        <div ref="modelMenuRef" class="relative">
          <button
            type="button"
            class="flex items-center gap-3 rounded-2xl border border-white/10 bg-white/5 px-4 py-3 text-left transition hover:bg-white/8"
            @click="modelMenuOpen = !modelMenuOpen"
          >
            <div class="min-w-0">
              <div class="flex items-center gap-2">
                <span class="text-sm font-semibold text-white">{{ modelLabel }}</span>
                <span class="material-symbols-outlined text-[18px] text-white/70">expand_more</span>
              </div>
              <p class="truncate text-xs text-white/55">{{ modelSubLabel }}</p>
            </div>
          </button>

          <div
            v-if="modelMenuOpen"
            class="absolute left-0 top-[calc(100%+10px)] z-30 w-[min(520px,calc(100vw-2rem))] overflow-hidden rounded-[20px] border border-white/10 bg-surface-dark shadow-2xl shadow-black/50"
          >
            <div class="border-b border-white/10 px-4 py-3">
              <p class="text-xs uppercase tracking-[0.22em] text-white/45">Models</p>
              <p class="text-sm text-white/75">Only from connected providers</p>
            </div>
            <div class="max-h-[60vh] overflow-y-auto p-2 custom-scrollbar">
              <div
                v-for="group in providerGroups"
                :key="group.providerId"
                class="mb-2 rounded-2xl border border-white/10 bg-black/20 p-2"
              >
                <div class="flex items-center justify-between px-2 py-2">
                  <p class="text-sm font-semibold text-white">{{ group.providerName }}</p>
                  <Badge size="sm" variant="default">{{ group.models.length }}</Badge>
                </div>
                <div class="grid gap-2 sm:grid-cols-2">
                  <button
                    v-for="model in group.models"
                    :key="model.id"
                    type="button"
                    :class="`rounded-[14px] border px-3 py-3 text-left transition ${model.id === activeModelId ? 'border-blue-400/40 bg-blue-500/15' : 'border-white/10 bg-white/5 hover:bg-white/8'}`"
                    @click="handleSelectModel(model.id)"
                  >
                    <div class="flex items-start justify-between gap-3">
                      <div class="min-w-0">
                        <p class="truncate text-sm font-medium text-white">{{ model.name }}</p>
                        <p class="truncate text-[11px] text-white/45">{{ model.requestModel }}</p>
                      </div>
                      <span
                        v-if="model.id === activeModelId"
                        class="material-symbols-outlined text-[18px] text-blue-300"
                      >check_circle</span>
                    </div>
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>

        <div class="flex items-center gap-2">
          <button
            type="button"
            class="rounded-2xl border border-white/10 bg-white/5 px-4 py-3 text-sm text-white/80 transition hover:bg-white/8"
            @click="historyOpen = !historyOpen"
          >
            History
          </button>
          <Button
            variant="ghost"
            size="sm"
            icon="delete"
            :disabled="!activeSessionId || sessions.length === 0"
            @click="handleDeleteCurrentChat"
          >
            Clear
          </Button>
        </div>
      </div>

      <div
        v-if="historyOpen"
        ref="historyMenuRef"
        class="absolute right-4 top-18 z-20 w-[min(360px,calc(100vw-2rem))] rounded-[20px] border border-white/10 bg-surface-dark p-2 shadow-2xl shadow-black/50 lg:right-6"
      >
        <div class="px-3 py-2">
          <p class="text-xs uppercase tracking-[0.22em] text-white/45">Recent chats</p>
        </div>
        <div class="max-h-[48vh] space-y-2 overflow-y-auto p-1 custom-scrollbar">
          <div
            v-if="sessionItems.length === 0"
            class="rounded-2xl border border-dashed border-white/10 bg-white/5 p-4 text-sm text-white/55"
          >
            No conversations yet.
          </div>
          <button
            v-for="session in sessionItems"
            v-else
            :key="session.id"
            type="button"
            :class="`w-full rounded-2xl border px-3 py-3 text-left transition ${session.id === activeSessionId ? 'border-blue-400/40 bg-blue-500/15' : 'border-white/10 bg-white/5 hover:bg-white/8'}`"
            @click="handleSelectSession(session.id)"
          >
            <div class="flex items-start justify-between gap-3">
              <div class="min-w-0 flex-1">
                <p class="truncate text-sm font-medium text-white">{{ session.title }}</p>
                <p class="mt-1 truncate text-xs text-white/50">{{ latestMessagePreview(session) }}</p>
              </div>
              <span class="text-[10px] text-white/40 shrink-0">{{ formatRelativeTime(session.updatedAt) }}</span>
            </div>
          </button>
        </div>
      </div>

      <div
        v-if="loadError"
        class="mt-4 rounded-[18px] border border-rose-500/20 bg-rose-500/10 px-4 py-3 text-rose-100"
      >
        <div class="flex items-start gap-3">
          <span class="material-symbols-outlined text-[20px]">error</span>
          <p class="text-sm leading-6">{{ loadError }}</p>
        </div>
      </div>

      <div class="flex flex-1 flex-col min-h-0">
        <div class="flex-1 overflow-y-auto py-4 custom-scrollbar">
          <div
            v-if="currentMessages.length === 0"
            class="flex min-h-[50vh] items-center justify-center px-4 text-center"
          >
            <div class="max-w-xl space-y-4">
              <div
                class="mx-auto flex size-16 items-center justify-center rounded-[20px] border border-white/10 bg-white/5 text-white/80"
              >
                <span class="material-symbols-outlined text-[30px]">chat</span>
              </div>
              <div class="space-y-2">
                <h2 class="text-2xl font-semibold text-white">Start a conversation</h2>
                <p class="text-sm leading-6 text-white/60">
                  Simple chat interface to interact with any AI model from connected providers. Select a model and start chatting!
                </p>
              </div>
            </div>
          </div>

          <div class="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4">
            <div
              v-for="message in currentMessages"
              :key="message.id"
              :class="`flex w-full ${message.role === 'user' ? 'justify-end' : 'justify-start'} mb-6`"
            >
              <div
                :class="`max-w-[min(88%,42rem)] ${message.role === 'user' ? 'rounded-3xl bg-[#2f2f2f] px-5 py-3.5 text-white' : 'text-white/90'}`"
              >
                <div class="mb-1 flex items-center justify-between gap-3">
                  <span class="text-xs font-semibold">{{ message.role === "user" ? "You" : activeModel?.name || "Assistant" }}</span>
                </div>

                <div
                  v-if="message.attachments?.length"
                  class="mb-3 grid grid-cols-2 gap-2 sm:grid-cols-3 mt-2"
                >
                  <a
                    v-for="attachment in message.attachments"
                    :key="attachment.id"
                    :href="attachment.dataUrl"
                    target="_blank"
                    rel="noreferrer"
                    class="overflow-hidden rounded-[18px] border border-white/10 bg-black/20"
                  >
                    <img
                      :src="attachment.dataUrl"
                      :alt="attachment.name"
                      class="h-28 w-full object-cover"
                      loading="lazy"
                      decoding="async"
                    />
                  </a>
                </div>

                <div class="whitespace-pre-wrap wrap-break-word text-[15px] leading-7">
                  {{ messageContent(message) }}
                  <span v-if="isStreamingMessage(message) && !streamingText" class="inline-block animate-pulse">▋</span>
                </div>
              </div>
            </div>
          </div>
        </div>

        <div class="shrink-0 pt-2">
          <div
            v-if="attachments.length > 0"
            class="mx-auto mb-3 flex w-full max-w-3xl flex-wrap gap-2 px-4"
          >
            <div
              v-for="attachment in attachments"
              :key="attachment.id"
              class="flex items-center gap-2 rounded-full border border-white/10 bg-white/5 px-3 py-2"
            >
              <span class="text-xs text-white/80 max-w-48 truncate">{{ attachment.name }}</span>
              <button
                type="button"
                class="text-white/55 hover:text-white"
                aria-label="Remove attachment"
                @click="removeAttachment(attachment.id)"
              >
                <span class="material-symbols-outlined text-[18px]">close</span>
              </button>
            </div>
          </div>

          <div class="mx-auto w-full max-w-3xl px-4 pb-2">
            <div class="rounded-[26px] bg-[#2f2f2f] px-3 pt-3 pb-2 shadow-[0_0_15px_rgba(0,0,0,0.10)] ring-1 ring-white/5">
              <textarea
                :value="draft"
                placeholder="Message AI"
                :rows="1"
                class="w-full resize-none bg-transparent px-2 text-[15px] leading-6 text-white outline-none placeholder:text-white/40 custom-scrollbar max-h-[25vh] overflow-y-auto"
                @input="onDraftInput"
                @keydown="handleKeyDown"
              />

              <div class="mt-2 flex items-center justify-between gap-3">
                <div class="flex items-center gap-2">
                  <button
                    type="button"
                    :disabled="!activeModel || loadingData"
                    class="p-2 text-white/50 hover:text-white transition rounded-full hover:bg-white/5"
                    @click="fileInputRef?.click()"
                  >
                    <span class="material-symbols-outlined text-[20px]">attach_file</span>
                  </button>
                  <input
                    ref="fileInputRef"
                    type="file"
                    accept="image/*"
                    multiple
                    class="hidden"
                    @change="handleAttachFiles"
                  />
                  <span class="text-xs font-medium text-white/30 truncate max-w-30">{{ activeModel ? activeModel.name : "No model" }}</span>
                </div>

                <div class="flex items-center gap-2">
                  <button
                    v-if="isSending"
                    type="button"
                    class="p-2 text-white bg-white/10 hover:bg-white/20 transition rounded-full h-8 w-8 flex items-center justify-center"
                    @click="handleStop"
                  >
                    <span class="material-symbols-outlined text-[16px]">stop</span>
                  </button>
                  <button
                    type="button"
                    :disabled="!canSend"
                    :class="`h-8 w-8 rounded-full flex items-center justify-center transition ${canSend ? 'bg-white text-black hover:opacity-90' : 'bg-white/10 text-white/30 cursor-not-allowed'}`"
                    @click="sendMessage"
                  >
                    <span class="material-symbols-outlined text-[16px]">arrow_upward</span>
                  </button>
                </div>
              </div>
            </div>
          </div>
        </div>

        <p class="mx-auto mt-2 max-w-3xl px-4 pb-4 text-center text-[11px] text-white/30">
          Model list is filtered from connected providers.
        </p>
      </div>
    </div>
  </div>
</template>
