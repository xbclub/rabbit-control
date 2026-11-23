import type {
  AdminSettings,
  AdminUser,
  AuthTokensResponse,
  AuthUser,
  CreateUserPayload,
  DevicesQuery,
  DevicesResponse,
  DeviceView,
  HealthResponse,
  PkiCaInfo,
  RelaySessionInfo,
  SessionsResponse,
  StatsResponse,
  UpdateUserPayload,
  UsersListQuery,
  UsersListResponse,
} from '../types/api';
import { formatDeviceId } from '../utils/format';

export class MockApiError extends Error {
  status: number;
  code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

interface MockStoredUser extends AdminUser {
  password: string;
}

const now = Date.now();
const minsAgo = (m: number) => new Date(now - m * 60_000).toISOString();
const hoursAgo = (h: number) => new Date(now - h * 3600_000).toISOString();
const daysAgo = (d: number) => new Date(now - d * 86400_000).toISOString();

function createInitialState() {
  const users: MockStoredUser[] = [
    {
      id: '00000000-0000-4000-8000-000000000001',
      email: 'admin@rabbitcontrol.local',
      display_name: '初始系统管理员',
      password: 'InitAdmin#2026!',
      role: 'admin',
      status: 'active',
      must_change_password: true,
      device_count: 1,
      last_login_at: null,
      created_at: daysAgo(1),
    },
    {
      id: '00000000-0000-4000-8000-000000000002',
      email: 'ops@rabbitcontrol.local',
      display_name: '运维值班主管',
      password: 'OpsAdmin#2026!',
      role: 'admin',
      status: 'active',
      must_change_password: false,
      device_count: 2,
      last_login_at: minsAgo(12),
      created_at: daysAgo(14),
    },
    {
      id: '00000000-0000-4000-8000-000000000003',
      email: 'alice.chen@example.com',
      display_name: '陈晓雪 (研发部)',
      password: 'UserPass#2026!',
      role: 'user',
      status: 'active',
      must_change_password: false,
      device_count: 2,
      last_login_at: hoursAgo(2),
      created_at: daysAgo(9),
    },
    {
      id: '00000000-0000-4000-8000-000000000004',
      email: 'bob.wang@example.com',
      display_name: '王博 (测试组)',
      password: 'UserPass#2026!',
      role: 'user',
      status: 'active',
      must_change_password: true,
      device_count: 1,
      last_login_at: daysAgo(1),
      created_at: daysAgo(5),
    },
    {
      id: '00000000-0000-4000-8000-000000000005',
      email: 'contractor@example.com',
      display_name: '外包临时账号',
      password: 'UserPass#2026!',
      role: 'user',
      status: 'disabled',
      must_change_password: false,
      device_count: 0,
      last_login_at: daysAgo(7),
      created_at: daysAgo(21),
    },
  ];

  const devices: DeviceView[] = [
    {
      device_id: '482910375',
      formatted_id: formatDeviceId('482910375'),
      hostname: 'MacBook-Pro-Ops',
      os: 'macOS 15.3 (arm64)',
      device_group: 'ops-core',
      disabled: false,
      online: true,
      in_session: true,
      status: 'in_session',
      public_udp_addr: '203.0.113.42:54120',
      lan_addrs: ['192.168.10.24'],
      owner_email: 'ops@rabbitcontrol.local',
      owner_user_id: '00000000-0000-4000-8000-000000000002',
      alias: '主力运维机',
      created_at: daysAgo(14),
      last_seen_at: minsAgo(0),
    },
    {
      device_id: '739201486',
      formatted_id: formatDeviceId('739201486'),
      hostname: 'BUILD-WIN-01',
      os: 'Windows 11 Pro (x64)',
      device_group: 'build-farm',
      disabled: false,
      online: true,
      in_session: true,
      status: 'in_session',
      public_udp_addr: '198.51.100.18:61204',
      lan_addrs: ['10.20.4.18'],
      owner_email: 'alice.chen@example.com',
      owner_user_id: '00000000-0000-4000-8000-000000000003',
      alias: 'Windows 编译节点',
      created_at: daysAgo(9),
      last_seen_at: minsAgo(0),
    },
    {
      device_id: '195847263',
      formatted_id: formatDeviceId('195847263'),
      hostname: 'Studio-Display-iMac',
      os: 'macOS 15.2 (arm64)',
      device_group: 'design',
      disabled: false,
      online: true,
      in_session: false,
      status: 'online',
      public_udp_addr: '203.0.113.88:49210',
      lan_addrs: ['192.168.10.57'],
      owner_email: 'alice.chen@example.com',
      owner_user_id: '00000000-0000-4000-8000-000000000003',
      alias: '工位桌面',
      created_at: daysAgo(8),
      last_seen_at: minsAgo(1),
    },
    {
      device_id: '614029384',
      formatted_id: formatDeviceId('614029384'),
      hostname: 'UBUNTU-GPU-02',
      os: 'Ubuntu 24.04 LTS (x64)',
      device_group: 'ops-core',
      disabled: false,
      online: true,
      in_session: false,
      status: 'online',
      public_udp_addr: '192.0.2.77:51822',
      lan_addrs: ['10.0.8.12'],
      owner_email: 'ops@rabbitcontrol.local',
      owner_user_id: '00000000-0000-4000-8000-000000000002',
      alias: '算力节点 02',
      created_at: daysAgo(11),
      last_seen_at: minsAgo(2),
    },
    {
      device_id: '305918274',
      formatted_id: formatDeviceId('305918274'),
      hostname: 'QA-WIN-LAPTOP',
      os: 'Windows 11 Enterprise',
      device_group: 'qa',
      disabled: false,
      online: false,
      in_session: false,
      status: 'offline',
      lan_addrs: [],
      owner_email: 'bob.wang@example.com',
      owner_user_id: '00000000-0000-4000-8000-000000000004',
      alias: '测试笔记本',
      created_at: daysAgo(5),
      last_seen_at: hoursAgo(6),
    },
    {
      device_id: '884712093',
      formatted_id: formatDeviceId('884712093'),
      hostname: 'LEGACY-HOST-09',
      os: 'Ubuntu 22.04 LTS',
      device_group: 'ops-core',
      disabled: true,
      online: false,
      in_session: false,
      status: 'offline',
      lan_addrs: [],
      owner_email: 'admin@rabbitcontrol.local',
      owner_user_id: '00000000-0000-4000-8000-000000000001',
      alias: '已封存节点',
      created_at: daysAgo(30),
      last_seen_at: daysAgo(4),
    },
  ];

  const sessions: RelaySessionInfo[] = [
    {
      session_id: 'rs-8f2a9c10-4b11',
      host_id: '739201486',
      client_id: '482910375',
      started_at: minsAgo(18),
      duration_sec: 1084,
      bytes_up: 148_920_320,
      bytes_down: 18_432_110,
      engine: 'tls-mtls-userspace',
    },
    {
      session_id: 'rs-3d91e704-9a02',
      host_id: '614029384',
      client_id: '195847263',
      started_at: minsAgo(4),
      duration_sec: 256,
      bytes_up: 42_190_848,
      bytes_down: 3_812_400,
      engine: 'tls-mtls-userspace',
    },
  ];

  const settings: AdminSettings = {
    allow_registration: true,
    cache_backend: 'redis',
    updated_at: hoursAgo(3),
  };

  return { users, devices, sessions, settings };
}

let state = createInitialState();
let currentUserId: string | null = null;
let currentAccessToken: string | null = null;
let currentRefreshToken: string | null = null;
const startedAtMs = Date.now() - 4 * 3600_000 - 23 * 60_000;

function delay(ms = 140): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function toAuthUser(u: MockStoredUser): AuthUser {
  return {
    id: u.id,
    email: u.email,
    display_name: u.display_name,
    role: u.role,
    status: u.status,
    must_change_password: u.must_change_password,
  };
}

function toAdminUser(u: MockStoredUser): AdminUser {
  const deviceCount = state.devices.filter((d) => d.owner_user_id === u.id).length;
  return {
    ...toAuthUser(u),
    device_count: deviceCount,
    last_login_at: u.last_login_at,
    created_at: u.created_at,
    updated_at: u.updated_at,
  };
}

function restoreMockSessionIfNeeded(token: string | null) {
  if (token && !currentAccessToken && token.startsWith('mock-access-')) {
    const savedUser = localStorage.getItem('rc_admin_session');
    if (savedUser) {
      try {
        const parsed = JSON.parse(savedUser) as {
          access_token?: string;
          user?: AuthUser;
          refresh_token?: string;
        };
        if (parsed.access_token === token && parsed.user?.id) {
          currentAccessToken = token;
          currentUserId = parsed.user.id;
          if (parsed.refresh_token && !currentRefreshToken) {
            currentRefreshToken = parsed.refresh_token;
          }
        }
      } catch {
        // ignore
      }
    }
  }
}

function requireMockAuth(token: string | null, allowPwdChangeOnly = false): MockStoredUser {
  restoreMockSessionIfNeeded(token);
  if (!token || !currentUserId || token !== currentAccessToken) {
    throw new MockApiError(401, 'unauthorized', '会话已过期或未登录');
  }
  const user = state.users.find((u) => u.id === currentUserId);
  if (!user || user.status !== 'active') {
    throw new MockApiError(401, 'unauthorized', '账号不存在或已被禁用');
  }
  if (user.must_change_password && !allowPwdChangeOnly) {
    throw new MockApiError(403, 'password_change_required', '首次登录必须先修改初始密码');
  }
  return user;
}

function requireMockAdmin(token: string | null): MockStoredUser {
  const user = requireMockAuth(token, false);
  if (user.role !== 'admin') {
    throw new MockApiError(403, 'forbidden', '仅管理员可执行该操作');
  }
  return user;
}

export const mockServer = {
  reset() {
    state = createInitialState();
    currentUserId = null;
    currentAccessToken = null;
    currentRefreshToken = null;
  },

  async login(email: string, password: string): Promise<AuthTokensResponse> {
    await delay();
    const norm = email.trim().toLowerCase();
    const user = state.users.find((u) => u.email.toLowerCase() === norm);
    if (!user || user.password !== password) {
      throw new MockApiError(401, 'invalid_credentials', '邮箱或密码不正确');
    }
    if (user.status !== 'active') {
      throw new MockApiError(403, 'account_disabled', '该账号已被管理员禁用');
    }
    user.last_login_at = new Date().toISOString();
    currentUserId = user.id;
    currentAccessToken = `mock-access-${cryptoRandom()}`;
    currentRefreshToken = `mock-refresh-${cryptoRandom()}`;
    return {
      access_token: currentAccessToken,
      refresh_token: currentRefreshToken,
      expires_in_sec: 900,
      user: toAuthUser(user),
    };
  },

  async refresh(refreshToken: string): Promise<AuthTokensResponse> {
    await delay(80);
    if (!refreshToken || !currentUserId || refreshToken !== currentRefreshToken) {
      throw new MockApiError(401, 'invalid_refresh_token', 'Refresh Token 无效或已失效');
    }
    const user = state.users.find((u) => u.id === currentUserId);
    if (!user || user.status !== 'active') {
      throw new MockApiError(401, 'unauthorized', '用户状态异常');
    }
    currentAccessToken = `mock-access-${cryptoRandom()}`;
    currentRefreshToken = `mock-refresh-${cryptoRandom()}`;
    return {
      access_token: currentAccessToken,
      refresh_token: currentRefreshToken,
      expires_in_sec: 900,
      user: toAuthUser(user),
    };
  },

  async logout(): Promise<void> {
    await delay(60);
    currentUserId = null;
    currentAccessToken = null;
    currentRefreshToken = null;
  },

  async me(token: string | null): Promise<AuthUser> {
    await delay(60);
    const user = requireMockAuth(token, true);
    return toAuthUser(user);
  },

  async changePassword(
    token: string | null,
    currentPassword: string,
    newPassword: string,
  ): Promise<AuthTokensResponse> {
    await delay();
    const user = requireMockAuth(token, true);
    if (user.password !== currentPassword) {
      throw new MockApiError(400, 'invalid_password', '当前密码校验失败');
    }
    if (newPassword.length < 8 || newPassword.length > 128) {
      throw new MockApiError(400, 'invalid_input', '新密码长度需在 8 到 128 位之间');
    }
    if (newPassword === currentPassword) {
      throw new MockApiError(400, 'invalid_input', '新密码不能与当前密码相同');
    }
    user.password = newPassword;
    user.must_change_password = false;
    user.updated_at = new Date().toISOString();
    currentAccessToken = `mock-access-${cryptoRandom()}`;
    currentRefreshToken = `mock-refresh-${cryptoRandom()}`;
    return {
      access_token: currentAccessToken,
      refresh_token: currentRefreshToken,
      expires_in_sec: 900,
      user: toAuthUser(user),
    };
  },

  async getHealth(): Promise<HealthResponse> {
    await delay(60);
    return {
      status: 'ok',
      postgres: 'ok',
      ephemeral_backend: state.settings.cache_backend,
      relay_engine: 'tls-mtls-userspace',
      uptime_sec: Math.floor((Date.now() - startedAtMs) / 1000),
      version: '0.2.0',
      tls_enabled: true,
      ca_fingerprint_sha256: 'a9c4e18b72d044f18e6b920d3b5f4120c9a87312e4f09d81a2c3b4d5e6f70819',
      allow_registration: state.settings.allow_registration,
      cache_backend: state.settings.cache_backend,
    };
  },

  async getStats(token: string | null): Promise<StatsResponse> {
    await delay(80);
    requireMockAdmin(token);
    const onlineCount = state.devices.filter((d) => d.online || d.in_session).length;
    const up = state.sessions.reduce((acc, s) => acc + s.bytes_up, 4_294_967_296);
    const down = state.sessions.reduce((acc, s) => acc + s.bytes_down, 1_073_741_824);
    return {
      online_count: onlineCount,
      registered_count: state.devices.length,
      active_relay_sessions: state.sessions.length,
      total_relay_sessions: 128 + state.sessions.length,
      total_relay_bytes: up + down,
      total_bytes_up: up,
      total_bytes_down: down,
      punch_coord_count: 1420,
      ephemeral_backend: state.settings.cache_backend,
      relay_engine: 'tls-mtls-userspace',
      uptime_sec: Math.floor((Date.now() - startedAtMs) / 1000),
    };
  },

  async listUsers(token: string | null, query: UsersListQuery): Promise<UsersListResponse> {
    await delay(90);
    requireMockAdmin(token);
    const q = (query.q || '').trim().toLowerCase();
    const role = (query.role || '').trim().toLowerCase();
    const status = (query.status || '').trim().toLowerCase();
    const limit = query.limit ?? 10;
    const offset = query.offset ?? 0;

    const filtered = state.users
      .filter((u) => {
        if (q && !u.email.toLowerCase().includes(q) && !u.display_name.toLowerCase().includes(q)) {
          return false;
        }
        if (role && role !== 'all' && u.role !== role) return false;
        if (status && status !== 'all' && u.status !== status) return false;
        return true;
      })
      .map(toAdminUser);

    return {
      users: filtered.slice(offset, offset + limit),
      total: filtered.length,
      limit,
      offset,
    };
  },

  async createUser(token: string | null, payload: CreateUserPayload): Promise<AdminUser> {
    await delay();
    requireMockAdmin(token);
    const email = payload.email.trim().toLowerCase();
    if (state.users.some((u) => u.email.toLowerCase() === email)) {
      throw new MockApiError(409, 'email_conflict', `邮箱 ${email} 已存在`);
    }
    const created: MockStoredUser = {
      id: `00000000-0000-4000-8000-${String(Date.now()).slice(-12)}`,
      email,
      display_name: payload.display_name.trim() || email.split('@')[0],
      password: payload.password,
      role: payload.role,
      status: 'active',
      must_change_password: payload.must_change_password,
      device_count: 0,
      last_login_at: null,
      created_at: new Date().toISOString(),
    };
    state.users.unshift(created);
    return toAdminUser(created);
  },

  async updateUser(
    token: string | null,
    id: string,
    payload: UpdateUserPayload,
  ): Promise<AdminUser> {
    await delay();
    const actor = requireMockAdmin(token);
    const target = state.users.find((u) => u.id === id);
    if (!target) {
      throw new MockApiError(404, 'user_not_found', '目标用户不存在');
    }

    const activeAdmins = state.users.filter((u) => u.role === 'admin' && u.status === 'active');
    const isLastActiveAdmin =
      activeAdmins.length <= 1 && target.role === 'admin' && target.status === 'active';
    if (
      isLastActiveAdmin &&
      ((payload.status && payload.status !== 'active') || (payload.role && payload.role !== 'admin'))
    ) {
      throw new MockApiError(409, 'last_admin_protected', '禁止禁用或降权最后一个活跃管理员');
    }

    if (payload.email !== undefined) {
      const norm = payload.email.trim().toLowerCase();
      if (state.users.some((u) => u.id !== id && u.email.toLowerCase() === norm)) {
        throw new MockApiError(409, 'email_conflict', `邮箱 ${norm} 已被占用`);
      }
      target.email = norm;
    }
    if (payload.display_name !== undefined) {
      target.display_name = payload.display_name.trim();
    }
    if (payload.role !== undefined) {
      target.role = payload.role;
    }
    if (payload.status !== undefined) {
      target.status = payload.status;
      if (payload.status === 'disabled') {
        state.devices.forEach((d) => {
          if (d.owner_user_id === target.id) {
            d.online = false;
            d.in_session = false;
            d.status = 'offline';
          }
        });
        state.sessions = state.sessions.filter((s) => {
          const host = state.devices.find((d) => d.device_id === s.host_id);
          const client = state.devices.find((d) => d.device_id === s.client_id);
          return host?.owner_user_id !== target.id && client?.owner_user_id !== target.id;
        });
      }
    }
    if (payload.reset_password) {
      target.password = payload.reset_password;
    }
    if (payload.must_change_password !== undefined) {
      target.must_change_password = payload.must_change_password;
    }
    target.updated_at = new Date().toISOString();

    if (actor.id === target.id && target.must_change_password) {
      // reflect forced password change immediately
    }
    return toAdminUser(target);
  },

  async deleteUser(token: string | null, id: string): Promise<void> {
    await delay();
    const actor = requireMockAdmin(token);
    const idx = state.users.findIndex((u) => u.id === id);
    if (idx === -1) {
      throw new MockApiError(404, 'user_not_found', '目标用户不存在');
    }
    const target = state.users[idx];
    if (target.id === actor.id) {
      throw new MockApiError(409, 'cannot_delete_self', '不能删除当前正在登录的管理员账号');
    }
    const activeAdmins = state.users.filter((u) => u.role === 'admin' && u.status === 'active');
    if (activeAdmins.length <= 1 && target.role === 'admin' && target.status === 'active') {
      throw new MockApiError(409, 'last_admin_protected', '禁止删除最后一个活跃管理员');
    }
    state.users.splice(idx, 1);
    state.devices.forEach((d) => {
      if (d.owner_user_id === id) {
        d.owner_user_id = null;
        d.owner_email = null;
        d.online = false;
        d.in_session = false;
        d.status = 'offline';
      }
    });
  },

  async getSettings(token: string | null): Promise<AdminSettings> {
    await delay(70);
    requireMockAdmin(token);
    return { ...state.settings };
  },

  async updateSettings(
    token: string | null,
    allowRegistration: boolean,
  ): Promise<AdminSettings> {
    await delay(120);
    requireMockAdmin(token);
    state.settings = {
      ...state.settings,
      allow_registration: allowRegistration,
      updated_at: new Date().toISOString(),
    };
    return { ...state.settings };
  },

  async listDevices(token: string | null, query: DevicesQuery): Promise<DevicesResponse> {
    await delay(80);
    requireMockAdmin(token);
    const q = (query.q || '').trim().toLowerCase();
    const status = (query.status || 'all').toLowerCase();
    const limit = query.limit ?? 20;
    const offset = query.offset ?? 0;

    const filtered = state.devices.filter((d) => {
      if (q) {
        const hay = `${d.device_id} ${d.formatted_id} ${d.hostname} ${d.owner_email || ''} ${d.alias || ''}`.toLowerCase();
        if (!hay.includes(q)) return false;
      }
      if (status !== 'all' && d.status !== status) return false;
      return true;
    });

    return {
      devices: filtered.slice(offset, offset + limit),
      total: filtered.length,
      limit,
      offset,
    };
  },

  async disableDevice(token: string | null, deviceId: string): Promise<void> {
    await delay(100);
    requireMockAdmin(token);
    const dev = state.devices.find((d) => d.device_id === deviceId);
    if (!dev) {
      throw new MockApiError(404, 'device_not_found', '设备不存在');
    }
    dev.disabled = true;
    dev.online = false;
    dev.in_session = false;
    dev.status = 'offline';
    state.sessions = state.sessions.filter(
      (s) => s.host_id !== deviceId && s.client_id !== deviceId,
    );
  },

  async listSessions(token: string | null): Promise<SessionsResponse> {
    await delay(70);
    requireMockAdmin(token);
    return {
      sessions: state.sessions.map((s) => ({ ...s })),
      total: state.sessions.length,
    };
  },

  async kickSession(token: string | null, sessionId: string): Promise<void> {
    await delay(90);
    requireMockAdmin(token);
    const idx = state.sessions.findIndex((s) => s.session_id === sessionId);
    if (idx === -1) {
      throw new MockApiError(404, 'session_not_found', '中继会话已结束或不存在');
    }
    const [removed] = state.sessions.splice(idx, 1);
    state.devices.forEach((d) => {
      if (d.device_id === removed.host_id || d.device_id === removed.client_id) {
        d.in_session = false;
        d.status = d.online ? 'online' : 'offline';
      }
    });
  },

  async getPkiCa(): Promise<PkiCaInfo> {
    await delay(50);
    return {
      ca_cert_pem: '-----BEGIN CERTIFICATE-----\n(Mock Root CA)\n-----END CERTIFICATE-----',
      ca_fingerprint_sha256: 'a9c4e18b72d044f18e6b920d3b5f4120c9a87312e4f09d81a2c3b4d5e6f70819',
      auth_pubkey_hex: '4f8d2e11a7c90b3412ef88a091bc55d2f1a09834e7c6b5a4d3e2f10987654321',
    };
  },
};

function cryptoRandom(): string {
  if (typeof crypto !== 'undefined' && crypto.randomUUID) {
    return crypto.randomUUID();
  }
  return `${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
