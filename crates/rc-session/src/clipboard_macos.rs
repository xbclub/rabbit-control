//! macOS 剪贴板后端：基于 CarbonCore / HIServices Pasteboard 纯 C API（T20 / M2-8）。
//!
//! 本模块是 `rc-session` **唯一**允许 `unsafe` 的地方：模块头声明
//! `#![allow(unsafe_code)]` 与 `#![deny(clippy::undocumented_unsafe_blocks)]`，
//! 每个 `unsafe` 块及 `unsafe impl` 均附 `// SAFETY:` 说明（对齐
//! `crates/rc-input-macos/src/ffi/mod.rs` 的纪律）。
//!
//! # 为什么走 Pasteboard C API 而非 `NSPasteboard`
//!
//! - 不引入 `objc2` / `objc` 等重型依赖，只复用 workspace 既有的
//!   `core-foundation = "0.10"`；
//! - `com.apple.pasteboard.clipboard` 与 Cocoa `NSPasteboard.generalPasteboard`
//!   指向同一全局系统剪贴板资源，跨进程完全互通；
//! - **绝不向 `PasteboardCreate` 传 `NULL`**（`NULL` 创建的是私有粘贴板，
//!   读写永远不会触达系统剪贴板，见已知坑 3）；
//! - 每次读变更计数或读内容前必须先 `PasteboardSynchronize`（跨进程缓存
//!   一致性，见已知坑 4）。

#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::{c_ulong, c_void};
use std::io;
use std::ptr;

use core_foundation::base::{CFIndex, CFRelease, CFTypeRef, TCFType};
use core_foundation::data::{CFData, CFDataRef};
use core_foundation::string::{CFString, CFStringRef};
use rc_types::{Error, Result};

use crate::clipboard::ClipboardBackend;

/// 全局系统剪贴板名（等价 `Pasteboard.h` 的 `kPasteboardClipboard`）。
const SYSTEM_CLIPBOARD_NAME: &str = "com.apple.pasteboard.clipboard";

/// UTF-8 纯文本 UTI（等价 `kUTTypeUTF8PlainText`）。
const UTF8_PLAIN_TEXT_UTI: &str = "public.utf8-plain-text";

/// `PasteboardSynchronize` 返回标志：全局粘贴板自上次同步后被修改。
const K_PASTEBOARD_MODIFIED: u32 = 1 << 0;

/// `PasteboardPutItemFlavor` 标志：无特殊标志。
const K_PASTEBOARD_FLAVOR_NO_FLAGS: u32 = 0;

/// `Pasteboard.h` 错误码：条目下标不存在。
const BAD_PASTEBOARD_INDEX_ERR: i32 = -25131;
/// `Pasteboard.h` 错误码：条目引用不存在。
const BAD_PASTEBOARD_ITEM_ERR: i32 = -25132;
/// `Pasteboard.h` 错误码：条目不含所请求 flavor。
const BAD_PASTEBOARD_FLAVOR_ERR: i32 = -25133;

/// `PasteboardRef` 不透明指针类型（`struct OpaquePasteboardRef *`）。
type PasteboardRef = *mut c_void;
/// `PasteboardItemID` 条目标识符（`void *`）。
type PasteboardItemID = *mut c_void;
/// `OSStatus`（`MacTypes.h`: `typedef SInt32 OSStatus`）。
type OSStatus = i32;
/// `PasteboardSyncFlags`（`OptionBits` = `UInt32`）。
type PasteboardSyncFlags = u32;
/// `PasteboardFlavorFlags`（`OptionBits` = `UInt32`）。
type PasteboardFlavorFlags = u32;
/// `ItemCount`（`MacTypes.h`: `typedef unsigned long ItemCount`）。
type ItemCount = c_ulong;

mod raw {
    use super::*;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub(super) fn PasteboardCreate(
            in_name: CFStringRef,
            out_pasteboard: *mut PasteboardRef,
        ) -> OSStatus;

        pub(super) fn PasteboardSynchronize(in_pasteboard: PasteboardRef) -> PasteboardSyncFlags;

        pub(super) fn PasteboardClear(in_pasteboard: PasteboardRef) -> OSStatus;

        pub(super) fn PasteboardGetItemCount(
            in_pasteboard: PasteboardRef,
            out_item_count: *mut ItemCount,
        ) -> OSStatus;

        pub(super) fn PasteboardGetItemIdentifier(
            in_pasteboard: PasteboardRef,
            in_index: CFIndex,
            out_item: *mut PasteboardItemID,
        ) -> OSStatus;

        pub(super) fn PasteboardCopyItemFlavorData(
            in_pasteboard: PasteboardRef,
            in_item: PasteboardItemID,
            in_flavor_type: CFStringRef,
            out_data: *mut CFDataRef,
        ) -> OSStatus;

        pub(super) fn PasteboardPutItemFlavor(
            in_pasteboard: PasteboardRef,
            in_item: PasteboardItemID,
            in_flavor_type: CFStringRef,
            in_data: CFDataRef,
            in_flags: PasteboardFlavorFlags,
        ) -> OSStatus;
    }
}

