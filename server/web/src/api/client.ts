import type {
  AdminSettings,
  AdminUser,
  ApiErrorBody,
  ApiMode,
  AuthTokensResponse,
  AuthUser,
  CreateUserPayload,
  DevicesQuery,
  DevicesResponse,
  HealthResponse,
  PkiCaInfo,
  SessionsResponse,
  StatsResponse,
  UpdateUserPayload,
  UsersListQuery,
  UsersListResponse,
} from '../types/api';
import { MockApiError, mockServer } from './mockServer';

const SESSION_STORAGE_KEY = 'rc_admin_session';
const MODE_STORAGE_KEY = 'rc_admin_api_mode';

export interface StoredSession {
  access_token: string;
  refresh_token: string;
  user: AuthUser;
}

export class ApiError extends Error {
  status: number;
  code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

type AuthEventHandlers = {
  onSessionUpdated?: (session: StoredSession | null) => void;
  onAuthExpired?: (message: string) => void;
  onPasswordChangeRequired?: () => void;
};

let handlers: AuthEventHandlers = {};
let refreshInFlight: { gen: number; promise: Promise<StoredSession | null> } | null = null;
let authGeneration = 0;

export function setAuthEventHandlers(next: AuthEventHandlers): void {
  handlers = next;
}

export function getApiMode(): ApiMode {
  const saved = localStorage.getItem(MODE_STORAGE_KEY);
  return saved === 'mock' ? 'mock' : 'real';
}

export function setApiMode(mode: ApiMode): void {
  authGeneration += 1;
  localStorage.setItem(MODE_STORAGE_KEY, mode);
}

export function getStoredSession(): StoredSession | null {
  const raw = localStorage.getItem(SESSION_STORAGE_KEY);
  if (!raw) return null;
  try {
    const parsed = JSON.parse(raw) as StoredSession;
    if (parsed && typeof parsed.access_token === 'string' && parsed.user) {
      return parsed;
    }
    return null;
  } catch {
    return null;
  }
}

export function saveStoredSession(session: StoredSession | null): void {
  if (session) {
    localStorage.setItem(SESSION_STORAGE_KEY, JSON.stringify(session));
  } else {
    authGeneration += 1;
    localStorage.removeItem(SESSION_STORAGE_KEY);
  }
  handlers.onSessionUpdated?.(session);
}

function buildQueryString(params: Record<string, string | number | undefined>): string {
  const sp = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== '') {
      sp.set(k, String(v));
    }
  }
  const qs = sp.toString();
  return qs ? `?${qs}` : '';
}

async function parseErrorResponse(res: Response): Promise<ApiError> {
  let code = `http_${res.status}`;
  let message = `请求失败 (${res.status})`;
  try {
    const body = (await res.json()) as Partial<ApiErrorBody>;
    if (body.error) code = body.error;
    if (body.message) message = body.message;
  } catch {
    // non-JSON response fallback
    if (res.status === 502 || res.status === 503 || res.status === 504) {
      message = '无法连接到 RabbitControl 服务端（请检查后端是否已启动或切换左下角演示模式）';
    }
  }
  return new ApiError(res.status, code, message);
}

