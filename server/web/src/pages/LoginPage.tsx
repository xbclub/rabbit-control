import React, { useState } from 'react';
import { AlertCircle, Eye, EyeOff, FlaskConical, Lock, Mail, Server } from 'lucide-react';
import { ApiError, apiClient, type StoredSession } from '../api/client';
import { BrandMark } from '../components/BrandMark';
import { useToast } from '../components/Toast';
import type { ApiMode } from '../types/api';
import { isValidEmail } from '../utils/format';

interface LoginPageProps {
  mode: ApiMode;
  onModeChange: (mode: ApiMode) => void;
  onLoginSuccess: (session: StoredSession) => void;
  initialNotice?: string | null;
}

export function LoginPage({
  mode,
  onModeChange,
  onLoginSuccess,
  initialNotice,
}: LoginPageProps) {
  const { pushToast } = useToast();
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [showPassword, setShowPassword] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [errorMsg, setErrorMsg] = useState<string | null>(initialNotice ?? null);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setErrorMsg(null);

    if (!isValidEmail(email)) {
      setErrorMsg('请输入有效的管理员邮箱地址');
      return;
    }
    if (!password) {
      setErrorMsg('请输入登录密码');
      return;
    }

    setSubmitting(true);
    try {
      const session = await apiClient.login(email, password);
      if (session.user.must_change_password) {
        pushToast('info', '首次登录安全校验', '请先修改初始密码后再进入控制台');
      } else {
        pushToast('success', '登录成功', `欢迎回来，${session.user.display_name || session.user.email}`);
      }
      onLoginSuccess(session);
    } catch (err) {
      if (err instanceof ApiError) {
        setErrorMsg(err.message);
      } else {
        setErrorMsg('登录失败，请稍后重试');
      }
    } finally {
      setSubmitting(false);
    }
  };

  const fillDemo = (demoEmail: string, demoPwd: string) => {
    setEmail(demoEmail);
    setPassword(demoPwd);
    setErrorMsg(null);
  };

  return (
    <div className="auth-page">
      <div className="auth-card">
        <div className="auth-brand">
          <BrandMark size={40} />
          <div>
            <div style={{ fontSize: 18, fontWeight: 700, letterSpacing: '-0.01em' }}>
              RabbitControl Admin
            </div>
            <div style={{ fontSize: 12.5, color: 'var(--rc-text-muted)' }}>
              远程桌面控制服务端运维控制台
            </div>
          </div>
        </div>

        <h1 style={{ margin: '0 0 6px', fontSize: 20, fontWeight: 700 }}>管理员登录</h1>
        <p style={{ margin: '0 0 20px', fontSize: 13, color: 'var(--rc-text-muted)' }}>
          使用管理员邮箱与密码登录；首次使用初始凭证将强制引导修改密码。
        </p>

        {errorMsg && (
          <div className="alert alert-danger" style={{ marginBottom: 16 }}>
            <AlertCircle size={17} style={{ flexShrink: 0, marginTop: 1 }} />
            <span>{errorMsg}</span>
          </div>
        )}

        <form onSubmit={handleSubmit} style={{ display: 'flex', flexDirection: 'column', gap: 15 }}>
          <div className="form-field">
            <label className="form-label" htmlFor="login-email">
              管理员邮箱
            </label>
            <div className="input-with-icon">
              <span className="input-icon-left">
                <Mail size={16} />
              </span>
              <input
                id="login-email"
                type="email"
                autoComplete="username"
                className="input has-left-icon"
                placeholder="admin@rabbitcontrol.local"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                disabled={submitting}
              />
            </div>
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="login-password">
              登录密码
            </label>
            <div className="input-with-icon">
              <span className="input-icon-left">
                <Lock size={16} />
              </span>
              <input
                id="login-password"
                type={showPassword ? 'text' : 'password'}
                autoComplete="current-password"
                className="input has-left-icon has-right-action"
                placeholder="请输入密码"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                disabled={submitting}
              />
              <button
                type="button"
                className="input-right-btn"
                onClick={() => setShowPassword((v) => !v)}
                aria-label={showPassword ? '隐藏密码' : '显示密码'}
              >
                {showPassword ? <EyeOff size={16} /> : <Eye size={16} />}
              </button>
            </div>
          </div>

          <button
            type="submit"
            className="btn btn-primary btn-lg"
            disabled={submitting}
            style={{ width: '100%', marginTop: 4 }}
          >
            {submitting ? '正在验证凭证...' : '登录控制台'}
          </button>
        </form>

        <div
          style={{
            marginTop: 22,
            paddingTop: 16,
            borderTop: '1px solid var(--rc-border)',
            display: 'flex',
            flexDirection: 'column',
            gap: 10,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
            <span style={{ fontSize: 12, color: 'var(--rc-text-muted)', display: 'inline-flex', alignItems: 'center', gap: 6 }}>
              {mode === 'mock' ? (
                <>
                  <FlaskConical size={14} color="#D97706" />
                  <span>当前：内置演示预览模式（Mock）</span>
                </>
              ) : (
                <>
                  <Server size={14} color="var(--rc-primary)" />
                  <span>当前：连接真实服务端 <code className="mono">/api/v1</code></span>
                </>
              )}
            </span>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              style={{ height: 26, fontSize: 12, color: 'var(--rc-primary)' }}
              disabled={submitting}
              onClick={() => onModeChange(mode === 'real' ? 'mock' : 'real')}
            >
              {mode === 'real' ? '切换演示模式' : '切回真实接口'}
            </button>
          </div>

          {mode === 'mock' && (
            <div
              style={{
                background: 'var(--rc-surface-subtle)',
                border: '1px solid var(--rc-border)',
                borderRadius: 8,
                padding: '10px 12px',
                display: 'flex',
                flexDirection: 'column',
                gap: 8,
              }}
            >
              <div style={{ fontSize: 12, fontWeight: 600, color: 'var(--rc-text-secondary)' }}>
                演示模式快捷填入（仅本地 Mock 数据）：
              </div>
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <button
                  type="button"
                  className="btn btn-secondary btn-sm"
                  onClick={() => fillDemo('admin@rabbitcontrol.local', 'InitAdmin#2026!')}
                >
                  初始管理员 (触发强制改密)
                </button>
                <button
                  type="button"
                  className="btn btn-secondary btn-sm"
                  onClick={() => fillDemo('ops@rabbitcontrol.local', 'OpsAdmin#2026!')}
                >
                  常规运维管理员
                </button>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