fn osstatus_err(op: &'static str, status: OSStatus) -> Error {
    Error::Io(io::Error::other(format!(
        "macOS Pasteboard {op} 失败 (OSStatus={status})"
    )))
}

/// macOS 系统剪贴板后端（CarbonCore / HIServices Pasteboard 纯 C API）。
///
/// 持有对全局系统粘贴板 `com.apple.pasteboard.clipboard` 的本地引用、缓存的
/// `public.utf8-plain-text` UTI `CFString`，以及由 `PasteboardSynchronize`
/// 的 `kPasteboardModified` 标志驱动的单调变更计数。
pub struct NspasteboardBackend {
    /// 非空、由本结构体独占持有的 `PasteboardRef`，`Drop` 时 `CFRelease`。
    pb: PasteboardRef,
    /// 缓存的 `"public.utf8-plain-text"` `CFString`，避免每次读写重复分配。
    utf8_flavor: CFString,
    /// 本地单调变更计数：每次跨进程同步观测到 `kPasteboardModified` 或本端
    /// `write_text` 成功后递增。
    change_count: u64,
}

// SAFETY: `PasteboardRef` 是 Core Foundation 引用计数对象（`CFRelease` 线程
// 安全），`CFString` 是不可变 Core Foundation 对象。虽然 Pasteboard Manager API
// 本身不支持并发调用，但 `NspasteboardBackend` 仅通过 `&mut self` 串行访问、
// 绝不对外暴露共享引用（不需要 `Sync`），跨线程移交独占所有权是安全的。
unsafe impl Send for NspasteboardBackend {}

impl Drop for NspasteboardBackend {
    fn drop(&mut self) {
        if !self.pb.is_null() {
            // SAFETY: `self.pb` 由 `PasteboardCreate` 以 CF_RETURNS_RETAINED
            // 规则返回，且仅在 `Drop` 中释放一次，此后不再解引用。
            unsafe {
                CFRelease(self.pb as CFTypeRef);
            }
            self.pb = ptr::null_mut();
        }
    }
}

impl NspasteboardBackend {
    /// 构造并取得系统剪贴板（`com.apple.pasteboard.clipboard`）本地引用。
    ///
    /// 失败返回 `Err(Error::Io)`，绝不 panic。
    pub fn new() -> Result<Self> {
        let name = CFString::new(SYSTEM_CLIPBOARD_NAME);
        let mut pb: PasteboardRef = ptr::null_mut();
        // SAFETY: `name` 是有效的非空 `CFStringRef`（绝不传 NULL，见坑 3）；
        // `out_pasteboard` 指向已初始化的局部栈槽。
        let status = unsafe { raw::PasteboardCreate(name.as_concrete_TypeRef(), &mut pb) };
        if status != 0 || pb.is_null() {
            return Err(osstatus_err("PasteboardCreate", status));
        }
        // 首次同步，将本地引用与当前全局剪贴板对齐，使后续 `kPasteboardModified`
        // 只反映构造之后的新变更。
        // SAFETY: `pb` 是刚创建成功的有效非空 `PasteboardRef`。
        let _ = unsafe { raw::PasteboardSynchronize(pb) };

        Ok(Self {
            pb,
            utf8_flavor: CFString::new(UTF8_PLAIN_TEXT_UTI),
            change_count: 0,
        })
    }

    /// 同步本地引用与全局剪贴板资源；若全局资源已变更则递增 `change_count`。
    fn sync(&mut self) {
        // SAFETY: `self.pb` 在构造时保证非空有效，且 `&mut self` 保证串行访问。
        let flags = unsafe { raw::PasteboardSynchronize(self.pb) };
        if (flags & K_PASTEBOARD_MODIFIED) != 0 {
            self.change_count = self.change_count.wrapping_add(1);
        }
    }
}

impl ClipboardBackend for NspasteboardBackend {
    fn change_count(&mut self) -> Result<u64> {
        self.sync();
        Ok(self.change_count)
    }

