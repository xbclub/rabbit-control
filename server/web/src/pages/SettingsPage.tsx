import { useCallback, useEffect, useState } from 'react';
import { CheckCircle2, Cpu, RefreshCw, ShieldCheck, UserPlus } from 'lucide-react';
import { ApiError, apiClient } from '../api/client';
import { useToast } from '../components/Toast';
import type { AdminSettings, PkiCaInfo } from '../types/api';
import { formatDateTime } from '../utils/format';

export function SettingsPage() {
  const { pushToast } = useToast();
  const [settings, setSettings] = useState<AdminSettings | null>(null);
  const [pkiCa, setPkiCa] = useState<PkiCaInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  const loadSettings = useCallback(async () => {
    setLoading(true);
    try {
      const [s, ca] = await Promise.all([apiClient.getSettings(), apiClient.getPkiCa()]);
      setSettings(s);
      setPkiCa(ca);
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '加载系统设置失败';
      pushToast('error', '加载失败', msg);
    } finally {
      setLoading(false);
    }
  }, [pushToast]);

  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  const handleToggleRegistration = async () => {
    if (!settings || saving) return;
    const nextValue = !settings.allow_registration;
    setSaving(true);
    try {
      const updated = await apiClient.updateSettings(nextValue);
      setSettings(updated);
      pushToast(
        'success',
        updated.allow_registration ? '已开启客户端自助注册' : '已关闭客户端自助注册',
        `配置已写入数据库并即时同步至 ${updated.cache_backend.toUpperCase()} 缓存`,
      );
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '更新注册开关失败';
      pushToast('error', '保存失败', msg);
    } finally {
      setSaving(false);
    }
  };

  const backendName = (settings?.cache_backend || 'memory').toLowerCase();
  const isRedis = backendName === 'redis';

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 18, maxWidth: 920 }}>
      <div className="card">
        <div className="card-header">
          <div>
            <h3 className="card-title">访问与注册准入策略</h3>
            <p className="card-subtitle">控制桌面客户端及外部用户是否可通过公开注册接口创建新账号</p>
          </div>
          <button
            type="button"
            className="btn btn-secondary btn-sm"
            onClick={() => void loadSettings()}
            disabled={loading}
          >
            <RefreshCw size={14} />
            刷新状态
          </button>
        </div>

        <div className="card-body">
          {loading && !settings ? (
            <div style={{ padding: '20px 0', color: 'var(--rc-text-muted)' }}>正在加载设置...</div>
          ) : settings ? (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 18 }}>
              <div
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  gap: 20,
                  padding: '18px 20px',
                  borderRadius: 10,
                  border: `1px solid ${settings.allow_registration ? 'var(--rc-primary-border)' : 'var(--rc-border)'}`,
                  background: settings.allow_registration ? '#f7faff' : 'var(--rc-surface-subtle)',
                }}
              >
                <div style={{ display: 'flex', gap: 14, alignItems: 'flex-start' }}>
                  <div
                    style={{
                      width: 40,
                      height: 40,
                      borderRadius: 10,
                      background: settings.allow_registration ? 'var(--rc-primary-soft)' : '#eef2f6',
                      color: settings.allow_registration ? 'var(--rc-primary)' : '#64748b',
                      display: 'flex',
                      alignItems: 'center',
                      justifyContent: 'center',
                      flexShrink: 0,
                    }}
                  >
                    <UserPlus size={20} />
                  </div>
                  <div>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
                      <span style={{ fontSize: 15.5, fontWeight: 600 }}>允许客户端自助注册</span>
                      {settings.allow_registration ? (
                        <span className="badge badge-success">
                          <span className="badge-dot" />
                          已开启
                        </span>
                      ) : (
                        <span className="badge badge-neutral">已关闭</span>
                      )}
                    </div>
                    <p style={{ margin: '6px 0 0', fontSize: 13, color: 'var(--rc-text-secondary)', lineHeight: 1.55 }}>
                      {settings.allow_registration
                        ? '当前允许客户端通过 POST /api/v1/auth/register 自助注册普通用户账号。关闭后仅管理员可在「用户管理」中手动开通新账号。'
                        : '当前已禁用自助注册。客户端调用 POST /api/v1/auth/register 将立即收到 403 registration_disabled 拒绝响应，不影响管理员后台开户。'}
                    </p>
                  </div>
                </div>

                <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexShrink: 0 }}>
                  <button
                    type="button"
                    role="switch"
                    aria-checked={settings.allow_registration}
                    aria-label="允许客户端自助注册开关"
                    className={`switch ${settings.allow_registration ? 'checked' : ''}`}
                    disabled={saving}
                    onClick={() => void handleToggleRegistration()}
                  >
                    <span className="switch-thumb" />
                  </button>
                </div>
              </div>

              <div className="alert alert-info">
                <CheckCircle2 size={17} style={{ flexShrink: 0, marginTop: 1 }} />
                <div>
                  <strong>即时生效机制：</strong>
                  开关变更同时持久化至 PostgreSQL <code className="mono">system_settings</code> 并刷新{' '}
                  <code className="mono">SettingsCache</code>（键{' '}
                  <code className="mono">rc:setting:allow_registration</code>），无需重启服务端进程。
                </div>
              </div>
            </div>
          ) : null}
        </div>
      </div>

      <div className="grid-2">
        <div className="card">
          <div className="card-header">
            <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
              <Cpu size={17} color="var(--rc-primary)" />
              <h3 className="card-title">临时态缓存后端状态</h3>
            </div>
            <span className={`badge ${isRedis ? 'badge-primary' : 'badge-neutral'}`}>
              {isRedis ? 'Redis' : 'Memory'}
            </span>
          </div>
          <div className="card-body" style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 13 }}>
              <span style={{ color: 'var(--rc-text-muted)' }}>当前缓存引擎</span>
              <span className="mono" style={{ fontWeight: 600 }}>
                {isRedis ? 'Redis (分布式同步)' : 'Memory (单机进程内缓存)'}
              </span>
            </div>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 13 }}>
              <span style={{ color: 'var(--rc-text-muted)' }}>最近策略更新时间</span>
              <span className="tabular">{formatDateTime(settings?.updated_at)}</span>
            </div>
            <div style={{ fontSize: 12.5, color: 'var(--rc-text-secondary)', marginTop: 4 }}>
              {isRedis
                ? '已接入 Redis 临时态存储，设备在线表、证书吊销黑名单与自助注册开关将在多节点间自动同步。'
                : '当前以零依赖进程内 MemoryEphemeralStore 运行，适合单节点部署；如需多实例水平扩展可配置 --redis-url。'}
            </div>
          </div>
        </div>

        <div className="card">
          <div className="card-header">
            <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
              <ShieldCheck size={17} color="var(--rc-primary)" />
              <h3 className="card-title">PKI 根证书与鉴权公钥</h3>
            </div>
            <span className="badge badge-success">Ed25519 + ECDSA P-256</span>
          </div>
          <div className="card-body" style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
            <div>
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', marginBottom: 4 }}>
                Root CA SHA-256 指纹
              </div>
              <div
                className="mono"
                style={{
                  fontSize: 12,
                  padding: '8px 10px',
                  borderRadius: 6,
                  background: 'var(--rc-surface-subtle)',
                  border: '1px solid var(--rc-border)',
                  wordBreak: 'break-all',
                }}
              >
                {pkiCa?.ca_fingerprint_sha256 || '（服务端未返回 CA 信息）'}
              </div>
            </div>
            <div>
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', marginBottom: 4 }}>
                Ed25519 鉴权验签公钥
              </div>
              <div
                className="mono"
                style={{
                  fontSize: 12,
                  padding: '8px 10px',
                  borderRadius: 6,
                  background: 'var(--rc-surface-subtle)',
                  border: '1px solid var(--rc-border)',
                  wordBreak: 'break-all',
                }}
              >
                {pkiCa?.auth_pubkey_hex || '—'}
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
