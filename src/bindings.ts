// 手写的 Web 数据层，替代原 tauri-specta 生成的 bindings.ts。
// 保留 `commands.xxx(...)` 与 `events.yyy.listen(cb)` 的调用形状，只替换传输层：
//   - commands  -> HTTP POST /api/...（错误体为 CommandError，转为 { status: "error", error }）
//   - events    -> 单条 WebSocket 连接，按 topic 分发
// 注意：不使用 fetch 的 credentials，鉴权靠 Authorization 头。

/** ============================ 基础配置 ============================ */

/** 后端 base url。开发态走 vite 代理，同源即可。 */
const BASE_URL: string = "";

/** 认证 token 的 localStorage key，需与 store.ts 保持一致。 */
const TOKEN_KEY = "pica_token";

function getToken(): string {
	return localStorage.getItem(TOKEN_KEY) ?? "";
}

export function setToken(token: string): void {
	if (token) localStorage.setItem(TOKEN_KEY, token);
	else localStorage.removeItem(TOKEN_KEY);
}

/** ============================ 类型定义 ============================ */

export type Result<T, E> =
	| { status: "ok"; data: T }
	| { status: "error"; error: E };

export type DownloadByIdResult = {
        comicId: string;
        comicTitle: string;
        createdChapters: string[];
        skippedChapters: string[];
        alreadyRunningChapters: string[];
        createdCount: number;
};

export type CommandError = { err_title: string; err_message: string };

export type JsonValue =
	| null
	| boolean
	| number
	| string
	| JsonValue[]
	| { [key in string]: JsonValue };

export type Image = { originalName: string; path: string; fileServer: string };
export type ImageRespData = {
	originalName: string;
	path: string;
	fileServer: string;
};

export type DownloadFormat = "Jpeg" | "Png" | "Webp" | "Original";
export type ProxyMode = "System" | "NoProxy" | "Custom";
export type SearchSort = "TimeNewest" | "TimeOldest" | "LikeMost" | "ViewMost";
export type LogLevel = "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR";
export type DownloadTaskState =
	| "Pending"
	| "Downloading"
	| "Paused"
	| "Cancelled"
	| "Completed"
	| "Failed";

export type Pagination<T> = {
	total: number;
	limit: number;
	page: number;
	pages: number;
	docs: T[];
};

export type Config = {
	token: string;
	downloadDir: string;
	enableFileLogger: boolean;
	downloadFormat: DownloadFormat;
	dirFmt: string;
	proxyMode: ProxyMode;
	proxyHost: string;
	proxyPort: number;
	chapterConcurrency: number;
	chapterDownloadIntervalSec: number;
	imgConcurrency: number;
	imgDownloadIntervalSec: number;
	shouldDownloadCover: boolean;
	apiBaseUrl: string;
};

/** GET /api/server/info 的返回结构。 */
export type ServerInfo = {
	version: string;
	dataDir: string;
	downloadDir: string;
	eventSubscribers: number;
};

export type Creator = {
	id: string;
	gender: string;
	name: string;
	title: string;
	verified: boolean | null;
	exp: number;
	level: number;
	characters: string[];
	avatar: Image;
	slogan: string;
	role: string;
	character: string;
};

export type ChapterInfo = {
	chapterId: string;
	chapterTitle: string;
	order: number;
	isDownloaded?: boolean | null;
	chapterDownloadDir?: string | null;
};

export type Comic = {
	id: string;
	title: string;
	author: string;
	pagesCount: number;
	chapterInfos: ChapterInfo[];
	chapterCount: number;
	finished: boolean;
	categories: string[];
	thumb: Image;
	likesCount: number;
	creator: Creator;
	description: string;
	chineseTeam: string;
	tags: string[];
	updatedAt: string;
	createdAt: string;
	allowDownload: boolean;
	viewsCount: number;
	isLiked: boolean;
	commentsCount: number;
	isDownloaded?: boolean | null;
	comicDownloadDir?: string | null;
};


export type ComicInSearch = {
	id: string;
	author: string;
	categories: string[];
	chineseTeam: string;
	createdAt: string;
	description: string;
	finished: boolean;
	likesCount: number;
	tags: string[];
	thumb: ImageRespData;
	title: string;
	totalLikes: number | null;
	totalViews: number | null;
	updatedAt: string;
	isDownloaded: boolean;
	comicDownloadDir: string;
};

export type SearchResult = Pagination<ComicInSearch>;