    fn read_text(&mut self) -> Result<Option<String>> {
        self.sync();

        let mut item_count: ItemCount = 0;
        // SAFETY: `self.pb` 有效，`out_item_count` 指向有效可写栈内存。
        let status = unsafe { raw::PasteboardGetItemCount(self.pb, &mut item_count) };
        if status != 0 {
            return Err(osstatus_err("PasteboardGetItemCount", status));
        }
        if item_count == 0 {
            return Ok(None);
        }

        // Pasteboard Manager 条目索引为 1-based，取第 1 条主剪贴板项。
        let mut item_id: PasteboardItemID = ptr::null_mut();
        // SAFETY: `self.pb` 有效，`in_index = 1` 在 `1..=item_count` 范围内，
        // `out_item` 指向有效栈内存。
        let status = unsafe { raw::PasteboardGetItemIdentifier(self.pb, 1, &mut item_id) };
        if status == BAD_PASTEBOARD_INDEX_ERR || status == BAD_PASTEBOARD_ITEM_ERR {
            return Ok(None);
        }
        if status != 0 {
            return Err(osstatus_err("PasteboardGetItemIdentifier", status));
        }

        let mut data_ref: CFDataRef = ptr::null();
        // SAFETY: `self.pb`、`item_id`、`utf8_flavor` 均有效，`out_data` 指向
        // 有效栈槽。返回 `status == 0` 且非空时调用方拥有一次 retain 计数
        //（CF_RETURNS_RETAINED），下方立即交给 `CFData::wrap_under_create_rule`
        // 进行 RAII 托管，确保任何路径都不泄漏。
        let status = unsafe {
            raw::PasteboardCopyItemFlavorData(
                self.pb,
                item_id,
                self.utf8_flavor.as_concrete_TypeRef(),
                &mut data_ref,
            )
        };
        if status == BAD_PASTEBOARD_FLAVOR_ERR {
            return Ok(None);
        }
        if status != 0 {
            return Err(osstatus_err("PasteboardCopyItemFlavorData", status));
        }
        if data_ref.is_null() {
            return Ok(None);
        }

        // SAFETY: `PasteboardCopyItemFlavorData` 按 Create/Copy 规则返回
        // 拥有所有权的非空 `CFDataRef`，由 `CFData` 在作用域结束时 `CFRelease`。
        let data = unsafe { CFData::wrap_under_create_rule(data_ref) };
        match String::from_utf8(data.bytes().to_vec()) {
            Ok(s) => Ok(Some(s)),
            Err(_) => Ok(None),
        }
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        // SAFETY: `self.pb` 有效，`&mut self` 保证串行访问。
        let status = unsafe { raw::PasteboardClear(self.pb) };
        if status != 0 {
            return Err(osstatus_err("PasteboardClear", status));
        }

        if !text.is_empty() {
            let data = CFData::from_buffer(text.as_bytes());
            // 固定非零 item ID（仅在单次 clear 后的同一应用内区分条目）。
            let item_id = 1usize as PasteboardItemID;
            // SAFETY: `self.pb` 已由上方 `PasteboardClear` 取得所有权；
            // `utf8_flavor` 与 `data` 均为调用期间存活的有效 CF 引用。
            let status = unsafe {
                raw::PasteboardPutItemFlavor(
                    self.pb,
                    item_id,
                    self.utf8_flavor.as_concrete_TypeRef(),
                    data.as_concrete_TypeRef(),
                    K_PASTEBOARD_FLAVOR_NO_FLAGS,
                )
            };
            if status != 0 {
                return Err(osstatus_err("PasteboardPutItemFlavor", status));
            }
        }

        // 将本端引用同步到刚写入的版本，防止下次 `change_count()` 重复观测到
        // 本次写入的 `kPasteboardModified`，同时按契约显式递增本地变更计数。
        // SAFETY: `self.pb` 有效，`&mut self` 保证串行访问。
        let _ = unsafe { raw::PasteboardSynchronize(self.pb) };
        self.change_count = self.change_count.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机剪贴板读写回环（`#[ignore]`：无需 TCC，但会覆盖当前登录会话的真实剪贴板）。
    ///
    /// 验证：
    /// 1. `write_text` 递增 `change_count`，另一个独立 `NspasteboardBackend` 实例
    ///    经 `PasteboardSynchronize` 也能观测到变更并回读一致的 UTF-8 文本（排除坑 3
    ///    私有粘贴板假绿）；
    /// 2. 清空（无文本 flavor）后 `read_text` 返回 `Ok(None)` 而非错误。
    #[test]
    #[ignore = "会读写当前登录会话的真实系统剪贴板，需手动触发"]
    fn real_pasteboard_write_read_roundtrip() {
        let mut writer = NspasteboardBackend::new().expect("创建 writer 剪贴板后端");
        let mut reader = NspasteboardBackend::new().expect("创建 reader 剪贴板后端");

        let w0 = writer.change_count().expect("writer 初始 change_count");
        let r0 = reader.change_count().expect("reader 初始 change_count");

        let payload = "RabbitControl 剪贴板测试 🐰 line1\nline2";
        writer.write_text(payload).expect("write_text");

        let w1 = writer.change_count().expect("writer 写后 change_count");
        assert!(w1 > w0, "写入后本端 change_count 必须递增: {w0} -> {w1}");

        let r1 = reader.change_count().expect("reader 同步 change_count");
        assert!(
            r1 > r0,
            "独立实例必须通过全局剪贴板观测到变更（排除私有粘贴板坑 3）: {r0} -> {r1}"
        );

        let got = reader.read_text().expect("read_text");
        assert_eq!(got.as_deref(), Some(payload));

        // 写入空串（clear-only）→ 无文本 flavor 状态 → read_text 返回 Ok(None)
        writer.write_text("").expect("clear pasteboard");
        let cleared = reader.read_text().expect("read_text after clear");
        assert_eq!(cleared, None);
    }
}