async function refreshRealSession(
  failedAccessToken?: string,
  expectedGen?: number,
): Promise<StoredSession | null> {
  if (expectedGen !== undefined && expectedGen !== authGeneration) {
    return null;
  }
  const latestBefore = getStoredSession();
  if (
    failedAccessToken &&
    latestBefore?.access_token &&
    latestBefore.access_token !== failedAccessToken
  ) {
    return latestBefore;
  }
  const startGen = authGeneration;
  if (refreshInFlight && refreshInFlight.gen === startGen) {
    return refreshInFlight.promise;
  }

  const refreshPromise = (async () => {
    const current = getStoredSession();
    if (!current?.refresh_token) {
      if (authGeneration === startGen) {
        saveStoredSession(null);
      }
      return null;
    }

    try {
      const res = await fetch('/api/v1/auth/refresh', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: current.refresh_token }),
      });

      if (!res.ok) {
        const parsed = await parseErrorResponse(res);
        if (authGeneration !== startGen) {
          return null;
        }
        saveStoredSession(null);
        handlers.onAuthExpired?.(parsed.message || '登录凭证已过期，请重新登录');
        return null;
      }

      const data = (await res.json()) as AuthTokensResponse;
      if (authGeneration !== startGen) {
        return null;
      }
      const updated: StoredSession = {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        user: data.user,
      };
      saveStoredSession(updated);
      return updated;
    } catch {
      if (authGeneration !== startGen) {
        return null;
      }
      saveStoredSession(null);
      handlers.onAuthExpired?.('刷新登录凭证失败，请重新登录');
      return null;
    } finally {
      if (refreshInFlight?.gen === startGen) {
        refreshInFlight = null;
      }
    }
  })();

  refreshInFlight = { gen: startGen, promise: refreshPromise };
  return refreshPromise;
}

async function realRequest<T>(
  path: string,
  options: RequestInit = {},
  {
    auth = true,
    retryOn401 = true,
  }: { auth?: boolean; retryOn401?: boolean } = {},
): Promise<T> {
  const startGen = authGeneration;
  const headers = new Headers(options.headers);
  if (options.body && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json');
  }

  let usedAccessToken: string | undefined;
  if (auth) {
    const session = getStoredSession();
    if (session?.access_token) {
      usedAccessToken = session.access_token;
      headers.set('Authorization', `Bearer ${session.access_token}`);
    }
  }

  let res: Response;
  try {
    res = await fetch(path, { ...options, headers });
  } catch {
    throw new ApiError(
      0,
      'network_error',
      '网络请求失败，无法连接到管理服务端（可确认后端运行状态或使用登录页演示模式）',
    );
  }

  if (res.status === 401 && auth && retryOn401) {
    if (authGeneration !== startGen) {
      throw new ApiError(401, 'unauthorized', '登录状态已失效，请重新登录');
    }
    const refreshed = await refreshRealSession(usedAccessToken, startGen);
    if (refreshed && authGeneration === startGen) {
      return realRequest<T>(path, options, { auth, retryOn401: false });
    }
    throw new ApiError(401, 'unauthorized', '登录状态已失效，请重新登录');
  }

  if (!res.ok) {
    const err = await parseErrorResponse(res);
    if (authGeneration === startGen) {
      if (res.status === 403 && err.code === 'password_change_required') {
        const current = getStoredSession();
        if (current && !current.user.must_change_password) {
          saveStoredSession({
            ...current,
            user: { ...current.user, must_change_password: true },
          });
        }
        handlers.onPasswordChangeRequired?.();
      } else if (res.status === 403 && err.code === 'account_disabled' && auth) {
        saveStoredSession(null);
        handlers.onAuthExpired?.(err.message || '当前账号已被禁用');
      }
    }
    throw err;
  }

  if (res.status === 204) {
    return undefined as T;
  }
  const text = await res.text();
  if (!text) {
    return undefined as T;
  }
  return JSON.parse(text) as T;
}

function wrapMockError(err: unknown, startGen?: number): never {
  if (err instanceof MockApiError) {
    const sameGen = startGen === undefined || startGen === authGeneration;
    if (sameGen && err.status === 403 && err.code === 'password_change_required') {
      handlers.onPasswordChangeRequired?.();
    }
    if (sameGen && err.status === 401) {
      saveStoredSession(null);
      handlers.onAuthExpired?.(err.message);
    }
    throw new ApiError(err.status, err.code, err.message);
  }
  throw err;
}

async function runMock<T>(fn: () => Promise<T>): Promise<T> {
  const startGen = authGeneration;
  try {
    return await fn();
  } catch (e) {
    return wrapMockError(e, startGen);
  }
}

function tokenOrNull(): string | null {
  return getStoredSession()?.access_token ?? null;
}