export type UserProfileDetailRespData = {
	_id: string;
	gender: string;
	name: string;
	title: string;
	verified: boolean;
	exp: number;
	level: number;
	characters: string[];
	avatar?: ImageRespData;
	birthday: string;
	email: string;
	created_at: string;
	isPunched: boolean;
};

export type LogEvent = {
	timestamp: string;
	level: LogLevel;
	fields: { [key in string]: JsonValue };
	target: string;
	filename: string;
	line_number: number;
};

export type DownloadTaskEvent =
	| {
			event: "Create";
			data: {
				state: DownloadTaskState;
				comic: Comic;
				chapterInfo: ChapterInfo;
				downloadedImgCount: number;
				totalImgCount: number;
			};
	  }
	| {
			event: "Update";
			data: {
				chapterId: string;
				state: DownloadTaskState;
				downloadedImgCount: number;
				totalImgCount: number;
			};
	  };

/** ============================ HTTP 传输层 ============================ */

/** 从任意失败响应里尽力还原 CommandError。 */
async function toCommandError(res: Response): Promise<CommandError> {
	let title = `HTTP ${res.status}`;
	let message = res.statusText || "请求失败";
	try {
		const body = await res.json();
		if (body && (body.err_title || body.err_message)) {
			title = body.err_title ?? title;
			message = body.err_message ?? message;
		} else if (body && (body.errTitle || body.errMessage)) {
			// 鉴权中间件用的是 camelCase
			title = body.errTitle ?? title;
			message = body.errMessage ?? message;
		}
	} catch {
		// 非 JSON 响应，保留默认文案
	}
	return { err_title: title, err_message: message };
}

/** 发一个 POST 请求，返回原始 Response。 */
async function post(path: string, body: unknown): Promise<Response> {
	const token = getToken();
	const headers: Record<string, string> = {
		"Content-Type": "application/json",
	};
	if (token) headers["Authorization"] = `Bearer ${token}`;

	return await fetch(`${BASE_URL}${path}`, {
		method: "POST",
		headers,
		body: JSON.stringify(body ?? {}),
	});
}

/** 发一个 GET 请求，返回原始 Response。 */
async function get(path: string): Promise<Response> {
	const token = getToken();
	const headers: Record<string, string> = {};
	if (token) headers["Authorization"] = `Bearer ${token}`;

	return await fetch(`${BASE_URL}${path}`, { method: "GET", headers });
}

/**
 * 包装成 `Result<T, CommandError>`。
 * 网络层异常（断网 / 后端没起）也会被转成 CommandError，而不是抛出去，
 * 这样调用方原有的 `if (result.status === "error")` 分支依然有效。
 */
async function callResult<T>(
	fn: () => Promise<Response>,
): Promise<Result<T, CommandError>> {
	try {
		const res = await fn();
		if (!res.ok) {
			return { status: "error", error: await toCommandError(res) };
		}
		// 204 / 空 body 视为 null
		const text = await res.text();
		const data = text ? (JSON.parse(text) as T) : (null as T);
		return { status: "ok", data };
	} catch (e) {
		return {
			status: "error",
			error: {
				err_title: "网络错误",
				err_message: e instanceof Error ? e.message : String(e),
			},
		};
	}
}

/** 直返版本，失败时抛出，对应原 `Promise<T>`（非 Result）的命令。 */
async function callDirect<T>(fn: () => Promise<Response>): Promise<T> {
	const res = await fn();
	if (!res.ok) {
		const err = await toCommandError(res);
		throw new Error(`${err.err_title}: ${err.err_message}`);
	}
	const text = await res.text();
	return text ? (JSON.parse(text) as T) : (null as T);
}

/** ============================ 命令层 ============================ */

