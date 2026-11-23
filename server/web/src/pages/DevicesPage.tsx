import React, { useCallback, useEffect, useState } from 'react';
import {
  AlertTriangle,
  Ban,
  ChevronLeft,
  ChevronRight,
  MonitorSmartphone,
  RefreshCw,
  Search,
} from 'lucide-react';
import { ApiError, apiClient } from '../api/client';
import { Modal } from '../components/Modal';
import { useToast } from '../components/Toast';
import type { DeviceView } from '../types/api';
import { formatDateTime, formatDeviceId, formatRelativeTime } from '../utils/format';

const PAGE_SIZE = 15;

export function DevicesPage() {
  const { pushToast } = useToast();
  const [devices, setDevices] = useState<DeviceView[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [searchInput, setSearchInput] = useState('');
  const [queryText, setQueryText] = useState('');
  const [statusFilter, setStatusFilter] = useState('all');
  const [loading, setLoading] = useState(true);

  const [disableTarget, setDisableTarget] = useState<DeviceView | null>(null);
  const [disabling, setDisabling] = useState(false);

  const loadDevices = useCallback(async () => {
    setLoading(true);
    try {
      const res = await apiClient.listDevices({
        q: queryText || undefined,
        status: statusFilter,
        limit: PAGE_SIZE,
        offset,
      });
      setDevices(res.devices);
      setTotal(res.total_count ?? res.total);
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '加载设备列表失败';
      pushToast('error', '加载设备失败', msg);
    } finally {
      setLoading(false);
    }
  }, [queryText, statusFilter, offset, pushToast]);

  useEffect(() => {
    void loadDevices();
  }, [loadDevices]);

  const handleSearch = (e: React.FormEvent) => {
    e.preventDefault();
    setOffset(0);
    setQueryText(searchInput.trim());
  };

  const handleConfirmDisable = async () => {
    if (!disableTarget) return;
    setDisabling(true);
    try {
      await apiClient.disableDevice(disableTarget.device_id);
      pushToast(
        'success',
        '设备已禁用并踢线',
        `设备 ${formatDeviceId(disableTarget.device_id)} 的信令与中继会话已终止`,
      );
      setDisableTarget(null);
      await loadDevices();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '禁用设备失败';
      pushToast('error', '操作失败', msg);
    } finally {
      setDisabling(false);
    }
  };

  const hasNext = offset + PAGE_SIZE < total;

  return (
    <>
      <div className="card">
        <div className="card-header">
          <div>
            <h3 className="card-title">注册设备列表</h3>
            <p className="card-subtitle">
              查看设备 9 位 ID、主机名、归属用户邮箱、网络探测地址及在线状态
            </p>
          </div>
          <button type="button" className="btn btn-secondary btn-sm" onClick={() => void loadDevices()}>
            <RefreshCw size={14} />
            刷新
          </button>
        </div>

        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid var(--rc-border)',
            background: 'var(--rc-surface-subtle)',
          }}
        >
          <form onSubmit={handleSearch} style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
            <div className="input-with-icon" style={{ minWidth: 240, maxWidth: 340, flex: 1 }}>
              <span className="input-icon-left">
                <Search size={15} />
              </span>
              <input
                type="text"
                className="input input-sm has-left-icon"
                placeholder="搜索设备 ID、主机名或归属邮箱..."
                value={searchInput}
                onChange={(e) => setSearchInput(e.target.value)}
              />
            </div>

            <select
              className="select select-sm"
              style={{ width: 145 }}
              value={statusFilter}
              onChange={(e) => {
                setStatusFilter(e.target.value);
                setOffset(0);
              }}
              aria-label="设备状态筛选"
            >
              <option value="all">全部状态</option>
              <option value="online">在线 (online)</option>
              <option value="in_session">会话中 (in_session)</option>
              <option value="offline">离线 (offline)</option>
            </select>

            <button type="submit" className="btn btn-secondary btn-sm">
              查询
            </button>
          </form>
        </div>

        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>设备 ID (9位)</th>
                <th>主机与系统</th>
                <th>所属用户</th>
                <th>网络探测端点</th>
                <th>状态</th>
                <th>最近活跃</th>
                <th style={{ textAlign: 'right' }}>操作</th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <tr>
                  <td colSpan={7} style={{ textAlign: 'center', padding: 36, color: 'var(--rc-text-muted)' }}>
                    正在加载设备列表...
                  </td>
                </tr>
              ) : devices.length === 0 ? (
                <tr>
                  <td colSpan={7} style={{ textAlign: 'center', padding: 36, color: 'var(--rc-text-muted)' }}>
                    暂无匹配的设备记录
                  </td>
                </tr>
              ) : (
                devices.map((d) => (
                  <tr key={d.device_id}>
                    <td>
                      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                        <MonitorSmartphone size={16} color="var(--rc-primary)" />
                        <span className="mono" style={{ fontWeight: 700, fontSize: 13.5 }}>
                          {d.formatted_id || formatDeviceId(d.device_id)}
                        </span>
                      </div>
                      {d.alias && (
                        <div style={{ fontSize: 12, color: 'var(--rc-text-muted)', marginTop: 2 }}>
                          别名：{d.alias}
                        </div>
                      )}
                    </td>
                    <td>
                      <div style={{ fontWeight: 600 }}>{d.hostname || '未命名主机'}</div>
                      <div style={{ fontSize: 12, color: 'var(--rc-text-muted)' }}>
                        {d.os || '未知系统'}
                        {d.device_group ? ` · ${d.device_group}` : ''}
                      </div>
                    </td>
                    <td>
                      {d.owner_email ? (
                        <span className="mono" style={{ fontSize: 12.5 }}>
                          {d.owner_email}
                        </span>
                      ) : (
                        <span style={{ fontSize: 12.5, color: 'var(--rc-text-muted)' }}>未绑定账号</span>
                      )}
                    </td>
                    <td>
                      <div className="mono" style={{ fontSize: 12 }}>
                        {d.public_udp_addr || '—'}
                      </div>
                      {d.lan_addrs.length > 0 && (
                        <div className="mono" style={{ fontSize: 11.5, color: 'var(--rc-text-muted)' }}>
                          LAN: {d.lan_addrs.join(', ')}
                        </div>
                      )}
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
                        <span className="badge badge-neutral">离线</span>
                      )}
                    </td>
                    <td>
                      <div style={{ fontSize: 12.5 }}>{formatRelativeTime(d.last_seen_at)}</div>
                      <div style={{ fontSize: 11.5, color: 'var(--rc-text-muted)' }}>
                        {formatDateTime(d.last_seen_at).slice(0, 16)}
                      </div>
                    </td>
                    <td style={{ textAlign: 'right' }}>
                      <button
                        type="button"
                        className="btn btn-danger-outline btn-sm"
                        disabled={d.disabled}
                        onClick={() => setDisableTarget(d)}
                      >
                        <Ban size={13} />
                        {d.disabled ? '已禁用' : '禁用并踢线'}
                      </button>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>

        <div
          style={{
            padding: '12px 20px',
            borderTop: '1px solid var(--rc-border)',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            fontSize: 13,
            color: 'var(--rc-text-muted)',
          }}
        >
          <span>当前展示 {devices.length} 台设备（偏移 {offset}）</span>
          <div style={{ display: 'flex', gap: 8 }}>
            <button
              type="button"
              className="btn btn-secondary btn-sm"
              disabled={offset === 0}
              onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
            >
              <ChevronLeft size={14} />
              上一页
            </button>
            <button
              type="button"
              className="btn btn-secondary btn-sm"
              disabled={!hasNext}
              onClick={() => setOffset(offset + PAGE_SIZE)}
            >
              下一页
              <ChevronRight size={14} />
            </button>
          </div>
        </div>
      </div>

      <Modal
        open={Boolean(disableTarget)}
        title="确认禁用并踢下线设备"
        maxWidth={450}
        onClose={() => !disabling && setDisableTarget(null)}
        footer={
          <>
            <button
              type="button"
              className="btn btn-secondary"
              onClick={() => setDisableTarget(null)}
              disabled={disabling}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-danger"
              onClick={() => void handleConfirmDisable()}
              disabled={disabling}
            >
              {disabling ? '执行中...' : '确认禁用并踢线'}
            </button>
          </>
        }
      >
        {disableTarget && (
          <div className="alert alert-danger">
            <AlertTriangle size={18} style={{ flexShrink: 0, marginTop: 1 }} />
            <div>
              确定要禁用设备{' '}
              <span className="mono" style={{ fontWeight: 700 }}>
                {disableTarget.formatted_id || formatDeviceId(disableTarget.device_id)}
              </span>{' '}
              （{disableTarget.hostname || '未命名'}）吗？系统将立即断开该设备的信令连接并终止其所有活跃中继会话。
            </div>
          </div>
        )}
      </Modal>
    </>
  );
}
