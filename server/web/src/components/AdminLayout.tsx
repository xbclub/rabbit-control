import React, { useState } from 'react';
import {
  Activity,
  FlaskConical,
  LayoutDashboard,
  LogOut,
  Menu,
  MonitorSmartphone,
  Settings,
  Users,
  X,
} from 'lucide-react';
import { BrandMark } from './BrandMark';
import type { ApiMode, AuthUser } from '../types/api';

export type NavTab = 'dashboard' | 'users' | 'devices' | 'sessions' | 'settings';

interface AdminLayoutProps {
  activeTab: NavTab;
  onSelectTab: (tab: NavTab) => void;
  user: AuthUser;
  mode: ApiMode;
  onLogout: () => void;
  children: React.ReactNode;
}

const NAV_ITEMS: Array<{ id: NavTab; label: string; icon: React.ComponentType<{ size?: number }> }> = [
  { id: 'dashboard', label: '运行概览', icon: LayoutDashboard },
  { id: 'users', label: '用户管理', icon: Users },
  { id: 'devices', label: '设备列表', icon: MonitorSmartphone },
  { id: 'sessions', label: '中继会话', icon: Activity },
  { id: 'settings', label: '系统设置', icon: Settings },
];

const TAB_TITLES: Record<NavTab, string> = {
  dashboard: '运行概览与监控指标',
  users: '用户与权限管理',
  devices: '注册设备与在线状态',
  sessions: '活跃中继会话监控',
  settings: '系统设置与注册策略',
};

export function AdminLayout({
  activeTab,
  onSelectTab,
  user,
  mode,
  onLogout,
  children,
}: AdminLayoutProps) {
  const [mobileOpen, setMobileOpen] = useState(false);

  const handleNav = (id: NavTab) => {
    onSelectTab(id);
    setMobileOpen(false);
  };

  return (
    <div className="app-shell">
      {mobileOpen && (
        <div
          onClick={() => setMobileOpen(false)}
          style={{
            position: 'fixed',
            inset: 0,
            background: 'rgba(15, 23, 42, 0.35)',
            zIndex: 45,
          }}
        />
      )}

      <aside className={`sidebar ${mobileOpen ? 'open' : ''}`}>
        <div className="sidebar-brand">
          <BrandMark size={32} />
          <div style={{ flex: 1, minWidth: 0 }}>
            <div className="brand-title">RabbitControl</div>
            <div className="brand-sub">Server Admin Console</div>
          </div>
          {mobileOpen && (
            <button
              type="button"
              className="btn btn-ghost btn-sm mobile-menu-btn"
              onClick={() => setMobileOpen(false)}
              style={{ padding: 4 }}
            >
              <X size={18} />
            </button>
          )}
        </div>

        <nav className="sidebar-nav" aria-label="主导航">
          <div className="nav-section-label">控制平面</div>
          {NAV_ITEMS.map((item) => {
            const Icon = item.icon;
            const active = activeTab === item.id;
            return (
              <button
                key={item.id}
                type="button"
                className={`nav-item ${active ? 'active' : ''}`}
                onClick={() => handleNav(item.id)}
              >
                <Icon size={17} />
                <span>{item.label}</span>
              </button>
            );
          })}
        </nav>

        <div className="sidebar-footer">
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <div
              style={{
                width: 34,
                height: 34,
                borderRadius: '50%',
                background: 'var(--rc-primary-soft)',
                color: 'var(--rc-primary)',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
                fontWeight: 700,
                fontSize: 13,
                flexShrink: 0,
              }}
            >
              {(user.display_name || user.email).slice(0, 1).toUpperCase()}
            </div>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div
                style={{
                  fontWeight: 600,
                  fontSize: 13,
                  whiteSpace: 'nowrap',
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                }}
              >
                {user.display_name || user.email}
              </div>
              <div
                className="mono"
                style={{
                  fontSize: 11.5,
                  color: 'var(--rc-text-muted)',
                  whiteSpace: 'nowrap',
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                }}
              >
                {user.email}
              </div>
            </div>
          </div>
        </div>
      </aside>

      <div className="main-area">
        <header className="topbar">
          <div className="topbar-title-group">
            <button
              type="button"
              className="btn btn-ghost btn-sm mobile-menu-btn"
              onClick={() => setMobileOpen(true)}
              aria-label="打开导航"
            >
              <Menu size={18} />
            </button>
            <h2 className="topbar-title">{TAB_TITLES[activeTab]}</h2>
          </div>

          <div className="topbar-right">
            {mode === 'mock' ? (
              <span className="badge badge-warning">
                <FlaskConical size={13} />
                演示数据模式
              </span>
            ) : (
              <span className="badge badge-primary">
                <span className="badge-dot" />
                Live API
              </span>
            )}
            <span className="badge badge-neutral">管理员 · {user.role}</span>
            <button type="button" className="btn btn-secondary btn-sm" onClick={onLogout}>
              <LogOut size={14} />
              退出
            </button>
          </div>
        </header>

        <main className="content-container">{children}</main>
      </div>
    </div>
  );
}