export const commands = {
	async getConfig(): Promise<Config> {
		return await callDirect<Config>(() => get("/api/config"));
	},

	async saveConfig(config: Config): Promise<Result<null, CommandError>> {
		return await callResult<null>(() => post("/api/config", { config }));
	},

	async login(
		email: string,
		password: string,
	): Promise<Result<string, CommandError>> {
		const result = await callResult<string>(() =>
			post("/api/login", { email, password }),
		);
		// 登录成功即把 token 存下来，供后续所有请求使用。
		if (result.status === "ok" && result.data) setToken(result.data);
		return result;
	},

	async getUserProfile(): Promise<
		Result<UserProfileDetailRespData, CommandError>
	> {
		return await callResult<UserProfileDetailRespData>(() =>
			post("/api/user/profile", {}),
		);
	},

	async searchComic(
		keyword: string,
		sort: SearchSort,
		page: number,
		categories: string[],
	): Promise<Result<SearchResult, CommandError>> {
		return await callResult<SearchResult>(() =>
			post("/api/search", { keyword, sort, page, categories }),
		);
	},

	async getComic(comicId: string): Promise<Result<Comic, CommandError>> {
		return await callResult<Comic>(() => post("/api/comic", { comicId }));
	},

	async downloadComic(comicId: string): Promise<Result<null, CommandError>> {
		return await callResult<null>(() => post("/api/download/comic", { comicId }));
	},
      async downloadById(
              comicId: string,
              chapterId?: string,
      ): Promise<Result<DownloadByIdResult, CommandError>> {
              return await callResult<DownloadByIdResult>(() =>
                      post("/api/download/by-id", { comicId, chapterId }),
              );
      },

	async createDownloadTask(
		comic: Comic,
		chapterId: string,
	): Promise<Result<null, CommandError>> {
		return await callResult<null>(() =>
			post("/api/download/task", { comic, chapterId }),
		);
	},

	async pauseDownloadTask(
		chapterId: string,
	): Promise<Result<null, CommandError>> {
		return await callResult<null>(() =>
			post(`/api/download/task/${encodeURIComponent(chapterId)}/pause`, {}),
		);
	},

	async resumeDownloadTask(
		chapterId: string,
	): Promise<Result<null, CommandError>> {
		return await callResult<null>(() =>
			post(`/api/download/task/${encodeURIComponent(chapterId)}/resume`, {}),
		);
	},

	async cancelDownloadTask(
		chapterId: string,
	): Promise<Result<null, CommandError>> {
		return await callResult<null>(() =>
			post(`/api/download/task/${encodeURIComponent(chapterId)}/cancel`, {}),
		);
	},

	async getLogsDirSize(): Promise<Result<number, CommandError>> {
		return await callResult<number>(() => post("/api/logs/size", {}));
	},

	/** 后端版本与运行状态，给「关于」和「设置」页用。 */
	async getServerInfo(): Promise<ServerInfo> {
		return await callDirect<ServerInfo>(() => post("/api/server/info", {}));
	},

	/**
	 * 读取后端最新日志文件末尾若干行。
	 * 原桌面版没有这个命令（日志靠事件流），Web 版刷新页面后需要补历史。
	 */
	async getLogs(tail: number): Promise<string[]> {
		return await callDirect<string[]>(() => post("/api/logs", { tail }));
	},

	async getSyncedComic(comic: Comic): Promise<Result<Comic, CommandError>> {
		return await callResult<Comic>(() => post("/api/sync/comic", { comic }));
	},

	async getSyncedComicInSearch(
		comic: ComicInSearch,
	): Promise<Result<ComicInSearch, CommandError>> {
		return await callResult<ComicInSearch>(() =>
			post("/api/sync/comic-in-search", { comic }),
		);
	},
};

/** ============================ WebSocket 事件层 ============================ */

/** topic -> payload 类型映射，与原 events 对象一致。 */
type EventMap = {
	downloadTaskEvent: DownloadTaskEvent;
	logEvent: LogEvent;
        taskSnapshot: DownloadTaskEvent[];
};

/** topic -> 回调集合。 */
type Listener = (payload: any) => void;

const listeners = new Map<string, Set<Listener>>();

/** 最近一次收到的各 topic 消息，供后挂载的组件补状态。 */
const lastPayload = new Map<string, any>();

let socket: WebSocket | null = null;
let reconnectDelay = 1000;
let heartbeatTimer: number | null = null;
let manualClose = false;

function wsUrl(token: string): string {
	const proto = location.protocol === "https:" ? "wss:" : "ws:";
	// 浏览器的 WebSocket 构造函数不支持自定义请求头，凭证只能走查询串。
	// 后端 `require_auth` 有对应的 `?token=` 兜底分支。
	const query = token ? `?token=${encodeURIComponent(token)}` : "";
	return `${proto}//${location.host}/api/ws${query}`;
}

function dispatch(topic: string, payload: any): void {
	lastPayload.set(topic, payload);
	const set = listeners.get(topic);
	if (!set) return;
	for (const fn of set) {
		try {
			fn(payload);
		} catch (e) {
			console.error(`[ws] listener for "${topic}" threw`, e);
		}
	}
}

