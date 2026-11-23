import React, { useMemo, useState } from 'react';
import { AlertCircle, CheckCircle2, Eye, EyeOff, KeyRound, LogOut, ShieldAlert } from 'lucide-react';
import { ApiError, apiClient, type StoredSession } from '../api/client';
import { BrandMark } from '../components/BrandMark';
import { useToast } from '../components/Toast';
import type { AuthUser } from '../types/api';

interface ForceChangePasswordPageProps {
  user: AuthUser;
  onSuccess: (session: StoredSession) => void;
  onLogout: () => void;
}

function getPasswordStrength(pwd: string): { label: string; color: string; pct: number } {
  if (!pwd) return { label: '未输入', color: '#cbd5e1', pct: 0 };
  let score = 0;
  if (pwd.length >= 8) score += 1;
  if (pwd.length >= 12) score += 1;
  if (/[A-Z]/.test(pwd) && /[a-z]/.test(pwd)) score += 1;
  if (/\d/.test(pwd)) score += 1;
  if (/[^A-Za-z0-9]/.test(pwd)) score += 1;

  if (pwd.length < 8) return { label: '过短（至少 8 位）', color: '#dc2626', pct: 22 };
  if (score <= 2) return { label: '一般', color: '#d97706', pct: 50 };
  if (score <= 4) return { label: '较强', color: '#3370ff', pct: 78 };
  return { label: '高强度', color: '#0f9d58', pct: 100 };
}

export function ForceChangePasswordPage({
  user,
  onSuccess,
  onLogout,
}: ForceChangePasswordPageProps) {
  const { pushToast } = useToast();
  const [currentPassword, setCurrentPassword] = useState('');
  const [newPassword, setNewPassword] = useState('');
  const [confirmPassword, setConfirmPassword] = useState('');
  const [showPwd, setShowPwd] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const strength = useMemo(() => getPasswordStrength(newPassword), [newPassword]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setErrorMsg(null);

    if (!currentPassword) {
      setErrorMsg('请输入当前密码（即首次启动时生成的初始密码）');
      return;
    }
    if (newPassword.length < 8 || newPassword.length > 128) {
      setErrorMsg('新密码长度必须为 8 至 128 个字符');
      return;
    }
    if (newPassword === currentPassword) {
      setErrorMsg('新密码不能与当前初始密码相同');
      return;
    }
    if (newPassword !== confirmPassword) {
      setErrorMsg('两次输入的新密码不一致');
      return;
    }

    setSubmitting(true);
    try {
      const updated = await apiClient.changePassword(currentPassword, newPassword);
      pushToast('success', '密码已更新', '已签发全权限访问令牌，正在进入管理后台');
      onSuccess(updated);
    } catch (err) {
      if (err instanceof ApiError) {
        setErrorMsg(err.message);
      } else {
        setErrorMsg('修改密码失败，请稍后重试');
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="auth-page">
      <div className="auth-card" style={{ maxWidth: 480 }}>
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            marginBottom: 20,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <BrandMark size={36} />
            <div>
              <div style={{ fontSize: 15, fontWeight: 700 }}>RabbitControl Admin</div>
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)' }}>安全门禁拦截</div>
            </div>
          </div>
          <button type="button" className="btn btn-ghost btn-sm" disabled={submitting} onClick={onLogout}>
            <LogOut size={14} />
            退出登录
          </button>
        </div>

        <div className="alert alert-warning" style={{ marginBottom: 18 }}>
          <ShieldAlert size={18} style={{ flexShrink: 0, marginTop: 1 }} />
          <div>
            <div style={{ fontWeight: 600, marginBottom: 2 }}>首次登录必须修改初始密码</div>
            <div style={{ fontSize: 12.5 }}>
              当前账号 <span className="mono">{user.email}</span> 处于受限令牌状态，修改密码前无法访问仪表盘、用户管理或系统配置。
            </div>
          </div>
        </div>

        {errorMsg && (
          <div className="alert alert-danger" style={{ marginBottom: 16 }}>
            <AlertCircle size={17} style={{ flexShrink: 0, marginTop: 1 }} />
            <span>{errorMsg}</span>
          </div>
        )}

        <form onSubmit={handleSubmit} style={{ display: 'flex', flexDirection: 'column', gap: 15 }}>
          <div className="form-field">
            <label className="form-label" htmlFor="cur-pwd">
              当前密码
            </label>
            <div className="input-with-icon">
              <span className="input-icon-left">
                <KeyRound size={15} />
              </span>
              <input
                id="cur-pwd"
                type={showPwd ? 'text' : 'password'}
                autoComplete="current-password"
                className="input has-left-icon has-right-action"
                placeholder="输入当前使用的密码"
                value={currentPassword}
                onChange={(e) => setCurrentPassword(e.target.value)}
                disabled={submitting}
              />
              <button
                type="button"
                className="input-right-btn"
                onClick={() => setShowPwd((v) => !v)}
                aria-label={showPwd ? '隐藏密码' : '显示密码'}
              >
                {showPwd ? <EyeOff size={15} /> : <Eye size={15} />}
              </button>
            </div>
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="new-pwd">
              新密码（至少 8 位）
            </label>
            <input
              id="new-pwd"
              type={showPwd ? 'text' : 'password'}
              autoComplete="new-password"
              className="input"
              placeholder="设置高强度新密码"
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
              disabled={submitting}
            />
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginTop: 4 }}>
              <div
                style={{
                  flex: 1,
                  height: 5,
                  borderRadius: 99,
                  background: '#e5e7eb',
                  overflow: 'hidden',
                }}
              >
                <div
                  style={{
                    width: `${strength.pct}%`,
                    height: '100%',
                    background: strength.color,
                    transition: 'width 0.2s ease',
                  }}
                />
              </div>
              <span style={{ fontSize: 12, color: strength.color, fontWeight: 500 }}>
                {strength.label}
              </span>
            </div>
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="confirm-pwd">
              确认新密码
            </label>
            <input
              id="confirm-pwd"
              type={showPwd ? 'text' : 'password'}
              autoComplete="new-password"
              className="input"
              placeholder="再次输入新密码"
              value={confirmPassword}
              onChange={(e) => setConfirmPassword(e.target.value)}
              disabled={submitting}
            />
            {confirmPassword && confirmPassword === newPassword && (
              <span
                style={{
                  fontSize: 12,
                  color: 'var(--rc-success)',
                  display: 'inline-flex',
                  alignItems: 'center',
                  gap: 4,
                }}
              >
                <CheckCircle2 size={13} />
                两次输入一致
              </span>
            )}
          </div>

          <button
            type="submit"
            className="btn btn-primary btn-lg"
            disabled={submitting}
            style={{ width: '100%', marginTop: 4 }}
          >
            {submitting ? '正在保存并签发新令牌...' : '确认修改并进入管理后台'}
          </button>
        </form>
      </div>
    </div>
  );
}
