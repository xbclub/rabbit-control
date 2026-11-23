import { useCallback, useEffect, useState } from 'react';
import { Activity, AlertTriangle, RefreshCw, Unplug } from 'lucide-react';
import { ApiError, apiClient } from '../api/client';
import { Modal } from '../components/Modal';
import { useToast } from '../components/Toast';
import type { RelaySessionInfo } from '../types/api';
import { formatBytes, formatDateTime, formatDeviceId, formatDuration } from '../utils/format';

export function SessionsPage() {
  const { pushToast } = useToast();
  const [sessions, setSessions] = useState<RelaySessionInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [kickTarget, setKickTarget] = useState<RelaySessionInfo | null>(null);
  const [kicking, setKicking] = useState(false);

  const loadSessions = useCallback(
    async (silent = false) => {
      if (!silent) setLoading(true);
      try {
        const res = await apiClient.listSessions();
        setSessions(res.sessions);
      } catch (err) {
        const msg = err instanceof ApiError ? err.message : '获取中继会话失败';
        if (!silent) pushToast('error', '加载失败', msg);
      } finally {
        setLoading(false);
      }
    },
    [pushToast],
  );

  useEffect(() => {
    void loadSessions();
  }, [loadSessions]);

  useEffect(() => {
    if (!autoRefresh) return;
    const timer = window.setInterval(() => {
      void loadSessions(true);
    }, 5000);
    return () => window.clearInterval(timer);
  }, [autoRefresh, loadSessions]);

  const handleConfirmKick = async () => {
    if (!kickTarget) return;
    setKicking(true);
    try {
      await apiClient.kickSession(kickTarget.session_id);
      pushToast('success', '会话已强断', `已断开中继会话 ${kickTarget.session_id}`);
      setKickTarget(null);
      await loadSessions();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '强断会话失败';
      pushToast('error', '操作失败', msg);
    } finally {
      setKicking(false);
    }
  };

  return (
    <>
      <div className="card">
        <div className="card-header" style={{ flexWrap: 'wrap' }}>
          <div>
            <h3 className="card-title">活跃中继会话（:21117 mTLS Relay）</h3>
            <p className="card-subtitle">
              当前共 <span className="tabular" style={{ fontWeight: 600 }}>{sessions.length}</span> 条活跃会话，支持实时字节流量观测与一键强断
            </p>
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
            <label
              style={{
                display: 'inline-flex',
                alignItems: 'center',
                gap: 6,
                fontSize: 13,
                color: 'var(--rc-text-secondary)',
                cursor: 'pointer',
              }}
            >
              <input
                type="checkbox"
                checked={autoRefresh}
                onChange={(e) => setAutoRefresh(e.target.checked)}
                style={{ accentColor: 'var(--rc-primary)' }}
              />
              每 5 秒自动刷新
            </label>
            <button type="button" className="btn btn-secondary btn-sm" onClick={() => void loadSessions()}>
              <RefreshCw size={14} />
              刷新
            </button>
          </div>
        </div>

        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>会话 ID</th>
                <th>主控端 (Client)</th>
                <th>被控端 (Host)</th>
                <th>转发引擎</th>
                <th>持续时长</th>
                <th>实时流量 (上行 / 下行)</th>
                <th>建立时间</th>
                <th style={{ textAlign: 'right' }}>操作</th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <tr>
                  <td colSpan={8} style={{ textAlign: 'center', padding: 36, color: 'var(--rc-text-muted)' }}>
                    正在加载中继会话...
                  </td>
                </tr>
              ) : sessions.length === 0 ? (
                <tr>
                  <td colSpan={8} style={{ textAlign: 'center', padding: 40, color: 'var(--rc-text-muted)' }}>
                    <Activity size={24} style={{ opacity: 0.45, marginBottom: 6 }} />
                    <div>当前没有活跃的中继转发会话</div>
                  </td>
                </tr>
              ) : (
                sessions.map((s) => (
                  <tr key={s.session_id}>
                    <td className="mono" style={{ fontSize: 12.5, fontWeight: 600 }}>
                      {s.session_id}
                    </td>
                    <td className="mono" style={{ fontWeight: 600 }}>
                      {formatDeviceId(s.client_id)}
                    </td>
                    <td className="mono" style={{ fontWeight: 600 }}>
                      {formatDeviceId(s.host_id)}
                    </td>
                    <td>
                      <span className="badge badge-primary mono">{s.engine}</span>
                    </td>
                    <td className="tabular">{formatDuration(s.duration_sec)}</td>
                    <td className="tabular" style={{ fontSize: 12.5 }}>
                      <div>↑ {formatBytes(s.bytes_up)}</div>
                      <div style={{ color: 'var(--rc-text-muted)' }}>↓ {formatBytes(s.bytes_down)}</div>
                    </td>
                    <td style={{ fontSize: 12.5, color: 'var(--rc-text-muted)' }}>
                      {formatDateTime(s.started_at)}
                    </td>
                    <td style={{ textAlign: 'right' }}>
                      <button
                        type="button"
                        className="btn btn-danger-outline btn-sm"
                        onClick={() => setKickTarget(s)}
                      >
                        <Unplug size={13} />
                        强断会话
                      </button>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </div>

      <Modal
        open={Boolean(kickTarget)}
        title="确认强断中继会话"
        maxWidth={440}
        onClose={() => !kicking && setKickTarget(null)}
        footer={
          <>
            <button
              type="button"
              className="btn btn-secondary"
              onClick={() => setKickTarget(null)}
              disabled={kicking}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-danger"
              onClick={() => void handleConfirmKick()}
              disabled={kicking}
            >
              {kicking ? '正在断开...' : '确认强断'}
            </button>
          </>
        }
      >
        {kickTarget && (
          <div className="alert alert-danger">
            <AlertTriangle size={18} style={{ flexShrink: 0, marginTop: 1 }} />
            <div>
              即将强行终止中继会话 <span className="mono">{kickTarget.session_id}</span>（主控{' '}
              <span className="mono">{formatDeviceId(kickTarget.client_id)}</span> → 被控{' '}
              <span className="mono">{formatDeviceId(kickTarget.host_id)}</span>），正在进行的远程桌面传输将立即中断。
            </div>
          </div>
        )}
      </Modal>
    </>
  );
}
