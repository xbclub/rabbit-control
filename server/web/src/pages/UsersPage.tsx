import React, { useCallback, useEffect, useState } from 'react';
import {
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  Edit3,
  Plus,
  RefreshCw,
  Search,
  ShieldAlert,
  Trash2,
  UserCheck,
  UserX,
  Wand2,
} from 'lucide-react';
import { ApiError, apiClient } from '../api/client';
import { Modal } from '../components/Modal';
import { useToast } from '../components/Toast';
import type { AdminUser, AuthUser, UserRole, UserStatus } from '../types/api';
import { formatDateTime, formatRelativeTime, generateStrongPassword, isValidEmail } from '../utils/format';

interface UsersPageProps {
  currentUser: AuthUser;
}

const PAGE_SIZE = 10;

export function UsersPage({ currentUser }: UsersPageProps) {
  const { pushToast } = useToast();
  const [users, setUsers] = useState<AdminUser[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [loading, setLoading] = useState(true);

  // Filters
  const [searchInput, setSearchInput] = useState('');
  const [queryText, setQueryText] = useState('');
  const [roleFilter, setRoleFilter] = useState('all');
  const [statusFilter, setStatusFilter] = useState('all');

  // Create Modal
  const [createOpen, setCreateOpen] = useState(false);
  const [createEmail, setCreateEmail] = useState('');
  const [createName, setCreateName] = useState('');
  const [createPassword, setCreatePassword] = useState('');
  const [createConfirm, setCreateConfirm] = useState('');
  const [createRole, setCreateRole] = useState<UserRole>('user');
  const [createMustChange, setCreateMustChange] = useState(true);
  const [createError, setCreateError] = useState<string | null>(null);
  const [createSubmitting, setCreateSubmitting] = useState(false);

  // Edit Modal
  const [editTarget, setEditTarget] = useState<AdminUser | null>(null);
  const [editEmail, setEditEmail] = useState('');
  const [editName, setEditName] = useState('');
  const [editRole, setEditRole] = useState<UserRole>('user');
  const [editStatus, setEditStatus] = useState<UserStatus>('active');
  const [editResetPwd, setEditResetPwd] = useState('');
  const [editResetConfirm, setEditResetConfirm] = useState('');
  const [editMustChange, setEditMustChange] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  const [editSubmitting, setEditSubmitting] = useState(false);

  // Delete Modal
  const [deleteTarget, setDeleteTarget] = useState<AdminUser | null>(null);
  const [deleteSubmitting, setDeleteSubmitting] = useState(false);

  const loadUsers = useCallback(async () => {
    setLoading(true);
    try {
      const res = await apiClient.listUsers({
        q: queryText || undefined,
        role: roleFilter,
        status: statusFilter,
        limit: PAGE_SIZE,
        offset,
      });
      setUsers(res.users);
      setTotal(res.total);
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '获取用户列表失败';
      pushToast('error', '加载用户失败', msg);
    } finally {
      setLoading(false);
    }
  }, [queryText, roleFilter, statusFilter, offset, pushToast]);

  useEffect(() => {
    void loadUsers();
  }, [loadUsers]);

  const handleSearchSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    setOffset(0);
    setQueryText(searchInput.trim());
  };

  const openCreateModal = () => {
    setCreateEmail('');
    setCreateName('');
    setCreatePassword('');
    setCreateConfirm('');
    setCreateRole('user');
    setCreateMustChange(true);
    setCreateError(null);
    setCreateOpen(true);
  };

  const handleGeneratePwd = () => {
    const generated = generateStrongPassword(14);
    setCreatePassword(generated);
    setCreateConfirm(generated);
  };

  const handleCreateSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setCreateError(null);

    if (!isValidEmail(createEmail)) {
      setCreateError('请输入有效的邮箱地址');
      return;
    }
    if (createPassword.length < 8 || createPassword.length > 128) {
      setCreateError('初始密码长度需在 8 到 128 位之间');
      return;
    }
    if (createPassword !== createConfirm) {
      setCreateError('两次输入的密码不一致');
      return;
    }

    setCreateSubmitting(true);
    try {
      const created = await apiClient.createUser({
        email: createEmail.trim(),
        display_name: createName.trim(),
        password: createPassword,
        role: createRole,
        must_change_password: createMustChange,
      });
      pushToast('success', '用户创建成功', `已创建账号 ${created.email}`);
      setCreateOpen(false);
      setOffset(0);
      await loadUsers();
    } catch (err) {
      setCreateError(err instanceof ApiError ? err.message : '创建用户失败');
    } finally {
      setCreateSubmitting(false);
    }
  };

  const openEditModal = (u: AdminUser) => {
    setEditTarget(u);
    setEditEmail(u.email);
    setEditName(u.display_name);
    setEditRole(u.role);
    setEditStatus(u.status);
    setEditResetPwd('');
    setEditResetConfirm('');
    setEditMustChange(u.must_change_password);
    setEditError(null);
  };

  const handleEditSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!editTarget) return;
    setEditError(null);

    if (!isValidEmail(editEmail)) {
      setEditError('请输入有效的邮箱地址');
      return;
    }
    if (editResetPwd) {
      if (editResetPwd.length < 8 || editResetPwd.length > 128) {
        setEditError('重置密码长度需在 8 到 128 位之间');
        return;
      }
      if (editResetPwd !== editResetConfirm) {
        setEditError('两次输入的重置密码不一致');
        return;
      }
    }

    setEditSubmitting(true);
    try {
      await apiClient.updateUser(editTarget.id, {
        email: editEmail.trim(),
        display_name: editName.trim(),
        role: editRole,
        status: editStatus,
        reset_password: editResetPwd || undefined,
        must_change_password: editMustChange,
      });
      pushToast(
        'success',
        '用户信息已保存',
        editResetPwd || editStatus === 'disabled' || editRole !== editTarget.role
          ? '已同步吊销旧凭证并强制下线相关设备'
          : undefined,
      );
      setEditTarget(null);
      await loadUsers();
    } catch (err) {
      setEditError(err instanceof ApiError ? err.message : '保存修改失败');
    } finally {
      setEditSubmitting(false);
    }
  };

  const handleQuickToggleStatus = async (u: AdminUser) => {
    const nextStatus: UserStatus = u.status === 'active' ? 'disabled' : 'active';
    try {
      await apiClient.updateUser(u.id, { status: nextStatus });
      pushToast(
        'success',
        nextStatus === 'disabled' ? '已禁用账号' : '已重新启用账号',
        nextStatus === 'disabled'
          ? `${u.email} 已被禁用，在线会话与设备证书已吊销踢线`
          : `${u.email} 已恢复正常登录权限`,
      );
      await loadUsers();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '更新状态失败';
      pushToast('error', '操作失败', msg);
    }
  };

  const handleDeleteConfirm = async () => {
    if (!deleteTarget) return;
    setDeleteSubmitting(true);
    try {
      await apiClient.deleteUser(deleteTarget.id);
      pushToast(
        'success',
        '用户已删除',
        `${deleteTarget.email} 的证书、会话与设备绑定已全部级联清理`,
      );
      setDeleteTarget(null);
      if (users.length === 1 && offset > 0) {
        setOffset(Math.max(0, offset - PAGE_SIZE));
      } else {
        await loadUsers();
      }
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : '删除用户失败';
      pushToast('error', '删除失败', msg);
    } finally {
      setDeleteSubmitting(false);
    }
  };

  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const currentPage = Math.floor(offset / PAGE_SIZE) + 1;

  return (
    <>
      <div className="card">
        <div className="card-header" style={{ flexWrap: 'wrap' }}>
          <div>
            <h3 className="card-title">用户账号与角色管理</h3>
            <p className="card-subtitle">
              共 <span className="tabular" style={{ fontWeight: 600 }}>{total}</span> 个账号；禁用、重置密码或删除用户时将实时吊销凭证与 mTLS 证书
            </p>
          </div>
          <div style={{ display: 'flex', gap: 10 }}>
            <button type="button" className="btn btn-secondary btn-sm" onClick={() => void loadUsers()}>
              <RefreshCw size={14} />
              刷新
            </button>
            <button type="button" className="btn btn-primary" onClick={openCreateModal}>
              <Plus size={16} />
              新建用户
            </button>
          </div>
        </div>

        <div
          style={{
            padding: '14px 20px',
            borderBottom: '1px solid var(--rc-border)',
            background: 'var(--rc-surface-subtle)',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            flexWrap: 'wrap',
            gap: 12,
          }}
        >
          <form onSubmit={handleSearchSubmit} style={{ display: 'flex', gap: 8, flexWrap: 'wrap', flex: 1 }}>
            <div className="input-with-icon" style={{ minWidth: 240, maxWidth: 340, flex: 1 }}>
              <span className="input-icon-left">
                <Search size={15} />
              </span>
              <input
                type="text"
                className="input input-sm has-left-icon"
                placeholder="搜索邮箱或昵称..."
                value={searchInput}
                onChange={(e) => setSearchInput(e.target.value)}
              />
            </div>

            <select
              className="select select-sm"
              style={{ width: 130 }}
              value={roleFilter}
              onChange={(e) => {
                setRoleFilter(e.target.value);
                setOffset(0);
              }}
              aria-label="按角色筛选"
            >
              <option value="all">全部角色</option>
              <option value="admin">管理员 (admin)</option>
              <option value="user">普通用户 (user)</option>
            </select>

            <select
              className="select select-sm"
              style={{ width: 130 }}
              value={statusFilter}
              onChange={(e) => {
                setStatusFilter(e.target.value);
                setOffset(0);
              }}
              aria-label="按状态筛选"
            >
              <option value="all">全部状态</option>
              <option value="active">正常 (active)</option>
              <option value="disabled">已禁用 (disabled)</option>
            </select>

            <button type="submit" className="btn btn-secondary btn-sm">
              筛选
            </button>
          </form>
        </div>

        <div className="table-wrap">
          <table className="table">
            <thead>
              <tr>
                <th>用户</th>
                <th>角色</th>
                <th>状态</th>
                <th>绑定设备</th>
                <th>最近登录</th>
                <th>创建时间</th>
                <th style={{ textAlign: 'right' }}>操作</th>
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <tr>
                  <td colSpan={7} style={{ textAlign: 'center', padding: 36, color: 'var(--rc-text-muted)' }}>
                    正在加载用户列表...
                  </td>
                </tr>
              ) : users.length === 0 ? (
                <tr>
                  <td colSpan={7} style={{ textAlign: 'center', padding: 36, color: 'var(--rc-text-muted)' }}>
                    没有匹配的用户记录
                  </td>
                </tr>
              ) : (
                users.map((u) => {
                  const isSelf = u.id === currentUser.id;
                  return (
                    <tr key={u.id}>
                      <td>
                        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                          <div
                            style={{
                              width: 32,
                              height: 32,
                              borderRadius: '50%',
                              background: u.role === 'admin' ? 'var(--rc-primary-soft)' : '#f1f5f9',
                              color: u.role === 'admin' ? 'var(--rc-primary)' : '#475569',
                              display: 'flex',
                              alignItems: 'center',
                              justifyContent: 'center',
                              fontWeight: 700,
                              fontSize: 12.5,
                              flexShrink: 0,
                            }}
                          >
                            {(u.display_name || u.email).slice(0, 1).toUpperCase()}
                          </div>
                          <div>
                            <div style={{ fontWeight: 600, display: 'flex', alignItems: 'center', gap: 6 }}>
                              <span>{u.display_name || '未设置昵称'}</span>
                              {isSelf && <span className="badge badge-primary">当前账号</span>}
                            </div>
                            <div className="mono" style={{ fontSize: 12, color: 'var(--rc-text-muted)' }}>
                              {u.email}
                            </div>
                          </div>
                        </div>
                      </td>
                      <td>
                        {u.role === 'admin' ? (
                          <span className="badge badge-primary">管理员</span>
                        ) : (
                          <span className="badge badge-neutral">普通用户</span>
                        )}
                      </td>
                      <td>
                        <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
                          {u.status === 'active' ? (
                            <span className="badge badge-success">
                              <span className="badge-dot" />
                              正常
                            </span>
                          ) : (
                            <span className="badge badge-danger">已禁用</span>
                          )}
                          {u.must_change_password && (
                            <span className="badge badge-warning">待改密</span>
                          )}
                        </div>
                      </td>
                      <td className="tabular" style={{ fontWeight: 500 }}>
                        {u.device_count} 台
                      </td>
                      <td style={{ fontSize: 12.5, color: 'var(--rc-text-secondary)' }}>
                        {u.last_login_at ? formatRelativeTime(u.last_login_at) : '从未登录'}
                      </td>
                      <td style={{ fontSize: 12.5, color: 'var(--rc-text-muted)' }}>
                        {formatDateTime(u.created_at).slice(0, 16)}
                      </td>
                      <td>
                        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 6 }}>
                          <button
                            type="button"
                            className="btn btn-secondary btn-sm"
                            onClick={() => openEditModal(u)}
                          >
                            <Edit3 size={13} />
                            编辑
                          </button>
                          <button
                            type="button"
                            className="btn btn-secondary btn-sm"
                            disabled={isSelf}
                            title={isSelf ? '不可禁用当前登录账号' : undefined}
                            onClick={() => void handleQuickToggleStatus(u)}
                          >
                            {u.status === 'active' ? (
                              <>
                                <UserX size={13} />
                                禁用
                              </>
                            ) : (
                              <>
                                <UserCheck size={13} />
                                启用
                              </>
                            )}
                          </button>
                          <button
                            type="button"
                            className="btn btn-danger-outline btn-sm"
                            disabled={isSelf}
                            title={isSelf ? '不可删除当前登录账号' : undefined}
                            onClick={() => setDeleteTarget(u)}
                          >
                            <Trash2 size={13} />
                            删除
                          </button>
                        </div>
                      </td>
                    </tr>
                  );
                })
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
          <span>
            第 {currentPage} / {totalPages} 页 · 共 {total} 条
          </span>
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
              disabled={offset + PAGE_SIZE >= total}
              onClick={() => setOffset(offset + PAGE_SIZE)}
            >
              下一页
              <ChevronRight size={14} />
            </button>
          </div>
        </div>
      </div>

      {/* Create User Modal */}
      <Modal
        open={createOpen}
        title="新建用户账号"
        onClose={() => !createSubmitting && setCreateOpen(false)}
        footer={
          <>
            <button
              type="button"
              className="btn btn-secondary"
              onClick={() => setCreateOpen(false)}
              disabled={createSubmitting}
            >
              取消
            </button>
            <button
              type="submit"
              form="create-user-form"
              className="btn btn-primary"
              disabled={createSubmitting}
            >
              {createSubmitting ? '正在创建...' : '确认创建'}
            </button>
          </>
        }
      >
        <form id="create-user-form" onSubmit={handleCreateSubmit} style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
          {createError && <div className="alert alert-danger">{createError}</div>}

          <div className="form-field">
            <label className="form-label" htmlFor="cu-email">
              登录邮箱 *
            </label>
            <input
              id="cu-email"
              type="email"
              className="input"
              placeholder="name@company.com"
              value={createEmail}
              onChange={(e) => setCreateEmail(e.target.value)}
            />
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="cu-name">
              显示名称
            </label>
            <input
              id="cu-name"
              type="text"
              className="input"
              placeholder="例如：张三 (运维组)"
              value={createName}
              onChange={(e) => setCreateName(e.target.value)}
            />
          </div>

          <div className="form-field">
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <label className="form-label" htmlFor="cu-pwd">
                初始密码（≥ 8 位）*
              </label>
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                style={{ height: 24, fontSize: 12, color: 'var(--rc-primary)' }}
                onClick={handleGeneratePwd}
              >
                <Wand2 size={12} />
                生成随机强密码
              </button>
            </div>
            <input
              id="cu-pwd"
              type="text"
              className="input mono"
              placeholder="至少 8 个字符"
              value={createPassword}
              onChange={(e) => setCreatePassword(e.target.value)}
            />
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="cu-confirm">
              确认初始密码 *
            </label>
            <input
              id="cu-confirm"
              type="password"
              className="input"
              placeholder="再次输入密码"
              value={createConfirm}
              onChange={(e) => setCreateConfirm(e.target.value)}
            />
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="cu-role">
              账号角色
            </label>
            <select
              id="cu-role"
              className="select"
              value={createRole}
              onChange={(e) => setCreateRole(e.target.value as UserRole)}
            >
              <option value="user">普通用户 (user)</option>
              <option value="admin">系统管理员 (admin)</option>
            </select>
          </div>

          <label
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              padding: '10px 12px',
              borderRadius: 8,
              border: '1px solid var(--rc-border)',
              background: 'var(--rc-surface-subtle)',
              cursor: 'pointer',
            }}
          >
            <div>
              <div style={{ fontWeight: 500, fontSize: 13 }}>首次登录强制修改密码</div>
              <div style={{ fontSize: 12, color: 'var(--rc-text-muted)' }}>
                开启后，该用户首次登录必须改密才能使用设备与控制功能
              </div>
            </div>
            <input
              type="checkbox"
              checked={createMustChange}
              onChange={(e) => setCreateMustChange(e.target.checked)}
              style={{ width: 17, height: 17, accentColor: 'var(--rc-primary)' }}
            />
          </label>
        </form>
      </Modal>

      {/* Edit User Modal */}
      <Modal
        open={Boolean(editTarget)}
        title="编辑用户信息与权限"
        onClose={() => !editSubmitting && setEditTarget(null)}
        footer={
          <>
            <button
              type="button"
              className="btn btn-secondary"
              onClick={() => setEditTarget(null)}
              disabled={editSubmitting}
            >
              取消
            </button>
            <button
              type="submit"
              form="edit-user-form"
              className="btn btn-primary"
              disabled={editSubmitting}
            >
              {editSubmitting ? '保存中...' : '保存更改'}
            </button>
          </>
        }
      >
        <form id="edit-user-form" onSubmit={handleEditSubmit} style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
          <div className="alert alert-info">
            <ShieldAlert size={17} style={{ flexShrink: 0, marginTop: 1 }} />
            <span>
              安全提示：修改角色、禁用账号或重置密码保存后，系统将自动递增令牌版本、吊销该用户现有会话与设备 mTLS 证书，并立即踢下线相关设备。
            </span>
          </div>

          {editError && <div className="alert alert-danger">{editError}</div>}

          <div className="form-field">
            <label className="form-label" htmlFor="eu-email">
              登录邮箱
            </label>
            <input
              id="eu-email"
              type="email"
              className="input"
              value={editEmail}
              onChange={(e) => setEditEmail(e.target.value)}
            />
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="eu-name">
              显示名称
            </label>
            <input
              id="eu-name"
              type="text"
              className="input"
              value={editName}
              onChange={(e) => setEditName(e.target.value)}
            />
          </div>

          <div className="grid-2" style={{ gap: 12 }}>
            <div className="form-field">
              <label className="form-label" htmlFor="eu-role">
                账号角色
              </label>
              <select
                id="eu-role"
                className="select"
                value={editRole}
                disabled={Boolean(editTarget && currentUser.id === editTarget.id)}
                title={
                  editTarget && currentUser.id === editTarget.id
                    ? '不可降级当前登录管理员角色'
                    : undefined
                }
                onChange={(e) => setEditRole(e.target.value as UserRole)}
              >
                <option value="user">普通用户 (user)</option>
                <option value="admin">系统管理员 (admin)</option>
              </select>
            </div>

            <div className="form-field">
              <label className="form-label" htmlFor="eu-status">
                账号状态
              </label>
              <select
                id="eu-status"
                className="select"
                value={editStatus}
                disabled={Boolean(editTarget && currentUser.id === editTarget.id)}
                title={
                  editTarget && currentUser.id === editTarget.id
                    ? '不可禁用当前登录账号'
                    : undefined
                }
                onChange={(e) => setEditStatus(e.target.value as UserStatus)}
              >
                <option value="active">正常启用 (active)</option>
                <option value="disabled">立即禁用 (disabled)</option>
              </select>
            </div>
          </div>

          <div className="form-field">
            <label className="form-label" htmlFor="eu-pwd">
              重置密码（留空则不修改）
            </label>
            <input
              id="eu-pwd"
              type="password"
              className="input"
              placeholder="如需重置，请输入 ≥ 8 位新密码"
              value={editResetPwd}
              onChange={(e) => setEditResetPwd(e.target.value)}
            />
          </div>

          {editResetPwd && (
            <div className="form-field">
              <label className="form-label" htmlFor="eu-pwd-confirm">
                确认重置密码
              </label>
              <input
                id="eu-pwd-confirm"
                type="password"
                className="input"
                placeholder="再次输入新密码"
                value={editResetConfirm}
                onChange={(e) => setEditResetConfirm(e.target.value)}
              />
            </div>
          )}

          <label
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              padding: '10px 12px',
              borderRadius: 8,
              border: '1px solid var(--rc-border)',
              background: 'var(--rc-surface-subtle)',
              cursor: 'pointer',
            }}
          >
            <span style={{ fontWeight: 500, fontSize: 13 }}>要求下次登录强制改密</span>
            <input
              type="checkbox"
              checked={editMustChange}
              onChange={(e) => setEditMustChange(e.target.checked)}
              style={{ width: 17, height: 17, accentColor: 'var(--rc-primary)' }}
            />
          </label>
        </form>
      </Modal>

      {/* Delete Confirmation Modal */}
      <Modal
        open={Boolean(deleteTarget)}
        title="确认删除用户"
        maxWidth={460}
        onClose={() => !deleteSubmitting && setDeleteTarget(null)}
        footer={
          <>
            <button
              type="button"
              className="btn btn-secondary"
              onClick={() => setDeleteTarget(null)}
              disabled={deleteSubmitting}
            >
              取消
            </button>
            <button
              type="button"
              className="btn btn-danger"
              onClick={() => void handleDeleteConfirm()}
              disabled={deleteSubmitting}
            >
              {deleteSubmitting ? '正在删除...' : '确认删除并踢线'}
            </button>
          </>
        }
      >
        {deleteTarget && (
          <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
            <div className="alert alert-danger">
              <AlertTriangle size={18} style={{ flexShrink: 0, marginTop: 1 }} />
              <div>
                <div style={{ fontWeight: 600, marginBottom: 4 }}>高危不可逆操作提示</div>
                <div>
                  删除用户 <span className="mono">{deleteTarget.email}</span> 将立即执行以下级联清理：
                </div>
                <ul style={{ margin: '6px 0 0 18px', padding: 0 }}>
                  <li>吊销该用户全部登录 Refresh/Access 会话</li>
                  <li>吊销名下所有设备 mTLS 证书并写入吊销缓存</li>
                  <li>解绑名下 {deleteTarget.device_count} 台设备归属并强制踢下线</li>
                </ul>
              </div>
            </div>
          </div>
        )}
      </Modal>
    </>
  );
}
