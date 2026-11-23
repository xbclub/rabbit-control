import { useCallback, useEffect, useState } from 'react';
import { LogOut, ShieldAlert } from 'lucide-react';
import {
  apiClient,
  getApiMode,
  getStoredSession,
  saveStoredSession,
  setApiMode,
  setAuthEventHandlers,
  type StoredSession,
} from './api/client';
import { AdminLayout, type NavTab } from './components/AdminLayout';
import { ToastProvider, useToast } from './components/Toast';
import { DashboardPage } from './pages/DashboardPage';
import { DevicesPage } from './pages/DevicesPage';
import { ForceChangePasswordPage } from './pages/ForceChangePasswordPage';
import { LoginPage } from './pages/LoginPage';
import { SessionsPage } from './pages/SessionsPage';
import { SettingsPage } from './pages/SettingsPage';
import { UsersPage } from './pages/UsersPage';
import type { ApiMode } from './types/api';

function AppInner() {
  const { pushToast } = useToast();
  const [mode, setModeState] = useState<ApiMode>(() => getApiMode());
  const [session, setSession] = useState<StoredSession | null>(() => getStoredSession());
  const [activeTab, setActiveTab] = useState<NavTab>('dashboard');
  const [authNotice, setAuthNotice] = useState<string | null>(null);
  const [pwdChangeLocked, setPwdChangeLocked] = useState<boolean>(
    () => Boolean(getStoredSession()?.user.must_change_password),
  );

  useEffect(() => {
    setAuthEventHandlers({
      onSessionUpdated: (next) => {
        setSession(next);
        setPwdChangeLocked(Boolean(next?.user.must_change_password));
      },
      onAuthExpired: (message) => {
        setSession(null);
        setPwdChangeLocked(false);
        setAuthNotice(message);
        pushToast('error', '登录已过期', message);
      },
      onPasswordChangeRequired: () => {
        setPwdChangeLocked(true);
      },
    });
  }, [pushToast]);

  useEffect(() => {
    if (!session) return;
    let cancelled = false;
    void apiClient
      .getMe()
      .then((me) => {
        if (cancelled) return;
        setPwdChangeLocked(Boolean(me.must_change_password));
      })
      .catch(() => {
        // 401/403 handled via auth event handlers
      });
    return () => {
      cancelled = true;
    };
    // Verify identity once on boot or mode switch when a session exists
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode]);

  const handleModeChange = useCallback((nextMode: ApiMode) => {
    setApiMode(nextMode);
    setModeState(nextMode);
    saveStoredSession(null);
    setSession(null);
    setPwdChangeLocked(false);
    setAuthNotice(null);
  }, []);

  const handleLoginSuccess = useCallback((nextSession: StoredSession) => {
    setAuthNotice(null);
    setSession(nextSession);
    setPwdChangeLocked(Boolean(nextSession.user.must_change_password));
    setActiveTab('dashboard');
  }, []);

  const handlePasswordChanged = useCallback((nextSession: StoredSession) => {
    setSession(nextSession);
    setPwdChangeLocked(Boolean(nextSession.user.must_change_password));
    setActiveTab('dashboard');
  }, []);

  const handleLogout = useCallback(async () => {
    await apiClient.logout();
    setSession(null);
    setPwdChangeLocked(false);
    setAuthNotice(null);
    pushToast('info', '已退出登录');
  }, [pushToast]);

  if (!session) {
    return (
      <LoginPage
        mode={mode}
        onModeChange={handleModeChange}
        onLoginSuccess={handleLoginSuccess}
        initialNotice={authNotice}
      />
    );
  }

  if (session.user.must_change_password || pwdChangeLocked) {
    return (
      <ForceChangePasswordPage
        user={session.user}
        onSuccess={handlePasswordChanged}
        onLogout={() => void handleLogout()}
      />
    );
  }

  if (session.user.role !== 'admin') {
    return (
      <div className="auth-page">
        <div className="auth-card" style={{ maxWidth: 460 }}>
          <div className="alert alert-danger" style={{ marginBottom: 18 }}>
            <ShieldAlert size={18} style={{ flexShrink: 0, marginTop: 1 }} />
            <div>
              <div style={{ fontWeight: 600, marginBottom: 4 }}>无权访问管理员控制台</div>
              <div>
                当前账号 <span className="mono">{session.user.email}</span> 角色为{' '}
                <span className="mono">{session.user.role}</span>，仅 <span className="mono">admin</span> 角色可访问服务端运维后台。
              </div>
            </div>
          </div>
          <button
            type="button"
            className="btn btn-primary"
            style={{ width: '100%' }}
            onClick={() => void handleLogout()}
          >
            <LogOut size={15} />
            退出并切换账号
          </button>
        </div>
      </div>
    );
  }

  return (
    <AdminLayout
      activeTab={activeTab}
      onSelectTab={setActiveTab}
      user={session.user}
      mode={mode}
      onLogout={() => void handleLogout()}
    >
      {activeTab === 'dashboard' && <DashboardPage onNavigate={setActiveTab} />}
      {activeTab === 'users' && <UsersPage currentUser={session.user} />}
      {activeTab === 'devices' && <DevicesPage />}
      {activeTab === 'sessions' && <SessionsPage />}
      {activeTab === 'settings' && <SettingsPage />}
    </AdminLayout>
  );
}

export function App() {
  return (
    <ToastProvider>
      <AppInner />
    </ToastProvider>
  );
}