function scheduleReconnect(): void {
	if (manualClose) return;
	window.setTimeout(() => {
		connect();
	}, reconnectDelay);
	// 指数退避，上限 15s
	reconnectDelay = Math.min(reconnectDelay * 2, 15000);
}

function connect(): void {
	if (
		socket &&
		(socket.readyState === WebSocket.OPEN ||
			socket.readyState === WebSocket.CONNECTING)
	) {
		return;
	}
    // token 可能为空（后端关闭了认证），此时也建立连接，只是不带凭证。
    socket = new WebSocket(wsUrl(getToken()));
	socket.onopen = () => {
		reconnectDelay = 1000;
		// 后端 30s 主动 Ping，这里发个首帧让反代确认连接已建立
		try {
			socket?.send("ping");
		} catch {
			/* ignore */
		}
		// 客户端心跳，防止中间代理空闲断连
		if (heartbeatTimer !== null) window.clearInterval(heartbeatTimer);
		heartbeatTimer = window.setInterval(() => {
			if (socket?.readyState === WebSocket.OPEN) {
				try {
					socket.send("ping");
				} catch {
					/* ignore */
				}
			}
		}, 25000);
	};

	socket.onmessage = (ev: MessageEvent<string>) => {
		if (!ev.data || ev.data === "pong") return;
		let msg: { topic?: string; payload?: any };
		try {
			msg = JSON.parse(ev.data);
		} catch {
			return;
		}
		if (!msg.topic) return;
		dispatch(msg.topic, msg.payload);
	};

	socket.onclose = () => {
		if (heartbeatTimer !== null) {
			window.clearInterval(heartbeatTimer);
			heartbeatTimer = null;
		}
		socket = null;
		scheduleReconnect();
	};

	socket.onerror = () => {
		// onclose 会紧随其后，统一在那里重连
	};
}

/** 登录成功后调用，建立（或重建）事件连接。 */
export function reconnectEvents(): void {
	manualClose = false;
	if (socket) {
		try {
			socket.close();
		} catch {
			/* ignore */
		}
		socket = null;
	}
	connect();
}

/** 登出时调用，断开并清空。 */
export function disconnectEvents(): void {
	manualClose = true;
	if (heartbeatTimer !== null) {
		window.clearInterval(heartbeatTimer);
		heartbeatTimer = null;
	}
	if (socket) {
		try {
			socket.close();
		} catch {
			/* ignore */
		}
		socket = null;
	}
	lastPayload.clear();
}

const TOPIC_MAP: Record<keyof EventMap, string> = {
	downloadTaskEvent: "download-task-event",
	logEvent: "log-event",
        taskSnapshot: "task-snapshot-event",
};

function subscribe<K extends keyof EventMap>(
	key: K,
	cb: (ev: { payload: EventMap[K] }) => void,
): () => void {
	const topic = TOPIC_MAP[key];
	const wrapped: Listener = (payload) => cb({ payload });

	let set = listeners.get(topic);
	if (!set) {
		set = new Set();
		listeners.set(topic, set);
	}
	set.add(wrapped);

	// 新订阅者立刻拿到该 topic 的最近一条消息，
	// 避免组件挂载晚于事件到达而漏掉状态（如任务快照）。
	const cached = lastPayload.get(topic);
	if (cached !== undefined) {
		try {
			cb({ payload: cached });
		} catch (e) {
			console.error(`[ws] replay for "${topic}" threw`, e);
		}
	}

	connect();

	return () => {
		set?.delete(wrapped);
	};
}

/**
 * 保持与 tauri-specta 相同的 `events.xxx.listen(cb)` 形状。
 * 返回值是取消订阅函数，原版返回 Promise<UnlistenFn>，
 * 调用方大多写成 `await events.x.listen(...)`，await 一个函数同样是安全的。
 */
export const events = {
	downloadTaskEvent: {
		listen: (cb: (ev: { payload: DownloadTaskEvent }) => void) =>
			subscribe("downloadTaskEvent", cb),
	},
        taskSnapshot: {
                listen: (cb: (ev: { payload: DownloadTaskEvent[] }) => void) =>
                        subscribe("taskSnapshot", cb),
        },
	logEvent: {
		listen: (cb: (ev: { payload: LogEvent }) => void) =>
			subscribe("logEvent", cb),
	},
};