export const apiClient = {
  async login(email: string, password: string): Promise<StoredSession> {
    const startGen = authGeneration;
    if (getApiMode() === 'mock') {
      try {
        const res = await mockServer.login(email, password);
        if (authGeneration !== startGen) {
          throw new ApiError(409, 'auth_superseded', '登录请求已取消，请重试');
        }
        const session: StoredSession = {
          access_token: res.access_token,
          refresh_token: res.refresh_token,
          user: res.user,
        };
        authGeneration += 1;
        saveStoredSession(session);
        return session;
      } catch (e) {
        if (e instanceof MockApiError) {
          throw new ApiError(e.status, e.code, e.message);
        }
        throw e;
      }
    }

    const res = await realRequest<AuthTokensResponse>(
      '/api/v1/auth/login',
      {
        method: 'POST',
        body: JSON.stringify({ email: email.trim(), password }),
      },
      { auth: false, retryOn401: false },
    );
    if (authGeneration !== startGen) {
      throw new ApiError(409, 'auth_superseded', '登录请求已取消，请重试');
    }
    const session: StoredSession = {
      access_token: res.access_token,
      refresh_token: res.refresh_token,
      user: res.user,
    };
    authGeneration += 1;
    saveStoredSession(session);
    return session;
  },

  async logout(): Promise<void> {
    const session = getStoredSession();
    const accessToken = session?.access_token;
    const refreshToken = session?.refresh_token;
    authGeneration += 1;
    // Clear local session immediately so concurrent polls during the network request cannot refresh/emit 401 expiry.
    saveStoredSession(null);
    try {
      if (getApiMode() === 'mock') {
        await mockServer.logout();
      } else if (accessToken || refreshToken) {
        await realRequest<void>(
          '/api/v1/auth/logout',
          {
            method: 'POST',
            headers: accessToken
              ? { Authorization: `Bearer ${accessToken}` }
              : undefined,
            body: JSON.stringify(
              refreshToken ? { refresh_token: refreshToken } : {},
            ),
          },
          { auth: false, retryOn401: false },
        );
      }
    } catch {
      // Ignore network/401 errors during logout; local state is already cleared.
    }
  },

  async getMe(): Promise<AuthUser> {
    const startGen = authGeneration;
    if (getApiMode() === 'mock') {
      return runMock(async () => {
        const user = await mockServer.me(tokenOrNull());
        const current = getStoredSession();
        if (current && authGeneration === startGen) {
          saveStoredSession({ ...current, user });
        }
        return current && authGeneration !== startGen ? current.user : user;
      });
    }

    const user = await realRequest<AuthUser>('/api/v1/me');
    const current = getStoredSession();
    if (current && authGeneration === startGen) {
      saveStoredSession({ ...current, user });
    }
    return current && authGeneration !== startGen ? current.user : user;
  },

  async changePassword(currentPassword: string, newPassword: string): Promise<StoredSession> {
    const startGen = authGeneration;
    if (getApiMode() === 'mock') {
      try {
        const res = await mockServer.changePassword(tokenOrNull(), currentPassword, newPassword);
        if (authGeneration !== startGen) {
          throw new ApiError(409, 'auth_superseded', '会话已变更，改密请求已失效');
        }
        const session: StoredSession = {
          access_token: res.access_token,
          refresh_token: res.refresh_token,
          user: res.user,
        };
        authGeneration += 1;
        saveStoredSession(session);
        return session;
      } catch (e) {
        if (e instanceof MockApiError) {
          throw new ApiError(e.status, e.code, e.message);
        }
        throw e;
      }
    }

    const res = await realRequest<AuthTokensResponse>(
      '/api/v1/auth/change-password',
      {
        method: 'POST',
        body: JSON.stringify({
          current_password: currentPassword,
          new_password: newPassword,
        }),
      },
      { auth: true, retryOn401: true },
    );
    if (authGeneration !== startGen) {
      throw new ApiError(409, 'auth_superseded', '会话已变更，改密请求已失效');
    }
    const session: StoredSession = {
      access_token: res.access_token,
      refresh_token: res.refresh_token,
      user: res.user,
    };
    authGeneration += 1;
    saveStoredSession(session);
    return session;
  },

  async getHealth(): Promise<HealthResponse> {
    if (getApiMode() === 'mock') {
      return mockServer.getHealth();
    }
    return realRequest<HealthResponse>('/api/v1/health', {}, { auth: false, retryOn401: false });
  },

  async getPkiCa(): Promise<PkiCaInfo | null> {
    if (getApiMode() === 'mock') {
      return mockServer.getPkiCa();
    }
    try {
      return await realRequest<PkiCaInfo>('/api/v1/pki/ca', {}, { auth: false, retryOn401: false });
    } catch {
      return null;
    }
  },

  async getStats(): Promise<StatsResponse> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.getStats(tokenOrNull()));
    }
    return realRequest<StatsResponse>('/api/v1/stats');
  },

  async listUsers(query: UsersListQuery): Promise<UsersListResponse> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.listUsers(tokenOrNull(), query));
    }
    const qs = buildQueryString({
      q: query.q,
      role: query.role && query.role !== 'all' ? query.role : undefined,
      status: query.status && query.status !== 'all' ? query.status : undefined,
      limit: query.limit,
      offset: query.offset,
    });
    return realRequest<UsersListResponse>(`/api/v1/admin/users${qs}`);
  },

  async createUser(payload: CreateUserPayload): Promise<AdminUser> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.createUser(tokenOrNull(), payload));
    }
    const res = await realRequest<AdminUser | { user: AdminUser }>('/api/v1/admin/users', {
      method: 'POST',
      body: JSON.stringify(payload),
    });
    return 'user' in res ? res.user : res;
  },

  async updateUser(id: string, payload: UpdateUserPayload): Promise<AdminUser> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.updateUser(tokenOrNull(), id, payload));
    }
    const res = await realRequest<AdminUser | { user: AdminUser }>(
      `/api/v1/admin/users/${encodeURIComponent(id)}`,
      {
        method: 'PATCH',
        body: JSON.stringify(payload),
      },
    );
    return 'user' in res ? res.user : res;
  },

  async deleteUser(id: string): Promise<void> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.deleteUser(tokenOrNull(), id));
    }
    return realRequest<void>(`/api/v1/admin/users/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    });
  },

  async getSettings(): Promise<AdminSettings> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.getSettings(tokenOrNull()));
    }
    return realRequest<AdminSettings>('/api/v1/admin/settings');
  },

  async updateSettings(allowRegistration: boolean): Promise<AdminSettings> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.updateSettings(tokenOrNull(), allowRegistration));
    }
    return realRequest<AdminSettings>('/api/v1/admin/settings', {
      method: 'PATCH',
      body: JSON.stringify({ allow_registration: allowRegistration }),
    });
  },

  async listDevices(query: DevicesQuery): Promise<DevicesResponse> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.listDevices(tokenOrNull(), query));
    }
    const qs = buildQueryString({
      q: query.q,
      status: query.status && query.status !== 'all' ? query.status : undefined,
      limit: query.limit,
      offset: query.offset,
    });
    return realRequest<DevicesResponse>(`/api/v1/devices${qs}`);
  },

  async disableDevice(deviceId: string): Promise<void> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.disableDevice(tokenOrNull(), deviceId));
    }
    return realRequest<void>(`/api/v1/devices/${encodeURIComponent(deviceId)}`, {
      method: 'DELETE',
    });
  },

  async listSessions(): Promise<SessionsResponse> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.listSessions(tokenOrNull()));
    }
    return realRequest<SessionsResponse>('/api/v1/sessions');
  },

  async kickSession(sessionId: string): Promise<void> {
    if (getApiMode() === 'mock') {
      return runMock(() => mockServer.kickSession(tokenOrNull(), sessionId));
    }
    return realRequest<void>(`/api/v1/sessions/${encodeURIComponent(sessionId)}`, {
      method: 'DELETE',
    });
  },
};
