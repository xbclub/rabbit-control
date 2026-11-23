export type UserRole = 'admin' | 'user';
export type UserStatus = 'active' | 'disabled';

export interface AuthUser {
  id: string;
  email: string;
  display_name: string;
  role: UserRole;
  status: UserStatus;
  must_change_password: boolean;
}

export interface AuthTokensResponse {
  access_token: string;
  refresh_token: string;
  expires_in_sec?: number;
  user: AuthUser;
}

export interface AdminUser extends AuthUser {
  device_count: number;
  last_login_at: string | null;
  created_at: string;
  updated_at?: string;
}

export interface UsersListQuery {
  q?: string;
  role?: string;
  status?: string;
  limit?: number;
  offset?: number;
}

export interface UsersListResponse {
  users: AdminUser[];
  total: number;
  limit: number;
  offset: number;
}

export interface CreateUserPayload {
  email: string;
  display_name: string;
  password: string;
  role: UserRole;
  must_change_password: boolean;
}

export interface UpdateUserPayload {
  email?: string;
  display_name?: string;
  role?: UserRole;
  status?: UserStatus;
  reset_password?: string;
  must_change_password?: boolean;
}

export interface AdminSettings {
  allow_registration: boolean;
  cache_backend: string;
  updated_at: string;
}

export interface HealthResponse {
  status: string;
  postgres: string;
  ephemeral_backend: string;
  relay_engine: string;
  uptime_sec: number;
  version: string;
  tls_enabled?: boolean;
  ca_fingerprint_sha256?: string;
  allow_registration?: boolean;
  cache_backend?: string;
}

export interface StatsResponse {
  online_count: number;
  registered_count: number;
  active_relay_sessions: number;
  total_relay_sessions: number;
  total_relay_bytes: number;
  total_bytes_up: number;
  total_bytes_down: number;
  punch_coord_count: number;
  ephemeral_backend: string;
  relay_engine: string;
  uptime_sec: number;
}

export interface DeviceView {
  device_id: string;
  formatted_id: string;
  hostname: string;
  os: string;
  device_group: string;
  disabled: boolean;
  online: boolean;
  in_session: boolean;
  status: 'online' | 'in_session' | 'offline' | string;
  public_udp_addr?: string;
  lan_addrs: string[];
  owner_email?: string | null;
  owner_user_id?: string | null;
  alias?: string;
  created_at: string;
  last_seen_at: string;
}

export interface DevicesQuery {
  q?: string;
  status?: string;
  limit?: number;
  offset?: number;
}

export interface DevicesResponse {
  devices: DeviceView[];
  total: number;
  total_count?: number;
  limit: number;
  offset: number;
}

export interface RelaySessionInfo {
  session_id: string;
  host_id: string;
  client_id: string;
  started_at: string;
  duration_sec: number;
  bytes_up: number;
  bytes_down: number;
  engine: string;
}

export interface SessionsResponse {
  sessions: RelaySessionInfo[];
  total: number;
}

export interface PkiCaInfo {
  ca_cert_pem: string;
  ca_fingerprint_sha256: string;
  auth_pubkey_hex: string;
}

export interface ApiErrorBody {
  error: string;
  message: string;
}

export type ApiMode = 'real' | 'mock';
