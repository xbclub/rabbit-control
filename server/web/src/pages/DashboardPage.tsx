import { useCallback, useEffect, useState } from 'react';
import {
  Activity,
  ArrowRight,
  Clock,
  Cpu,
  Database,
  HardDrive,
  MonitorSmartphone,
  RefreshCw,
  ShieldCheck,
  Wifi,
} from 'lucide-react';
import { ApiError, apiClient } from '../api/client';
import { useToast } from '../components/Toast';
import type { NavTab } from '../components/AdminLayout';
import type { DeviceView, HealthResponse, RelaySessionInfo, StatsResponse } from '../types/api';
import { formatBytes, formatDeviceId, formatDuration, formatRelativeTime } from '../utils/format';

interface DashboardPageProps {
  onNavigate: (tab: NavTab) => void;
}

export function DashboardPage({ onNavigate }: DashboardPageProps) {
  const { pushToast } = useToast();
  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [stats, setStats] = useState<StatsResponse | null>(null);
  const [sessions, setSessions] = useState<RelaySessionInfo[]>([]);
  const [devices, setDevices] = useState<DeviceView[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshSec, setRefreshSec] = useState<number>(15);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const loadData = useCallback(async (silent = false) => {
    if (!silent) setLoading(true);
    setErrorMsg(null);
    try {
      const [h, s, sess, devs] = await Promise.all([
        apiClient.getHealth(),
        apiClient.getStats(),
        apiClient.listSessions(),
        apiClient.listDevices({ limit: 5, offset: 0 }),
      ]);
      setHealth(h);
      setStats(s);
      setSessions(sess.sessions.slice(0, 5));
      setDevices(devs.devices.slice(0, 5));
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '加载运行指标失败';
      setErrorMsg(msg);
      if (!silent) pushToast('error', '数据刷新失败', msg);
    } finally {
      setLoading(false);
    }
  }, [pushToast]);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  useEffect(() => {
    if (refreshSec <= 0) return;
    const timer = window.setInterval(() => {
      void loadData(true);
    }, refreshSec * 1000);
    return () => window.clearInterval(timer);
  }, [refreshSec, loadData]);

  const cacheBackend = stats?.ephemeral_backend || health?.cache_backend || health?.ephemeral_backend || 'memory';

  return (
    <>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          flexWrap: 'wrap',
          gap: 12,
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          {health ? (
            <span className={`badge ${health.status === 'ok' ? 'badge-success' : 'badge-warning'}`}>
              <span className="badge-dot" />
              服务端状态：{health.status === 'ok' ? '健康运行中' : '降级告警'}
            </span>
          ) : (
            <span className="badge badge-neutral">状态检测中...</span>
          )}
          {health?.version && (
            <span className="badge badge-neutral mono">v{health.version}</span>
          )}
        </div>

        <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <select
            className="select select-sm"
            style={{ width: 130 }}
            value={refreshSec}
            onChange={(e) => setRefreshSec(Number(e.target.value))}
            aria-label="自动刷新周期"
          >
            <option value={0}>手动刷新</option>
            <option value={5}>每 5 秒刷新</option>
            <option value={15}>每 15 秒刷新</option>
            <option value={30}>每 30 秒刷新</option>
          </select>
          <button
            type="button"
            className="btn btn-secondary btn-sm"
            onClick={() => void loadData()}
            disabled={loading}
          >
            <RefreshCw size={14} />
            立即刷新
          </button>
        </div>
      </div>

      {errorMsg && (
        <div className="alert alert-danger">
          <span>{errorMsg}</span>
        </div>
      )}

      <div className="kpi-grid">
        <div className="kpi-card">
          <div className="kpi-top">
            <span>在线设备数</span>
            <span className="kpi-icon">
              <MonitorSmartphone size={18} />
            </span>
          </div>
          <div className="kpi-value tabular">{stats ? stats.online_count : '—'}</div>
          <div className="kpi-sub tabular">
            已注册设备总数：{stats ? stats.registered_count : '—'}
          </div>
        </div>

        <div className="kpi-card">
          <div className="kpi-top">
            <span>活跃中继会话</span>
            <span className="kpi-icon">
              <Activity size={18} />
            </span>
          </div>
          <div className="kpi-value tabular">{stats ? stats.active_relay_sessions : '—'}</div>
          <div className="kpi-sub tabular">
            累计历史中继：{stats ? stats.total_relay_sessions : '—'} 次
          </div>
        </div>

        <div className="kpi-card">
          <div className="kpi-top">
            <span>累计中继流量</span>
            <span className="kpi-icon">
              <HardDrive size={18} />
            </span>
          </div>
          <div className="kpi-value tabular">
            {stats ? formatBytes(stats.total_relay_bytes) : '—'}
          </div>
          <div className="kpi-sub tabular">
            {stats
              ? `上行 ${formatBytes(stats.total_bytes_up)} · 下行 ${formatBytes(stats.total_bytes_down)}`
              : '—'}
          </div>
        </div>

        <div className="kpi-card">
          <div className="kpi-top">
            <span>服务运行时长</span>
            <span className="kpi-icon">
              <Clock size={18} />
            </span>
          </div>
          <div className="kpi-value tabular">
            {stats ? formatDuration(stats.uptime_sec) : '—'}
          </div>
          <div className="kpi-sub tabular">
            P2P 打洞协调：{stats ? stats.punch_coord_count : '—'} 次
          </div>
        </div>
      </div>

      <div className="card">
        <div className="card-header">
          <div>
            <h3 className="card-title">基础设施与安全引擎状态</h3>
            <p className="card-subtitle">实时反映数据库、临时态缓存引擎、mTLS 中继转发与注册策略</p>
          </div>
          <button
            type="button"
            className="btn btn-ghost btn-sm"
            onClick={() => onNavigate('settings')}
          >
            前往系统设置
            <ArrowRight size={14} />
          </button>
        </div>
        <div className="card-body">
          <div
            style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))',
              gap: 14,
            }}
          >
            <div
              style={{
                padding: '12px 14px',
                borderRadius: 8,
                border: '1px solid var(--rc-border)',
                background: 'var(--rc-surface-subtle)',
              }}
            >
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
                <Database size={14} />
                PostgreSQL 持久化层
              </div>
              <div style={{ marginTop: 8, display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <span style={{ fontWeight: 600 }}>主存储连接</span>
                <span className={`badge ${health?.postgres === 'ok' ? 'badge-success' : 'badge-danger'}`}>
                  {health?.postgres === 'ok' ? '正常' : health?.postgres || '未知'}
                </span>
              </div>
            </div>

            <div
              style={{
                padding: '12px 14px',
                borderRadius: 8,
                border: '1px solid var(--rc-border)',
                background: 'var(--rc-surface-subtle)',
              }}
            >
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
                <Cpu size={14} />
                临时态 / 策略缓存引擎
              </div>
              <div style={{ marginTop: 8, display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <span className="mono" style={{ fontWeight: 600 }}>
                  {cacheBackend.toUpperCase()}
                </span>
                <span className={`badge ${cacheBackend === 'redis' ? 'badge-primary' : 'badge-neutral'}`}>
                  {cacheBackend === 'redis' ? 'Redis 分布式缓存' : '进程内 Memory'}
                </span>
              </div>
            </div>

            <div
              style={{
                padding: '12px 14px',
                borderRadius: 8,
                border: '1px solid var(--rc-border)',
                background: 'var(--rc-surface-subtle)',
              }}
            >
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
                <Wifi size={14} />
                中继转发引擎 (:21117)
              </div>
              <div style={{ marginTop: 8, display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <span className="mono" style={{ fontWeight: 600, fontSize: 13 }}>
                  {stats?.relay_engine || health?.relay_engine || '—'}
                </span>
                <span className="badge badge-success">运行中</span>
              </div>
            </div>

            <div
              style={{
                padding: '12px 14px',
                borderRadius: 8,
                border: '1px solid var(--rc-border)',
                background: 'var(--rc-surface-subtle)',
              }}
            >
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
                <ShieldCheck size={14} />
                TLS / 自助注册策略
              </div>
              <div style={{ marginTop: 8, display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
                <span style={{ fontWeight: 600 }}>
                  {health?.allow_registration === false ? '仅管理员开户' : '允许自助注册'}
                </span>
                <span className={`badge ${health?.tls_enabled === false ? 'badge-neutral' : 'badge-primary'}`}>
                  {health?.tls_enabled === false ? 'HTTP 模式' : 'TLS 1.3'}
                </span>
              </div>
            </div>
          </div>
        </div>
      </div>

      <div className="grid-2">
        <div className="card">
          <div className="card-header">
            <div>
              <h3 className="card-title">活跃中继会话</h3>
              <p className="card-subtitle">当前正在通过服务器中转的加密会话</p>
            </div>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => onNavigate('sessions')}
            >
              管理全部
              <ArrowRight size={14} />
            </button>
          </div>
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>主控 → 被控</th>
                  <th>时长</th>
                  <th>流量 (上/下)</th>
                </tr>
              </thead>
              <tbody>
                {sessions.length === 0 ? (
                  <tr>
                    <td colSpan={3} style={{ textAlign: 'center', color: 'var(--rc-text-muted)', padding: 28 }}>
                      暂无活跃中继会话
                    </td>
                  </tr>
                ) : (
                  sessions.map((s) => (
                    <tr key={s.session_id}>
                      <td>
                        <div className="mono" style={{ fontWeight: 600, fontSize: 13 }}>
                          {formatDeviceId(s.client_id)} → {formatDeviceId(s.host_id)}
                        </div>
                        <div className="mono" style={{ fontSize: 11.5, color: 'var(--rc-text-muted)' }}>
                          {s.session_id}
                        </div>
                      </td>
                      <td className="tabular">{formatDuration(s.duration_sec)}</td>
                      <td className="tabular" style={{ fontSize: 12.5 }}>
                        ↑ {formatBytes(s.bytes_up)} / ↓ {formatBytes(s.bytes_down)}
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        </div>

        <div className="card">
          <div className="card-header">
            <div>
              <h3 className="card-title">最近活跃设备</h3>
              <p className="card-subtitle">展示设备 9 位识别码、归属用户与连接状态</p>
            </div>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => onNavigate('devices')}
            >
              查看设备表
              <ArrowRight size={14} />
            </button>
          </div>
          <div className="table-wrap">
            <table className="table">
              <thead>
                <tr>
                  <th>设备 ID / 主机名</th>
                  <th>归属用户</th>
                  <th>状态</th>
                </tr>
              </thead>
              <tbody>
                {devices.length === 0 ? (
                  <tr>
                    <td colSpan={3} style={{ textAlign: 'center', color: 'var(--rc-text-muted)', padding: 28 }}>
                      暂无设备记录
                    </td>
                  </tr>
                ) : (
                  devices.map((d) => (
                    <tr key={d.device_id}>
                      <td>
                        <div className="mono" style={{ fontWeight: 600, fontSize: 13 }}>
                          {d.formatted_id || formatDeviceId(d.device_id)}
                        </div>
                        <div style={{ fontSize: 12, color: 'var(--rc-text-muted)' }}>
                          {d.hostname || '未命名主机'} · {d.os || '未知系统'}
                        </div>
                      </td>
                      <td style={{ fontSize: 12.5, color: 'var(--rc-text-secondary)' }}>
                        {d.owner_email || '未绑定'}
                      </td>
                      <td>
                        {d.disabled ? (
                          <span className="badge badge-danger">已禁用</span>
                        ) : d.in_session ? (
                          <span className="badge badge-primary">
                            <span className="badge-dot" />
                            会话中
                          </span>
                        ) : d.online ? (
                          <span className="badge badge-success">
                            <span className="badge-dot" />
                            在线
                          </span>
                        ) : (
                          <span className="badge badge-neutral">
                            离线 · {formatRelativeTime(d.last_seen_at)}
                          </span>
                        )}
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        </div>
      </div>
    </>
  );
}
