//! 轻量变更通知（S4-1a，ADR 0005 §1）。
//!
//! ## 只做"有事发生"，不做"发了什么"
//!
//! 事件只带 ID（哪篇笔记被物化 / 结构被投影），**不带正文**：正文可能很大，而且
//! 订阅者本来就要回 storage 再读一次（读的是已提交快照）。这样通知的代价是常数级，
//! 不会因为事件里塞内容而拖慢写路径。
//!
//! ## 绝不阻塞写
//!
//! 发布走 `try_send`：队列满或者根本没人订阅时，**立刻放弃投递**，改置一个
//! [`Vault::take_reconcile_required`] 能看到的重对账标记。检索侧看到这个标记就做一次
//! 全量对账——这比在写路径上等消费者慢半拍要安全得多（ADR 0005 §1 的取舍）。
//!
//! 注意：标记是**粘性**的，只有 `take_reconcile_required()` 会清它；连续丢事件不会丢信息
//! （反正都要全量对账）。

use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use crate::actor::Shared;

/// 存储层发出的轻量变更事件（只带 ID）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageEvent {
    /// 某篇笔记的正文被物化（`notes.body_md/body_seq` 已推进）。
    BodyMaterialized {
        /// 笔记 ID。
        note_id: String,
    },
    /// 结构被投影（`tree` / `notes` 行已更新）。
    ///
    /// `note_id` 为 `None` 表示"整棵树可能有变化"（结构提交是按整棵树投影的，
    /// 精确到单篇的成本高且没必要）。
    StructureProjected {
        /// 涉及的笔记 ID（`None` = 整棵树）。
        note_id: Option<String>,
    },
}

/// 一个订阅者句柄；`Drop` 时自动退订。
#[derive(Debug)]
pub struct StorageEventSubscriber {
    /// 接收端（容量由订阅时指定）。
    rx: Receiver<StorageEvent>,
    /// 退订用的共享状态 + 自己的槽位 ID。
    shared: Arc<Shared>,
    /// 槽位 ID（`subscribe` 时分配）。
    slot: u64,
}

impl StorageEventSubscriber {
    /// 非阻塞取一条事件（`None` = 当前没有事件或订阅端已关闭）。
    #[must_use]
    pub fn try_recv(&self) -> Option<StorageEvent> {
        self.rx.try_recv().ok()
    }

    /// 阻塞等待一条事件（带超时）。
    ///
    /// 返回值：`Ok(event)` 收到；`Err(Timeout)` 超时；`Err(Disconnected)` 发布端已关闭。
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<StorageEvent, std::sync::mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

impl Drop for StorageEventSubscriber {
    fn drop(&mut self) {
        if let Ok(mut slots) = self.shared.event_slots.lock() {
            slots.retain(|(id, _)| *id != self.slot);
        }
    }
}

/// 事件槽位：(订阅者 ID, 发送端)。
pub(crate) type EventSlot = (u64, SyncSender<StorageEvent>);

impl Shared {
    /// 注册一个订阅者，返回句柄（容量 = 队列上限，满了就置重对账标记）。
    pub(crate) fn subscribe_events(self: &Arc<Self>, capacity: usize) -> StorageEventSubscriber {
        let (tx, rx) = sync_channel(capacity.max(1));
        let slot = self.next_event_slot.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut slots) = self.event_slots.lock() {
            slots.push((slot, tx));
        }
        StorageEventSubscriber {
            rx,
            shared: Arc::clone(self),
            slot,
        }
    }

    /// 发布一条事件：`try_send` 到所有订阅者；**一个都没送到**就置重对账标记。
    ///
    /// 返回值：成功投递的订阅者数量（测试用）。
    pub(crate) fn publish_event(&self, event: StorageEvent) -> usize {
        let mut delivered = 0usize;
        if let Ok(slots) = self.event_slots.lock() {
            for (_, tx) in slots.iter() {
                if tx.try_send(event.clone()).is_ok() {
                    delivered += 1;
                }
            }
        }
        if delivered == 0 {
            self.reconcile_required.store(true, Ordering::SeqCst);
        }
        delivered
    }

    /// 取走并清除"需要全量对账"标记。
    pub(crate) fn take_reconcile_required(&self) -> bool {
        self.reconcile_required.swap(false, Ordering::SeqCst)
    }
}

/// 事件队列的默认容量（够一次提交里多篇笔记各发一条）。
pub const DEFAULT_EVENT_QUEUE: usize = 64;

/// 内部使用：把槽位列表包成互斥体。
pub(crate) fn new_slots() -> Mutex<Vec<EventSlot>> {
    Mutex::new(Vec::new())
}
