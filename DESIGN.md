# 四层记忆系统设计文档

## 1. 设计原则

**写入路径：单线贯穿，每层职责单一**
```
写入: L0(write) → L1(write) → L2(write) → L3(write)
召回: L0(hit?) → L1(hit?) → L2(hit?) → L3(default)
```

**核心原则：**
- 每层的组件自己管理自己的状态，不存在跨层隐式依赖
- 任何异步操作必须在状态变更之前完成，不存在"飞行中"数据
- 崩溃恢复路径必须和正常路径用同一套代码，不存在"忘了接入"的空函数
- 每个组件必须有 owner（谁初始化、谁清理），没有游离的独立模块

---

## 2. 四层职责

### L0: Working Memory（进程内存）
- **介质**: DashMap in-process
- **持久化**: 无（进程重启即清空）
- **作用**: 当前 agent session 的工作缓存，延迟 < 1µs
- **淘汰**: LRU，容量上限 10_000 条，或 session 结束清空
- **wake**: 不存在

### L1: Short-Term Memory（短期记忆）
- **介质**: SQLite（WAL 模式）+ 内存索引
- **持久化**: 每次写入立即 fsync（通过 SQLite ACID）
- **作用**: 跨 agent 的近期记忆，TTL = 7 天（可配置）
- **淘汰**: 定时任务，按 `last_accessed + importance` 综合评分
- **崩溃恢复**: SQLite WAL 机制，不需要外挂 WAL
- **关键**: L1 是**唯一**写入磁盘的层，L2/L3 从 L1 异步消费

### L2: Semantic Memory（语义记忆）
- **介质**: L1 快照 + 内存向量索引
- **持久化**: 向量索引 dump 到磁盘（JSON），L1 是 source of truth
- **作用**: 基于语义相似度的召回（BM25 + Embedding 混合）
- **淘汰**: 向量过期时间 30 天，过期后从内存移除但不删 L1 原始数据
- **崩溃恢复**: 从 L1 快照重建，不需要 replay

### L3: Archive Memory（归档记忆）
- **介质**: 文件系统（.md 每日归档）
- **持久化**: L1 超过 30 天的记忆自动归档为 .md 文件
- **作用**: 极低成本长期存储，支持人工查阅
- **召回**: 不参与自动召回，仅在 L2 完全 miss 时提示用户"可在归档中查找"
- **崩溃恢复**: 启动时扫描 .md 文件重新导入 L1

---

## 3. 数据流设计

### 写入流
```
Agent 调用 write()
  → L0: 写入内存 DashMap（同步）
  → L1: 写入 SQLite + fsync（同步，等待确认）
  → L1 gc loop: 定时检查，超过 30 天的 → 写入 L3 .md → 删除 L1 记录
  → L2 background: 定时从 L1 增量同步，生成/更新向量
```

**关键保证：L1 写入完成之前，L2/L3 不可能持有这条数据**
- 优点：崩溃后没有任何"飞行中数据"
- 缺点：L2/L3 召回有延迟（L2 最多滞后 5 分钟，L3 最多滞后 24 小时）
- 这是**明确的设计决策，不是缺陷**

### 召回流
```
Agent 调用 recall()
  → L0: 精确 key 查找（O(1)）
  → L1: 精确 key 或前缀匹配（TTL 内数据）
  → L2: 语义/BM25 混合召回（L2 有，向量存在）
  → L3: 不参与自动召回（返回"已在归档"提示）
```

---

## 4. 组件清单（谁拥有什么）

```
MemorySystem (root owner)
  ├── L0: self (self-owned, no cleanup needed)
  ├── L1: self (owns SQLite connection, WAL thread)
  ├── L2: self (owns vector index, background sync task)
  └── L3: self (owns file scanner, background archive task)

每个组件：
  - 在 MemorySystem::new() 中初始化
  - 有明确的 Drop 实现或 cleanup() 方法
  - 没有"外部必须调用才能生效"的隐式依赖
```

---

## 5. L1: SQLite 替代 sled 的理由

sled 的问题（当前代码已经暴露）：
- WAL 需要外挂实现，replay 容易漏接
- mmap 自动 flush 时机不明确，难以精确控制
- 批量写入窗口（50ms）内 crash 会丢数据

SQLite WAL 模式的优势：
- fsync 精确控制：写入返回即确认落盘
- 内置 WAL replay，无需外挂
- 超过 189 billion 设备在使用，崩溃恢复逻辑经过充分验证

**代价**: 需要系统安装 SQLite（大多数 Linux 默认有），不是纯 Rust。

---

## 6. L2 向量索引设计

```rust
struct SemanticIndex {
    // 全部存在内存，disk 作为 source of truth
    vectors: HashMap<Key, Vec<f32>>,     // key → 向量
    store: HashMap<Key, Entry>,          // key → 原始数据（用于展示）
    bm25_index: Bm25Index,               // BM25 倒排索引
    
    // 向量从 L1 异步构建，不阻塞写入
    pending_keys: Arc<Mutex<HashSet<Key>>>,  // 待处理的 key 队列
    sync_task: JoinHandle<()>,
}
```

**同步策略**：
- 写入 L1 时，key 加入 `pending_keys`
- 后台任务每 60 秒批量处理 `pending_keys`，生成向量
- 如果 L1 的 entry 被删除，`pending_keys` 中对应 key 也移除，不生成无效向量
- 启动时：从 L1 全量重建 `store` + `bm25_index`（不重建向量，直接用已有向量）

---

## 7. 关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| L1 持久化介质 | SQLite | 内置 WAL，无外挂依赖 |
| L2 向量存储 | 纯内存 | L1 是 source of truth，L2 可完全重建 |
| L2 同步方式 | 拉模式（pull） | L1 是 source，L2 主动读取，不在写入路径 |
| L3 淘汰触发 | 定时任务（每日） | 避免和写入路径竞争 I/O |
| L2 向量生成 | 后台批量，非同步 | 不拖慢写入延迟 |
| L1 写入确认 | fsync 后返回 | 写入返回即保证不丢 |
| Crash recovery | 同一路径，不需 replay | SQLite WAL + L2 从 L1 重建 |

---

## 8. 测试方案

### 单元测试（每个组件独立）

**L0 测试** (`l0/test.rs`)
- 并发写入 100 个 key，读回验证一致
- LRU 容量超限后验证淘汰行为
- session clear 验证所有 key 删除

**L1 测试** (`l1/test.rs`)
- 写入 1000 条，进程崩溃（SIGKILL 模拟），重启后验证全部存在
- TTL 过期后验证数据消失
- 并发写入不丢数据

**L2 测试** (`l2/test.rs`)
- 写入 L1 后，验证 L2 定时同步
- BM25 召回结果相关性验证
- 向量维度一致性验证

**L3 测试** (`l3/test.rs`)
- 归档任务运行后验证 .md 文件格式正确
- 启动时重新导入验证数据完整性

### 集成测试（`tests/integration.rs`）

**test_write_and_recall**
```
write("key1", "hello world")
→ L0 hit, L1 hit, L2 sync, L3 not yet
recall("hello") → 找到 key1
```

**test_crash_recovery**
```
写入 100 条
SIGKILL 模拟崩溃
重启
→ L1 有 100 条
→ L2 重建后有 100 条
```

**test_concurrent_writes**
```
100 并发写入不同 key
100 并发召回
→ 无 panic，无数据丢失
→ 召回结果数 = 100
```

**test_l3_archival**
```
写入 L1
等待归档任务触发（或手动调用）
→ L1 中对应记录删除
→ .md 文件存在且内容正确
```

**test_no_flying_data**
```
写入后立即崩溃（模拟写入完成但未 flush 的边界）
→ L1 有该条记录（SQLite ACID 保证）
```

**test_l2_semantic_recall**
```
写入: "我喜欢吃苹果和香蕉" → key=fruit_1
写入: "今天天气很好" → key=weather_1
recall("水果") → 必须找到 fruit_1，不找 weather_1
```

**test_l2_rebuild_from_l1**
```
L2 持有 500 条数据
清空 L2 向量索引
触发重建
→ L2 store 有 500 条
→ L2 向量索引重建（非 L1 重复写入）
→ BM25 可召回
```

### 混沌测试（`tests/chaos.rs`）

**test_sudden_kill**
用 `std::process::Command` fork 子进程写入后立即 `kill -9`，父进程验证数据完整性。

**test_disk_full**
mock 磁盘满场景，验证 L1 降级处理（拒绝写入，返回错误，不崩溃）。

---

## 9. 项目结构

```
memory-system-v2/
  ├── Cargo.toml
  ├── src/
  │   ├── lib.rs              # MemorySystem 入口
  │   ├── l0/                 # Working Memory
  │   │   └── mod.rs
  │   ├── l1/                 # Short-Term Memory (SQLite)
  │   │   ├── mod.rs
  │   │   └── sqlite/         # SQLite 操作（独立文件）
  │   ├── l2/                 # Semantic Memory
  │   │   ├── mod.rs
  │   │   ├── bm25.rs
  │   │   └── embedding.rs
  │   ├── l3/                 # Archive Memory
  │   │   └── mod.rs
  │   └── errors.rs           # 统一错误类型
  └── tests/
      ├── integration/
      │   ├── write_recall.rs
      │   ├── crash_recovery.rs
      │   └── archival.rs
      └── chaos/
          └── sudden_kill.rs
```

---

## 10. 实现优先级

**第一阶段（必须完整测试）**
1. L0 + L1（完整实现 + crash recovery 测试）
2. L2 BM25（语义等第二阶段）
3. 集成测试框架

**第二阶段（可选，后验）**
4. L2 向量语义召回
5. L3 归档任务
6. HTTP API 层

---

## 11. 已知未知风险清单

> 以下是 v1/v2 审查中暴露但尚未在设计中完全解决的问题。任何实现都应优先处理这些。

### R1. 向量与原始数据的一致性（Last-Write-Wins 陷阱）

**问题**: 并发写入同一个 key，last-write-wins 语义索引的是旧值，向量是新值（或相反）。

**影响**: `recall_semantic()` 返回的 cosine similarity 基于旧内容，但展示给用户的 entry.value 是新值。

**缓解方案**:
- 写入时附带递增 `sequence_number`
- 向量存储时带 `sequence_number`
- 召回时若本地 `sequence_number > 向量 sequence_number`，降级为纯 BM25，并 log warning
- 最终方案：写入路径和索引路径串行化，不接受"异步索引导致的不一致"

### R2. 删除不是真正删除

**问题**: L1 删除后，L2 向量索引中可能还有旧向量，语义召回返回一个"内容已被删除"的 entry。

**影响**: 用户看到一条召回结果，内容已不存在（已被用户主动删除）。

**缓解方案**:
- 删除路径：`delete(key)` → 发 `delete_event` → L2 消费 → 从向量索引移除 → 从 BM25 移除
- L3 消费 `delete_event` → 已归档的不恢复（归档是最终态）
- 实现 `delete_event` channel，不依赖 L1 的定期扫描

### R3. L2 异步索引窗口（幽灵数据）

**问题**: `remember()` 返回成功（L1 已确认），但 L2 还未生成向量，语义召回找不到它。

**影响**: 对用户是隐式行为——"我明明写成功了，为什么搜不到"。

**缓解方案**:
- API 层面明确语义：`write()` 返回后，`recall_semantic()` 在 60s 内可用是正常
- 写成功响应中包含 `indexed: false` 提示（如果 L2 向量还未就绪）
- 或：写入路径同步完成后才返回（L2 索引同步完成后再返回给调用方）

### R4. 向量索引是单点，丢失代价极高

**问题**: L2 全内存，进程重启后向量全部丢失。10 万条记录重建需要 ~28 小时（Gemini 限速 60 req/min），费用约 $10/次。

**影响**: 每次重启都有不可接受的恢复时间窗口。

**缓解方案**:
- 向量必须持久化到磁盘（JSON 或 sqlite）
- 启动时从持久化向量文件恢复，不重新调用 API
- 只对"新增且无持久化向量"的 entry 补充调用 API
- 持久化格式：每条向量存 `key, version_id, vector[]` 三元组

### R5. NaN/Inf 向量静默污染整个索引

**问题**: Gemini API 异常输出 non-finite float，cosine similarity 返回 NaN，全部语义召回失效。

**影响**: 静默故障，无任何告警，用户只会觉得"语义搜索不好用了"。

**缓解方案**:
- 每条向量入库前必须校验：`vec.iter().all(|x| x.is_finite())`
- 校验失败 → 拒绝写入向量，记录 error metric，**不影响 L1 数据**
- L2 向量损坏不影响 L1/L0 正常工作，降级为纯 BM25

### R6. Private 数据仅在 API 层过滤（多租户风险）

**问题**: `recall()` 的 layer 过滤在 API 层做，但 L2 的 `bm25_recall` 读的是完整 store。如果 L2 有 B 的 private 数据，而 A 调用 `recall()` 不带 layer 过滤，A 能搜到 B 的 private 数据。

**影响**: 数据隔离失效，private 不 private。

**缓解方案**:
- L2 内部按 `layer` 分 Shard：L2_Private_Store / L2_Public_Store
- 召回时只能访问对应 Shard，不存在跨 Shard 查询
- API 层的 layer 过滤作为防御性检查，不作为主要隔离机制

### R7. 冷启动 thundering herd（rebuild 期间状态不一致）

**问题**: 重启时 rebuild 从 L1 全量读，BM25 state doc_count 持续累加，此时若有新写入，BM25 state 和 store 完全不一致。

**影响**: rebuild 期间（可能持续数分钟）L2 召回结果错误。

**缓解方案**:
- rebuild 期间设置 L2 为 `rebuilding` 状态，拒绝语义召回请求（返回 503）
- 或：rebuild 采用增量模式，只重建 `last_modified > last_rebuild_timestamp` 的 entry
- 禁止在 rebuild 期间接受新的 `write()` 调用（通过锁或状态机）

### R8. 可观测性缺失

**问题**: 任何一层出错，用户只收到"找不到"或超时，无法诊断是 L1/L2/L3 哪层的问题。

**缓解方案**:
- 每层暴露 `/health`（存活）+ `/metrics`（队列长度、操作延迟、错误率）
- L2 embed 失败超过阈值 → circuit breaker 开启，语义召回自动降级为纯 BM25
- 关键指标：L2 向量生成延迟、L1 写入 QPS、GC 触发频率、rebuild 进度

### R9. 向量模型版本漂移

**问题**: embedding 模型升级后，旧向量和新 query 的 cosine similarity 不再有效（分布变化）。

**影响**: 语义召回质量逐渐下降，用户无感知。

**缓解方案**:
- 每条向量带 `version_id`（模型名称 + 训练日期，如 `gemini-embedding-004-2025-01`）
- cosine similarity 只比较 `version_id` 相同的向量
- 不同 version 的 entry 不参与混合分数，log warning
- 后台任务：渐进式 re-embed 旧版本向量（每次 100 条，不影响在线服务）

---

## 12. 缓解方案实施对照表

| 风险 | 缓解方案 | 实施位置 | 优先级 |
|------|---------|---------|-------|
| R1 向量/数据不一致 | 写入与索引串行，或 sequence number 校验 | L2 write path | P0 |
| R2 删除不是真正删除 | delete_event channel，L2/L3 消费 | L1 delete + L2 consumer | P0 |
| R3 幽灵数据窗口 | API 明确语义，或同步完成后返回 | API layer | P1 |
| R4 向量丢失代价高 | 向量持久化到磁盘，增量重建 | L2 init + persist | P0 |
| R5 NaN/Inf 污染 | 入库前 is_finite() 校验 | L2 embed_and_cache | P0 |
| R6 Private 隔离失效 | L2 内部按 layer 分 Shard | L2 store architecture | P1 |
| R7 rebuild 状态不一致 | rebuild 期间拒绝召回，或增量 rebuild | L2 init | P1 |
| R8 可观测性缺失 | /health + /metrics + circuit breaker | API layer | P1 |
| R9 模型版本漂移 | version_id + 渐进 re-embed | L2 background task | P2 |

**P0 = 必须实现，P1 = 上线前实现，P2 = 迭代优化**

---

## 13. 数据结构定义

### Entry（记忆条目）

```rust
struct Entry {
    /// 唯一标识符（UUID v4），不可为空
    id: String,
    /// 记忆的语义 key，格式：<layer>:<category>:<subcategory>:<identifier>
    /// 示例："private:project:memory-v2:001"
    key: String,
    /// 记忆原文内容
    value: String,
    /// 重要性等级（影响淘汰优先级）
    importance: Importance,
    /// 自定义标签，用于过滤召回
    tags: Vec<String>,
    /// 记忆来源 agent ID
    source: String,
    /// 访问层级
    layer: Layer,
    /// 创建时间
    created_at: DateTime<Utc>,
    /// 最后访问时间（自动更新）
    last_accessed: DateTime<Utc>,
    /// 过期时间（None 表示永不过期）
    expires_at: Option<DateTime<Utc>>,
}

enum Importance {
    Critical = 4,  // 不可删除，不受 TTL 限制
    High     = 3,
    Normal   = 2,
    Low      = 1,  // TTL 到达时优先删除
}

enum Layer {
    Private,  // 仅创建者可读
    Public,   // 所有 agent 可读
}
```

### RecallRequest（召回请求）

```rust
struct RecallRequest {
    /// 搜索语义（关键词或自然语言）
    query: String,
    /// 精确 key 列表（优先精确查找，可选）
    keys: Option<Vec<String>>,
    /// 限定召回来源 agent（可选）
    agent_id: Option<String>,
    /// 按标签过滤（AND 逻辑）
    tags: Option<Vec<String>>,
    /// 返回上限（默认 10）
    limit: Option<usize>,
    /// 限定层级（默认全部）
    layer: Option<Layer>,
}

struct RecallResult {
    entry: Entry,
    /// 综合相似度分数（0.0 ~ 1.0）
    score: f32,
    /// 召回来源层（"L0" / "L1" / "L2:BM25" / "L2:Gemini" / "L2:Hybrid"）
    from_layer: String,
}
```

### MemoryStats（系统状态）

```rust
struct MemoryStats {
    l0_entries: usize,
    l1_entries: usize,
    l2_entries: usize,
    l2_vectors_cached: usize,
    l2_pending_vectors: usize,
    l3_archived_files: usize,
    /// 各层健康状态（None = 正常，Some(msg) = 异常）
    layer_health: HashMap<&'static str, Option<String>>,
}
```

### Error 类型

```rust
enum MemoryError {
    // L1 错误
    L1WriteFailed(String),     // 写入失败（磁盘满、权限等）
    L1NotFound(String),       // key 不存在

    // L2 错误
    L2WriteFailed(String),     // 写入失败
    L2EmbedFailed(String),     // embedding API 调用失败
    L2InvalidVector(String),   // 向量校验失败（NaN/Inf）
    L2Rebuilding,             // 正在 rebuild，拒绝访问

    // L3 错误
    L3ArchiveFailed(String),   // 归档写入失败
    L3ImportFailed(String),    // 启动时导入失败

    // API 错误
    ApiKeyMissing,            // 未提供 API Key
    ApiKeyInvalid,            // API Key 错误
    InvalidRequest(String),     // 请求格式错误

    // 系统错误
    Internal(String),          // 内部未分类错误
}
```

---

## 14. Event Bus 设计

> L1 和 L2/L3 之间通过事件总线通信。避免 v1 中"WAL 忘了接"的隐式依赖问题。

### 事件类型

```rust
enum MemoryEvent {
    /// 新增或更新记忆
    Upsert {
        key: String,
        entry: Entry,
        /// 事件序列号，单调递增，用于检测重复和乱序
        seq: u64,
    },
    /// 删除记忆
    Delete {
        key: String,
        seq: u64,
    },
    /// L2 向量生成完成（内部事件，不跨层）
    VectorReady {
        key: String,
    },
}
```

### Channel 设计

```
L1 (生产者)
  │ Upsert / Delete 事件
  ▼
crossbeam-channel (bounded, 10000)
  │ 全双工，广播给 L2 和 L3
  ▼
L2 消费者 ←─ L2 处理 Upsert/Delete，更新向量索引
L3 消费者 ←─ L3 处理 Delete（L3 不消费 Upsert，只在 L1 淘汰时归档）
```

**为什么不选 pub/sub 或 message queue？**
- pub/sub（如 redis pubsub）：消费端挂掉会丢消息，内存 channel 有 buffer 可以缓冲
- 持久化 MQ（如 kafka）：过重，L1/L2/L3 都在同一进程内，不需要分布式
- 当前进程内 channel：最轻量，崩溃时 L1 本身靠 SQLite WAL 恢复，不依赖 channel

### L1 事件生产者

```rust
impl L1 {
    // 写入成功后，立即发送 Upsert 事件（不等待确认）
    fn write_and_publish(&self, key: String, entry: Entry) -> Result<()> {
        // 1. SQLite write + fsync（同步，等待落盘）
        self.inner_write(&key, &entry)?;

        // 2. 发送事件（失败不阻塞写入，因为 L1 数据已确认）
        let event = MemoryEvent::Upsert {
            key: key.clone(),
            entry: entry.clone(),
            seq: self.next_seq(),
        };
        if let Err(e) = self.event_tx.send(event) {
            tracing::warn!("L2/L3 消费者离线，事件丢弃: {}", e);
        }
        Ok(())
    }

    // 删除同理
    fn delete_and_publish(&self, key: &str) -> Result<()> {
        self.inner_delete(key)?;
        let event = MemoryEvent::Delete {
            key: key.to_string(),
            seq: self.next_seq(),
        };
        let _ = self.event_tx.send(event);
        Ok(())
    }
}
```

### L2 消费者行为

```rust
impl L2 {
    fn run_event_consumer(event_rx: crossbeam_channel::Receiver<MemoryEvent>) {
        loop {
            match event_rx.recv() {
                Ok(MemoryEvent::Upsert { key, entry, seq: _ }) => {
                    // 1. 更新 store
                    self.store.insert(key.clone(), entry.clone());

                    // 2. 加入 pending_keys（供向量生成任务消费）
                    self.pending_keys.lock().unwrap().insert(key);
                }
                Ok(MemoryEvent::Delete { key, seq: _ }) => {
                    // 1. 从 store 删除
                    self.store.remove(&key);

                    // 2. 从向量索引删除
                    self.vectors.write().unwrap().remove(&key);

                    // 3. 从 BM25 删除（重建索引该 key 的倒排链）
                    self.remove_key_from_bm25(&key);
                }
                Err(_) => {
                    // Channel 断开，退出消费者
                    break;
                }
            }
        }
    }
}
```

### 重复和乱序处理

- 每个事件带单调递增 `seq`
- L2 消费者维护 `processed_seq: u64`
- 收到的 `seq <= processed_seq` → 丢弃（重复或旧事件）
- `seq > processed_seq + 1` → **警告**：中间丢了事件，需要从 L1 修复

### Channel 容量设计

| 参数 | 值 | 理由 |
|------|----|------|
| channel buffer | 10,000 | 足够 L2/L3 消费者离线数分钟内不丢事件 |
| 消费者超时 | 100ms | 单个事件处理超过 100ms 则降速或告警 |
| 全程无锁 | — | 写入路径（L1）和消费者（L2/L3）无锁竞争 |

---

## 15. HTTP API 规格

### 认证

所有 API 需要 header：`X-API-Key: <token>`
未配置 `MEMORY_API_KEY` 时跳过认证。

### 端点列表

| 方法 | 路径 | 描述 |
|------|------|------|
| GET | `/health` | 系统健康检查 |
| GET | `/stats` | 各层条目数量 |
| POST | `/remember` | 写入记忆 |
| GET | `/recall` | 召回记忆（默认 BM25） |
| GET | `/recall?semantic=true` | 语义召回（BM25 + Gemini） |
| GET | `/get` | 精确获取单条记忆 |
| DELETE | `/delete` | 删除记忆 |
| GET | `/metrics` | Prometheus 格式指标 |

---

### POST /remember

**Request:**
```json
{
  "key": "private:project:memory-v2:001",
  "value": "这是记忆内容",
  "importance": "normal",
  "tags": ["项目", "笔记"],
  "layer": "private",
  "expires_at": null
}
```

**Response (200):**
```json
{
  "ok": true,
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "indexed": false,
  "note": "语义索引将在 60s 内可用"
}
```

**Response (400):**
```json
{
  "ok": false,
  "error": "InvalidRequest",
  "message": "key 必须包含 layer: 前缀，如 private:xxx"
}
```

**Response (500):**
```json
{
  "ok": false,
  "error": "L1WriteFailed",
  "message": "磁盘已满，无法写入"
}
```

---

### GET /recall

**Request:**
```
GET /recall?query=水果&limit=10&layer=private&semantic=false
```

| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| query | string | — | 搜索语义（必需） |
| limit | int | 10 | 返回上限 |
| layer | string | 全部 | `private` / `public` |
| semantic | bool | false | 是否启用语义向量召回 |

**Response (200):**
```json
{
  "ok": true,
  "results": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "key": "private:project:memory-v2:001",
      "value": "我喜欢吃苹果和香蕉",
      "score": 0.95,
      "from_layer": "L2:BM25",
      "importance": "normal",
      "tags": ["项目"],
      "created_at": "2026-03-25T10:00:00Z"
    }
  ],
  "degraded": false,
  "degraded_reason": null,
  "total_l0": 5,
  "total_l1": 42,
  "total_l2": 1024
}
```

**语义召回降级响应：**
```json
{
  "ok": true,
  "results": [...],
  "degraded": true,
  "degraded_reason": "L2 Gemini unavailable, using BM25 only",
  "total_l0": 5,
  "total_l1": 42,
  "total_l2": 1024
}
```

---

### GET /stats

**Response (200):**
```json
{
  "ok": true,
  "layers": {
    "l0": { "entries": 12 },
    "l1": { "entries": 234 },
    "l2": { "entries": 1847, "vectors_cached": 1600, "pending": 247 },
    "l3": { "archived_files": 89 }
  },
  "health": {
    "l0": null,
    "l1": null,
    "l2": "rebuilding (23%)",
    "l3": null
  }
}
```

---

### GET /health

**Response (200):**
```
OK
```

**Response (503):**
```
DEGRADED: L2 unrecoverable
```

---

### GET /metrics

Prometheus text format：
```
# HELP memory_l1_entries_total L1 entry count
# TYPE memory_l1_entries_total gauge
memory_l1_entries_total 234

# HELP memory_l2_embed_errors_total L2 embedding errors
# TYPE memory_l2_embed_errors_total counter
memory_l2_embed_errors_total 12

# HELP memory_write_duration_seconds Write latency
# TYPE memory_write_duration_seconds histogram
memory_write_duration_seconds_bucket{le="0.01"} 1000
memory_write_duration_seconds_bucket{le="0.1"} 10000
memory_write_duration_seconds_bucket{le="1"} 50000
memory_write_duration_seconds_sum 5000.0
memory_write_duration_seconds_count 100000
```

---

## 16. 降级策略（Degradation Map）

> 当某一层不可用时，系统整体如何降级，而非整体崩溃。

```
正常状态：
  recall() → L0 → L1 → L2(BM25+Gemini) → 结果

L2 语义通道挂（Gemma API 超时/报错了）：
  recall_semantic() → 自动降级为纯 BM25
  → 响应中带 "degraded": true, "degraded_reason": "..."
  → 不阻塞用户，不报错，只提示质量下降

L2 完全挂（向量索引损坏、OOM 等）：
  recall() → L0 → L1 → （L2 不可达）
  → 降级为纯 L1 全文搜索
  → 返回结果明显减少，告知用户"L2 不可用，记忆召回不完整"

L1 完全挂（SQLite 崩溃）：
  recall() → L0 → （L1 不可达）
  → 只返回 L0 的结果
  → API 返回 200，但 stats 显示 l1_entries=0
  → 用户感知"记忆只找到最近的 session 内的"

L1 + L2 同时挂：
  recall() → L0 → （全部不可达）
  → 返回 503 Service Unavailable
  → /health 也返回 503

L3 归档任务挂（.md 写入失败）：
  → 不影响任何召回，L3 是归档层，离线可用
  → l3_health 变为 "archive_failed: <reason>"，不影响系统健康

Circuit Breaker 阈值：
  L2 Gemini 连续失败 5 次 → 开启 circuit breaker
  breaker 开启后 60s 内，所有语义召回降级为 BM25
  60s 后自动放行一个请求，失败则继续熔断
```

---

## 17. 安全模型

### 多租户隔离保证

**目标**：Agent A 创建的 `layer=Private` 记忆，Agent B 绝对无法访问。

**具体保证：**

| 场景 | 隔离保证 |
|------|---------|
| Agent A 写 `private:X` → Agent B 召回 | Agent B 的 recall() 只发 `layer=private`，但 B 没有 X 的 key，BM25 搜不到 |
| Agent B 猜到了 Agent A 的 key，尝试 `GET /get?key=private:X` | API 层验证：请求者 `sender_id` 必须等于 entry 的 `source` agent |
| Agent A 的 `private:X` 在 L2 向量索引中 | L2 内部 Shard：L2_Private_Store / L2_Public_Store |
| Agent A 的 `private:X` 在 L3 .md 文件中 | L3 文件名不包含内容，文件本身由 L1 owner 控制访问权限 |

**实现：**

```rust
// API 层：所有记忆操作必须验证 owner
fn verify_ownership(entry: &Entry, requester_agent: &str) -> Result<()> {
    if entry.layer == Layer::Private && entry.source != requester_agent {
        return Err(MemoryError::Unauthorized(
            format!("Agent {} cannot access private memory of {}", requester_agent, entry.source)
        ));
    }
    Ok(())
}

// GET /get?key=xxx
fn handle_get(key: &str, requester_agent: &str) -> Result<Entry> {
    let entry = l1.get(key)?;
    verify_ownership(&entry, requester_agent)?;
    Ok(entry)
}

// L2 内部：按 layer 分 Shard 查询
impl L2 {
    fn recall_private(&self, query: &str) -> Vec<RecallResult> { /* 只查 private shard */ }
    fn recall_public(&self, query: &str) -> Vec<RecallResult> { /* 只查 public shard */ }
}
```

### API Key 鉴权

- `X-API-Key` header 验证请求者身份
- Key 不对应具体 agent，只用于控制 API 访问权限
- 飞书等 channel 传来的消息，自动注入 `sender_id`（来自飞书 user_open_id）

### Public 记忆

- `layer=Public` 的记忆所有 agent 可见
- 适用于团队共享上下文、规则、系统 prompt 等

### 默认行为

- 新建记忆默认 `layer=Private`（安全第一）
- 除非显式指定 `layer: "public"`，否则都是私有


---

## 18. L0 LRU 算法实现细节

### 为什么不用标准库的 LruCache

标准库 `lru_cache` 不支持并发安全，且无法在淘汰时触发回调（更新 `last_accessed`）。自实现更灵活。

### 数据结构

```rust
use std::collections::HashMap;
use std::sync::RwLock;

struct LruEntry {
    value: Arc<Entry>,
    order: usize,    // 访问顺序，越小越旧
}

pub struct WorkingMemory {
    store: DashMap<String, Arc<Entry>>,
    // LRU 淘汰队列：O(1) 插入和删除
    lru_index: RwLock<HashMap<String, usize>>,  // key → order
    access_order: RwLock<usize>,                 // 单调递增计数器
    capacity: usize,
}
```

### 访问时更新 LRU

```rust
impl WorkingMemory {
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        // 1. DashMap 读取（无锁并发）
        let value = self.store.get(key)?.value().clone();

        // 2. 更新 LRU 顺序
        let order = {
            let mut orders = self.lru_index.write().unwrap();
            let next = {
                let mut counter = self.access_order.write().unwrap();
                *counter += 1;
                *counter
            };
            orders.insert(key.to_string(), next);
            next
        };

        // 3. 如果超过容量，淘汰最老的
        let to_evict = {
            let orders = self.lru_index.read().unwrap();
            if orders.len() > self.capacity {
                // 找最小 order（即最老）
                orders.iter().min_by_key(|(_, o)| *o).map(|(k, _)| k.clone())
            } else {
                None
            }
        };

        if let Some(old_key) = to_evict {
            self.store.remove(&old_key);
            self.lru_index.write().unwrap().remove(&old_key);
        }

        Some(value)
    }
}
```

**时间复杂度：**
- `get`: O(1) 均摊（HashMap 读 + LRU 更新 + 可能淘汰）
- `write`: O(1)（HashMap 写 + 更新 order）
- `evict`: O(n) 找最老（`min()` over HashMap），但只在超过容量时触发

### Session 清理

```rust
impl WorkingMemory {
    /// 清除当前 session 的所有 key（session 结束时调用）
    pub fn clear_session(&self, session_keys: &[String]) {
        for key in session_keys {
            self.store.remove(key);
        }
        let mut orders = self.lru_index.write().unwrap();
        for key in session_keys {
            orders.remove(key);
        }
    }
}
```

---

## 19. L1 SQLite 完整 Schema

### 表结构

```sql
-- entries 主表
CREATE TABLE entries (
    key      TEXT PRIMARY KEY,
    id       TEXT NOT NULL,
    value    TEXT NOT NULL,
    importance INTEGER NOT NULL DEFAULT 2,
    source   TEXT NOT NULL,
    layer    TEXT NOT NULL DEFAULT 'private',
    created_at   INTEGER NOT NULL,  -- Unix timestamp (ms)
    last_accessed INTEGER NOT NULL, -- Unix timestamp (ms)
    expires_at    INTEGER,          -- Unix timestamp (ms), NULL = 不过期
    seq          INTEGER NOT NULL DEFAULT 0  -- 事件序列号
);

CREATE INDEX idx_entries_layer ON entries(layer);
CREATE INDEX idx_entries_importance ON entries(importance);
CREATE INDEX idx_entries_last_accessed ON entries(last_accessed);
CREATE INDEX idx_entries_expires_at ON entries(expires_at) WHERE expires_at IS NOT NULL;

-- 全文搜索虚拟表（FTS5）
CREATE VIRTUAL TABLE entries_fts USING fts5(
    key,
    value,
    content='entries',
    content_rowid='rowid'
);
```

### 写入事务

```rust
impl L1 {
    fn write_tx(&self, key: &str, entry: &Entry) -> Result<()> {
        let json = serde_json::to_string(entry)?;
        let now = Utc::now().timestamp_millis();
        let seq = self.next_seq();

        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO entries
             (key, id, value, importance, source, layer, created_at, last_accessed, expires_at, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                key,
                entry.id,
                json,
                entry.importance as i32,
                entry.source,
                entry.layer as &str,
                entry.created_at.timestamp_millis(),
                now,
                entry.expires_at.map(|dt| dt.timestamp_millis()),
                seq,
            ],
        )?;

        // 同步更新 FTS 索引
        tx.execute(
            "INSERT OR REPLACE INTO entries_fts(key, value) VALUES (?1, ?2)",
            rusqlite::params![key, &entry.value],
        )?;

        tx.commit()?;
        Ok(())
    }
}
```

### 批量读取（rebuild 用）

```rust
impl L1 {
    /// 全量扫描，返回 (key, entry) 迭代器
    fn full_scan(&self) -> impl Iterator<Item = (String, Entry)> + '_ {
        self.db.prepare("SELECT key, id, value, importance, source, layer,
                               created_at, last_accessed, expires_at
                        FROM entries")
            .unwrap()
            .query_map([], |row| {
                let expires_ms: Option<i64> = row.get(8)?;
                Ok((
                    row.get::<_, String>(0)?,
                    Entry {
                        id: row.get(1)?,
                        key: row.get(0)?,
                        value: row.get::<_, String>(2)?,
                        importance: Importance::from(row.get::<_, i32>(3)?),
                        source: row.get(4)?,
                        layer: Layer::from(row.get::<_, String>(5)?),
                        created_at: DateTime::from_timestamp_millis(row.get(6)?).unwrap(),
                        last_accessed: DateTime::from_timestamp_millis(row.get(7)?).unwrap(),
                        expires_at: expires_ms.map(|ms| DateTime::from_timestamp_millis(ms).unwrap()),
                    },
                ))
            })
            .unwrap()
    }
}
```

---

## 20. L2 BM25 完整算法

### BM25 公式

```
score(D, Q) = Σ IDF(qi) × (tf(qi, D) × (k1 + 1))
                         ─────────────────────────────────
                         tf(qi, D) + k1 × (1 - b + b × |D| / avgdl)

IDF(qi) = ln((N - df + 0.5) / (df + 0.5) + 1)
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| k1 | 1.5 | 词频饱和参数，越大越容忍高频词 |
| b | 0.75 | 文档长度归一化参数 |
| avgdl | — | 平均文档长度（从状态中实时计算） |
| N | — | 总文档数（doc_count） |
| df | — | 包含词 qi 的文档数（doc_freq） |

### 实现

```rust
struct Bm25Index {
    doc_count: usize,        // N
    avg_doc_len: f32,       // avgdl
    doc_freq: HashMap<String, usize>,  // df: token → 文档数
    doc_lens: HashMap<String, usize>, // key → token 数（用于加速）
    k1: f32,
    b: f32,
}

impl Bm25Index {
    fn score(&self, doc_tokens: &[String], query_tokens: &[String]) -> f32 {
        let doc_len = doc_tokens.len() as f32;
        let mut total = 0.0f32;

        for qt in query_tokens {
            let df = self.doc_freq.get(qt).copied().unwrap_or(0);
            if df == 0 {
                continue;
            }

            let idf = ((self.doc_count as f32 - df as f32 + 0.5) / (df as f32 + 0.5) + 1e-4).ln() + 1.0;

            let tf = doc_tokens.iter().filter(|t| *t == qt).count() as f32;

            let numerator = tf * (self.k1 + 1.0);
            let denominator = tf + self.k1 * (1.0 - self.b + self.b * doc_len / self.avg_doc_len.max(1.0));
            total += idf * numerator / denominator;
        }
        total
    }
}
```

### 混合融合公式

```rust
fn hybrid_fuse(bm25_results: &[RecallResult], semantic_results: &[RecallResult],
                bm25_weight: f32, semantic_weight: f32) -> Vec<RecallResult> {
    let bm25_max = bm25_results.iter().map(|r| r.score).fold(1e-6, f32::max);

    let mut score_map: HashMap<String, (f32, RecallResult)> = HashMap::new();

    for r in bm25_results {
        let norm = (r.score / bm25_max).clamp(0.0, 1.0);
        score_map.insert(r.entry.id.clone(), (norm * bm25_weight, r.clone()));
    }

    for r in semantic_results {
        let norm = r.score.clamp(0.0, 1.0);
        score_map.entry(r.entry.id.clone())
            .and_modify(|(s, _)| *s += norm * semantic_weight)
            .or_insert((norm * semantic_weight, r.clone()));
    }

    let mut results: Vec<_> = score_map.into_values().map(|(_, mut r)| {
        r.from_layer = "L2:Hybrid";
        r
    }).collect();

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    results
}
```

---

## 21. L3 归档文件格式

### 文件命名

```
archive/
  2026-03-25.md        # 每日一个文件
  2026-03-24.md
  ...
```

### 文件内部格式

```markdown
# 记忆归档 - 2026-03-25

共 23 条记忆 | 归档时间: 2026-03-25T03:00:00Z

---

### private:project:memory-v2:550e8400

**重要性**: High
**来源**: agent-ai
**标签**: [project, design]

我喜欢吃苹果和香蕉

---

### public:system:rules:001

**重要性**: Critical
**来源**: aiboss
**标签**: [system, rules]

团队协作原则：决策前先确认数据来源

---
```

### 导入格式校验

```rust
impl L3 {
    fn parse_archive_entry(raw: &str) -> Option<Entry> {
        let lines: Vec<&str> = raw.split("\n---\n").collect();
        if lines.len() < 2 {
            return None;
        }
        let header = lines[0].trim();
        let body = lines[1].trim();

        // 解析 ## key 和 **重要性**: 等元信息
        let key = header.trim_start_matches("## ").to_string();

        Some(Entry {
            id: uuid::Uuid::new_v4().to_string(), // 归档时丢弃 id，重新生成
            key,
            value: body.to_string(),
            importance: Importance::Normal,
            source: "l3-archive".to_string(),
            layer: Layer::Private,
            created_at: chrono::Utc::now(),
            last_accessed: chrono::Utc::now(),
            expires_at: None,
        })
    }
}
```

---

## 22. 环境变量配置清单

| 变量 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `MEMORY_API_KEY` | string | (无) | API 鉴权 key，未设置则跳过验证 |
| `MEMORY_L0_MAX_ENTRIES` | usize | 10000 | L0 最大条目数 |
| `MEMORY_L1_PATH` | string | `/tmp/memory_v2_l1` | SQLite 数据库路径 |
| `MEMORY_L1_MAX_ENTRIES` | usize | 50000 | L1 最大条目数（触发淘汰） |
| `MEMORY_L1_MAX_AGE_SECS` | i64 | 604800 (7天) | L1 TTL 秒数 |
| `MEMORY_L1_GC_INTERVAL_SECS` | u64 | 3600 | GC 任务间隔 |
| `MEMORY_L2_MAX_ENTRIES` | usize | 100000 | L2 最大条目数 |
| `MEMORY_L2_PENDING_BATCH` | usize | 500 | 每批处理的 pending 向量数 |
| `MEMORY_L2_SYNC_INTERVAL_SECS` | u64 | 300 | L2 后台同步间隔 |
| `MEMORY_L2_EMBED_BATCH_SIZE` | usize | 50 | Gemini API 每批 embedding 条目数 |
| `MEMORY_L3_ARCHIVE_DIR` | string | `workspace/memory-archive` | 归档目录 |
| `MEMORY_L3_ARCHIVE_AFTER_DAYS` | i64 | 30 | 超过多少天归档 |
| `MEMORY_L3_GC_INTERVAL_SECS` | u64 | 86400 | L3 GC 任务间隔（每日） |
| `GEMINI_EMBEDDINGS_TOKEN` | string | (无) | Gemini API key，未设置则降级为纯 BM25 |
| `MEMORY_WAL_ENABLED` | bool | false | 备用 WAL（SQLite 已内置，此变量忽略） |

### 配置加载顺序

```rust
impl Config {
    fn from_env() -> Self {
        Self {
            l0_capacity: env_var_or("MEMORY_L0_MAX_ENTRIES", 10_000),
            l1_path: env_var_or("MEMORY_L1_PATH", "/tmp/memory_v2_l1"),
            l1_max_entries: env_var_or("MEMORY_L1_MAX_ENTRIES", 50_000),
            l1_ttl_secs: env_var_or("MEMORY_L1_MAX_AGE_SECS", 604_800),
            l2_max_entries: env_var_or("MEMORY_L2_MAX_ENTRIES", 100_000),
            l2_sync_interval: env_var_or("MEMORY_L2_SYNC_INTERVAL_SECS", 300),
            l3_archive_dir: env_var_or("MEMORY_L3_ARCHIVE_DIR", "memory-archive"),
            l3_archive_after_days: env_var_or("MEMORY_L3_ARCHIVE_AFTER_DAYS", 30),
            gemini_key: std::env::var("GEMINI_EMBEDDINGS_TOKEN").ok(),
            api_key: std::env::var("MEMORY_API_KEY").ok(),
        }
    }
}
```

---

## 23. 性能目标与容量规划

### 延迟目标

| 操作 | P50 | P99 | 说明 |
|------|-----|-----|------|
| L0 write | < 1µs | < 10µs | DashMap 纯内存 |
| L1 write (确认) | < 5ms | < 50ms | SQLite fsync |
| L1 read | < 1ms | < 10ms | mmap 读 |
| L2 BM25 recall | < 10ms | < 100ms | HashMap + token 遍历 |
| L2 语义 recall | < 200ms | < 2000ms | 含 Gemini API 延迟 |
| L3 归档写入 | < 100ms | < 1000ms | 文件系统 fsync |

### 容量规划

| 规模 | L0 | L1 | L2 向量 |
|------|----|----|---------|
| 个人用户 (~1000 条) | 10K | 5万条 ~50MB | ~200MB |
| 小团队 (~10 agents) | 10K | 5万条 ~50MB | ~1GB |
| 大型部署 (~100 agents) | 10K | 10万条 ~100MB | ~10GB |

**L2 向量估算**：每条 ~768维 × 4字节(float) = ~3KB，10万条约 300MB，加上 BM25 索引约 1-2GB。

**内存需求**：
- L0: ~10MB (1万条引用)
- L1: ~100MB (SQLite mmap)
- L2: ~2GB (含向量和索引)
- 合计: ~2.1GB（无 L3）

---

## 24. 生命周期状态机

一条记忆从创建到归档的完整状态变迁：

```
                    [Created]
                        │
          ┌─────────────┼─────────────┐
          ▼             ▼             ▼
      [L0 only]    [L0 + L1]    [L0 + L1 + L2]
          │             │             │
          │             │             │ L2向量生成
          │             │             ▼
          │             │      [Indexed]
          │             │             │
          ▼             ▼             ▼
      [Evicted]    [TTL expired]  [30 days]
      (L0删除)     (L1删除)      (归档触发)
          │             │             │
          ▼             ▼             ▼
       [Gone]       [Gone]       [Archived to .md]
                                     │
                                     ▼
                              [L3 Shard]
                                     │
                                     │ 重新导入(可选)
                                     ▼
                               [Back to L1]
```

### 各状态说明

| 状态 | 在哪些层 | 是否可召回 |
|------|---------|-----------|
| `L0 only` | L0 | recall() |
| `L0 + L1` | L0, L1 | recall() |
| `L0 + L1 + L2` | L0, L1, L2 | recall() 含语义 |
| `Indexed` | L0, L1, L2(有向量) | recall_semantic() |
| `Evicted` | — | 已删除 |
| `TTL expired` | — | 已删除 |
| `Archived` | L3 | recall() 在 L2 miss 后提示 |

---

## 25. 启动与关闭流程

### 启动顺序（必须按此顺序）

```rust
impl MemorySystem {
    /// 按顺序初始化各层，不允许跨层隐式依赖
    pub async fn new() -> Result<Self> {
        // 1. L1 最先启动（所有层的 source of truth）
        let l1 = Arc::new(L1::new().await?);

        // 2. L2 启动，从 L1 重建状态（rebuild_from_l1）
        let l2 = Arc::new(L2::new(l1.clone()).await?);

        // 3. L3 启动，扫描归档文件（如有）
        let l3 = Arc::new(L3::new(l1.clone()).await?);

        // 4. L0 最后（进程内存，随时可重建）
        let l0 = Arc::new(WorkingMemory::new());

        // 5. 启动后台任务（Event Bus 消费者、GC 任务等）
        //    在所有层初始化完成后才启动，防止消费者读到半初始化状态
        Self::start_background_tasks(&l1, &l2, &l3)?;

        Ok(Self { l0, l1, l2, l3 })
    }
}
```

### Graceful Shutdown

```rust
impl MemorySystem {
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("MemorySystem 关闭中...");

        // 1. 停止接受新请求（关闭 HTTP listener）
        // 2. 等待现有请求完成（带 30s 超时）

        // 3. 停止后台任务
        self.l2.stop_sync_task().await?;  // 停止 L2 向量同步
        self.l3.stop_archive_task().await?; // 停止归档任务

        // 4. 强制 flush 所有层
        self.l1.flush()?;                  // SQLite checkpoint

        // 5. 确认 L3 归档文件全部写入
        self.l3.finalize_today_archive()?; // 关闭今日归档文件

        tracing::info!("MemorySystem 已关闭，所有数据已持久化");
        Ok(())
    }
}
```

---

## 26. 备份与灾难恢复

### 备份策略

| 备份类型 | 频率 | 内容 | 存储位置 |
|---------|------|------|---------|
| L1 SQLite | 每日一次 + 每次大操作前 | 完整 .db 文件 | S3/本地 backup 目录 |
| L2 向量索引 | 每小时增量 | delta JSON | 同 L1 backup |
| L3 归档 | 实时（已持久化到文件系统） | — | 文件系统快照 |

### 恢复流程

```
场景：数据库损坏（L1 .db 文件无法打开）

1. 检测：SQLite 打开报错 → 标记 l1_health = "corrupted"
2. 从最近 backup 复制 L1 .db
3. 重启 L1（自动 replay SQLite WAL）
4. L2 触发 rebuild_from_l1
5. L3 扫描归档目录，确认文件完整性
6. health check 通过 → 恢复正常服务
```

### 备份命令（运维用）

```bash
# 备份 L1
cp /tmp/memory_v2_l1 /backup/memory_v2_l1.$(date +%Y%m%d).db

# 备份 L2 向量索引
cp /tmp/memory_v2_l2_vectors.json /backup/l2_vectors.$(date +%Y%m%d).json

# 备份 L3 归档
rsync -av /workspace/memory-archive/ /backup/archive/
```

---

## 27. 可配置阈值与调优指南

### Circuit Breaker 参数

| 参数 | 默认值 | 调优建议 |
|------|--------|---------|
| 连续失败次数阈值 | 5 | QPS 高时调高（如 20），避免误触发 |
| 熔断持续时间 | 60s | Gemini API 限速时调长 |
| 半开请求数 | 1 | 每次放行一个请求试探 |

### 向量生成批处理

| 参数 | 默认值 | 调优建议 |
|------|--------|---------|
| 每批条目数 | 50 | Gemini API 限制 100条/请求，调低提升并发 |
| 批次间隔 | 100ms | 防止触发 Gemini 限速 |
| 最大 pending 数 | 5000 | 超过时暂停接收新 write |

### GC 参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| GC 触发间隔 | 1h | 过长会导致 L1 内存持续增长 |
| 每轮最大删除数 | 1000 | 防止单次 GC 占用过多 I/O |
| Critical 条目保护 | TTL 内永不清除 | 防止核心记忆被意外删除 |


---

## 28. 一致性保证模型

### 各层保证级别

| 层 | 持久性 | 一致性 | 隔离级别 | 崩溃恢复 |
|----|--------|--------|---------|---------|
| L0 | 无（进程内存） | 单进程读写串行 | — | 丢失 |
| L1 | 落盘（SQLite fsync） | 写后读一致（write-read） | 语句级 | SQLite WAL 恢复 |
| L2 | 内存 + 可选持久化 | 最终一致（有窗口期） | — | 从 L1 重建 |
| L3 | 落盘（文件系统） | 写入即最终 | — | 从 .md 恢复 |

**解读：**
- L0 无任何保证，进程重启即清空，这是设计意图
- L1 是唯一有强一致性保证的层（SQLite ACID）
- L2 的 store（HashMap）和 BM25 state 之间有窗口期不一致，rebuild 时会修正
- L3 一旦归档，永不修改（append-only）

### 读写路径一致性

```
写入路径（强一致）：
  L0(write) ──→ L1(write + fsync) ✓ 确认返回
                     │
                     ▼
              Event Bus (async)
                     │
              ┌──────┴──────┐
              ▼              ▼
           L2 store      L3 archive
        (最终一致)      (归档时一致)

读取路径：
  recall() ──→ L0 ──→ L1 ──→ L2
                  ↑        ↑        ↑
                直接读   直接读   最终一致
              (无锁)   (SQLite)  (可能有窗口)
```

### 并发写入同一个 key

**场景**：Agent A 和 Agent B 同时写 `key=X`，value 不同。

**处理**：SQLite `INSERT OR REPLACE`（Last-Write-Wins），seq 大的覆盖 seq 小的。

**结果**：
- L1：最终只有一条记录，以 seq 大的为准
- L2 的 Event Bus：两个 Upsert 事件顺序不确定，但 seq 大的最终覆盖 seq 小的
- 用户看到的 value 是最后一次写入的值（正确）

**向量不一致风险**（R1）：
- L2 store 已更新为新 value，但旧 value 的向量可能还在处理中
- 处理完成后，向量会被新 seq 的 Upsert 覆盖
- **缓解**：seq 号嵌入向量 key，如 `vec_key = format!("{}_{}", key, seq)`，召回时只取最高 seq 的向量

---

## 29. Event Bus 消费者：从哪个 seq 恢复

### 问题

L2 消费者崩溃后重启，Event Bus channel 里的事件可能已积压（消费者离线期间 L1 继续写入）。从哪个 seq 开始消费？

### 方案：双指针机制

```rust
struct L2 {
    // L2 内存中已处理的最新 seq
    processed_seq: RwLock<u64>,
    // L1 持久化的 checkpoint（用于崩溃恢复）
    checkpoint_seq_key: &'static str = "__l2_checkpoint_seq",
}

impl L2 {
    fn run_event_consumer(&self, event_rx: Receiver<MemoryEvent>) {
        // 1. 从 checkpoint 恢复 seq（重启恢复）
        let mut cursor = self.load_checkpoint().unwrap_or(0);

        // 2. 处理积压事件（从 cursor 之后开始）
        for event in event_rx.iter() {
            let seq = event.seq();

            // 跳过已处理的（消费端重启但 channel 还有旧事件）
            if seq <= cursor {
                tracing::debug!("跳过已处理 seq={}", seq);
                continue;
            }

            // 顺序处理
            self.process_event(event).await;

            // 每处理 100 条持久化一次 checkpoint（异步，不阻塞）
            cursor = seq;
            if seq % 100 == 0 {
                self.save_checkpoint_async(seq);
            }
        }
    }
}
```

### Checkpoint 持久化

```rust
impl L2 {
    fn save_checkpoint_async(&self, seq: u64) {
        // 写入 L1 的 entries 表特殊 key（与其他数据同一事务）
        let entry = Entry {
            id: self.checkpoint_seq_key.to_string(),
            key: self.checkpoint_seq_key.to_string(),
            value: seq.to_string(),
            importance: Importance::Critical,
            source: "l2-checkpoint".to_string(),
            layer: Layer::Private,
            created_at: chrono::Utc::now(),
            last_accessed: chrono::Utc::now(),
            expires_at: None,
        };
        self.l1.write_internal(&self.checkpoint_seq_key, &entry);
    }

    fn load_checkpoint(&self) -> Option<u64> {
        self.l1.get(&self.checkpoint_seq_key)
            .and_then(|e| e.value.parse::<u64>().ok())
    }
}
```

### Dead Letter 事件处理

如果 seq 出现跳跃（`seq > cursor + 1`），说明中间丢了事件：

```rust
if seq > cursor + 1 {
    tracing::warn!("Event seq 跳跃：expected {}，got {}。从 L1 修复...",
                  cursor + 1, seq);
    // 紧急修复：重新从 L1 全量同步，丢弃 L2 当前状态
    self.emergency_rebuild().await;
}
```

---

## 30. 多 Agent 并发写入冲突处理

### 冲突场景

| 场景 | 处理方式 | 说明 |
|------|---------|------|
| 两个 Agent 写同一个 key | LWW（Last-Write-Wins） | seq 大的覆盖 |
| 同一 Agent 并发写同一个 key | 同上 | 客户端保证幂等 |
| Agent A 写，Agent B 同时删 | Delete 优先 | seq 相同时，delete 先处理 |
| 大量 Agent 同时写不同 key | 完全并发 | 无锁，L1 SQLite 内部串行化 |

### Delete 优先于 Upsert

当 seq 相同时，Delete 应先处理：

```rust
impl L2 {
    async fn process_event(&self, event: MemoryEvent) {
        match event {
            MemoryEvent::Delete { key, seq } => {
                self.apply_delete(&key).await;
                self.set_seq(seq);
            }
            MemoryEvent::Upsert { key, entry, seq } => {
                // 只有当 seq >= 当前处理的 seq 时才应用（防止 delete 被覆盖）
                if seq >= self.current_seq.load(Ordering::SeqCst) {
                    self.apply_upsert(key, entry).await;
                    self.set_seq(seq);
                }
            }
        }
    }
}
```

### 写入限流

单个 Agent 写入 QPS 限制：

```rust
// 每个 agent 写入令牌桶：10 次/秒
struct PerAgentRateLimiter {
    buckets: DashMap<String, TokenBucket>,
}

impl PerAgentRateLimiter {
    fn check(&self, agent_id: &str) -> Result<(), MemoryError> {
        let bucket = self.buckets.entry(agent_id.to_string())
            .or_insert_with(|| TokenBucket::new(10, 1.0)); // 10 req/s
        if !bucket.try_acquire() {
            return Err(MemoryError::RateLimited(agent_id.to_string()));
        }
        Ok(())
    }
}
```

---

## 31. 超大 Entry 处理

### 大小限制

| 字段 | 硬上限 | 软限制（建议） |
|------|--------|--------------|
| `value` | **1 MB** | 100 KB |
| `key` | 512 B | 128 B |
| 单次写入 | 1 MB | — |
| 单次召回 | 无限制 | — |
| L2 向量条目 | — | value < 500KB |

超过 1 MB → API 返回 400，拒绝写入（防止恶意灌数据）。

### 大 value 处理策略

```rust
impl L1 {
    fn validate_entry(&self, entry: &Entry) -> Result<(), MemoryError> {
        let value_len = entry.value.len();

        if value_len > 1024 * 1024 {
            return Err(MemoryError::EntryTooLarge {
                key: entry.key.clone(),
                size: value_len,
                max: 1024 * 1024,
            });
        }

        if value_len > 100 * 1024 {
            tracing::warn!("Entry value 较大 ({}KB)，建议拆分",
                           value_len / 1024);
        }

        Ok(())
    }
}
```

### 大 value 的向量处理

超过 500KB 的 value 不生成向量（Gemini embedding 也有 2048 token 限制）：

```rust
impl L2 {
    async fn embed_and_cache(&self, key: &str, value: &str) -> Result<()> {
        let value_len = value.len();

        // 不生成向量的情况
        if value_len > 500 * 1024 {
            tracing::info!("value 超过 500KB，不生成向量: key={}", key);
            self.vectorless_keys.insert(key.to_string()); // 标记为纯 BM25 条目
            return Ok(());
        }

        // 正常向量生成
        let vec = self.embed(value).await?;
        self.vectors.write().unwrap().insert(key.to_string(), vec);
        Ok(())
    }
}
```

---

## 32. FTS5 全文搜索策略

### 索引设计

SQLite FTS5 在 `entries_fts` 虚拟表中维护，与主表 `entries` 通过 `content_rowid` 关联。

```sql
-- entries_fts 随 entries 自动同步（在 L1 write 事务中）
CREATE VIRTUAL TABLE entries_fts USING fts5(
    key,
    value,
    content='entries',
    content_rowid='rowid'
);
```

### BM25 全文召回

```rust
impl L1 {
    fn full_text_search(&self, query: &str, layer: Option<Layer>, limit: usize)
        -> Vec<(String, f32)>  // (key, bm25_score)
    {
        let query = query.replace("'", "''"); // SQL injection 防护

        let layer_filter = match layer {
            Some(Layer::Private) => "AND e.layer = 'private'",
            Some(Layer::Public)  => "AND e.layer = 'public'",
            None => "",
        };

        let sql = format!(
            r#"
            SELECT e.key,
                   bm25(entries_fts, 10.0, 5.0) AS score
            FROM entries_fts
            JOIN entries e ON entries_fts.rowid = e.rowid
            WHERE entries_fts MATCH '{query}'
            {layer_filter}
            ORDER BY score
            LIMIT {limit}
            "#,
        );

        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
        })?;

        rows.filter_map(|r| r.ok()).collect()
    }
}
```

**FTS5 vs BM25 选择策略：**
- FTS5：L1 层的精确关键词召回（低延迟）
- BM25（L2）：L2 层的语义/模糊召回
- 两者可同时使用，结果合并去重

---

## 33. v1 → v2 数据迁移方案

### 迁移时机

**不提供自动迁移**。v1 和 v2 使用不同的存储路径（`/tmp/memory_l1` vs `/tmp/memory_v2_l1`），共存不冲突。用户在确认 v2 稳定后手动迁移。

### 迁移脚本流程

```rust
/// migrate-v1-to-v2
/// 读取 v1 的 sled 数据库，输出迁移 SQL 文件
fn migrate_v1_to_v2(v1_path: &str, output_sql: &str) -> Result<MigrationReport> {
    let db = sled::open(v1_path)?;
    let tree = db.open_tree(b"entries")?;

    let mut sql_lines = Vec::new();
    let mut total = 0;
    let mut skipped = 0;

    for item in tree.iter() {
        let (_, v) = item?;
        let entry: EntryV1 = serde_json::from_str(std::str::from_utf8(&v)?)?;

        // 字段映射：v1 → v2
        let v2_entry = EntryV2 {
            id: entry.id,
            key: entry.key,
            value: entry.value,
            importance: entry.importance,
            source: entry.source.unwrap_or("v1-migration".to_string()),
            layer: entry.layer,
            created_at: entry.created_at,
            last_accessed: entry.last_accessed,
            expires_at: entry.expires_at,
        };

        // 生成 INSERT 语句（检查 key 冲突）
        sql_lines.push(format!(
            "INSERT OR IGNORE INTO entries VALUES ('{}', '{}', '{}', {}, '{}', '{}', {}, {}, {}, {});",
            v2_entry.key.replace("'", "''"),
            v2_entry.id,
            v2_entry.value.replace("'", "''"),
            v2_entry.importance as i32,
            v2_entry.source.replace("'", "''"),
            v2_entry.layer,
            v2_entry.created_at.timestamp_millis(),
            v2_entry.last_accessed.timestamp_millis(),
            v2_entry.expires_at.map(|dt| dt.timestamp_millis()).unwrap_or(-1),
            0_i64, // seq = 0 (迁移数据无 seq)
        ));

        total += 1;
    }

    // 输出 SQL 文件
    std::fs::write(output_sql, sql_lines.join("\n"))?;

    Ok(MigrationReport {
        total,
        skipped,
        output_path: output_sql.to_string(),
    })
}
```

### 迁移步骤

```bash
# 1. 停止 v1 服务（读写停止）
# 2. 备份 v1 数据
cp -r /tmp/memory_l1 /backup/memory_l1.v1.$(date +%Y%m%d)

# 3. 运行迁移脚本（只读，不修改 v1 数据）
cargo run -- migrate --from /tmp/memory_l1 --output /tmp/migrate.sql

# 4. 导入 v2 数据库
sqlite3 /tmp/memory_v2_l1 < /tmp/migrate.sql

# 5. 启动 v2 服务
cargo run -- serve

# 6. 验证数据完整性
cargo run -- verify-stats
# 对比 v1 stats 和 v2 stats 条目数是否一致
```

---

## 34. OpenClaw Agent 系统集成

### 集成架构

```
OpenClaw Agent (aicode / ainews / ...)
      │
      │ sessions_send(agentId="memory", message=...)
      ▼
MemorySystem (v2)
  ├── L0: 当前 agent session 内存
  ├── L1: 跨 agent 共享短期记忆
  ├── L2: 语义召回层
  └── L3: 归档层
```

### Agent 记忆操作接口

```rust
// OpenClaw Agent 调用记忆的接口（通过 sessions_send / sessions_spawn）

// 写入记忆
sessions_send(agentId="memory", message="
  ACTION=remember
  KEY=private:project:design:001
  VALUE=四层记忆系统设计完成，v2 解决了 v1 的 WAL 脱节问题
  IMPORTANCE=high
  TAGS=design,memory-system
")

// 召回记忆
sessions_send(agentId="memory", message="
  ACTION=recall
  QUERY=四层记忆系统设计
  LAYER=private
  LIMIT=5
")

// 精确获取
sessions_send(agentId="memory", message="
  ACTION=get
  KEY=private:project:design:001
")

// 删除记忆
sessions_send(agentId="memory", message="
  ACTION=delete
  KEY=private:project:design:001
")
```

### OpenClawBridge 实现

```rust
impl OpenClawBridge {
    /// 处理来自 Agent 的消息（sessions_send 的入口）
    pub fn handle_agent_message(&self, msg: &str) -> String {
        let lines: HashMap<&str, &str> = msg
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();

        match (lines.get("ACTION"), lines.get("KEY")) {
            (Some(&"remember"), Some(key)) => {
                let entry = Entry {
                    id: uuid::Uuid::new_v4().to_string(),
                    key: key.to_string(),
                    value: lines.get("VALUE").unwrap_or(&"").to_string(),
                    importance: parse_importance(lines.get("IMPORTANCE")),
                    tags: parse_tags(lines.get("TAGS")),
                    source: lines.get("AGENT_ID").unwrap_or("unknown").to_string(),
                    layer: Layer::Private,
                    created_at: chrono::Utc::now(),
                    last_accessed: chrono::Utc::now(),
                    expires_at: None,
                };
                let rt = block_on(self.memory.remember(entry));
                serde_json::to_string(&rt).unwrap_or_default()
            }
            (Some(&"recall"), _) => {
                let req = RecallRequest {
                    query: lines.get("QUERY").unwrap_or(&"").to_string(),
                    keys: None,
                    agent_id: lines.get("AGENT_ID").map(String::from),
                    tags: parse_tags(lines.get("TAGS")),
                    limit: lines.get("LIMIT").and_then(|s| s.parse().ok()),
                    layer: lines.get("LAYER").and_then(|s| parse_layer(s)),
                };
                let results = block_on(self.memory.recall(req));
                serde_json::to_string(&results).unwrap_or_default()
            }
            (Some(&"get"), Some(key)) => {
                let entry = block_on(self.memory.get(key));
                serde_json::to_string(&entry).unwrap_or_default()
            }
            (Some(&"delete"), Some(key)) => {
                let rt = block_on(self.memory.delete(key));
                serde_json::to_string(&rt).unwrap_or_default()
            }
            _ => r#"{"error": "unknown_action"}"#.to_string(),
        }
    }
}
```

### Agent Session 生命周期

```
Agent Session 启动
      │
      ▼
L0.write(session_key, entry)   ← session 上下文写入 L0
      │
      │ Agent 运行中
      ▼
recall() ──→ L0(hit)          ← 当前 session 上下文
      │
      │ L0 miss
      ▼
recall() ──→ L1(hit)           ← 跨 session 共享记忆
      │
      │ L1 miss
      ▼
recall() ──→ L2(hit)           ← 语义召回
      │
      │ L2 miss + L3 存在
      ▼
提示: "可能在归档中 /memory-archive/YYYY-MM-DD.md"

Agent Session 结束
      │
      ▼
L0.clear_session(session_keys)  ← L0 session 数据清空
```

---

## 35. 错误处理策略

### 重试策略

| 错误类型 | 重试次数 | 退避策略 | 说明 |
|---------|---------|---------|------|
| SQLite 写入失败（磁盘满） | 0 | — | 立即返回错误 |
| SQLite 写入失败（锁冲突） | 3 | 指数退避 10ms | 内部锁释放后重试 |
| Gemini API 超时 | 3 | 指数退避 500ms | 含 circuit breaker |
| L2 向量持久化失败 | 2 | 线性退避 100ms | 降级为纯内存 |
| L3 .md 写入失败 | 0 | — | 归档失败不影响主流程 |

### Dead Letter Queue

消费者无法处理的事件进入 DLQ：

```rust
impl L2 {
    fn handle_dead_letter(&self, event: MemoryEvent, error: &str) {
        let dlq_entry = serde_json::json!({
            "event": event,
            "error": error,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "seq": event.seq(),
        });

        // 写入本地 DLQ 文件（不影响主流程）
        let dlq_path = format!("/tmp/memory_v2_l2_dlq/{}.json", event.seq());
        if let Ok(json) = serde_json::to_string(&dlq_entry) {
            let _ = std::fs::write(&dlq_path, json);
            tracing::error!("事件进入 DLQ: seq={}, path={}", event.seq(), dlq_path);
        }
    }
}
```

**DLQ 监控**：每 24h 检查 DLQ 目录，大于 100 条则告警（邮件/飞书通知）。

---

## 36. 系统容量硬上限

### 触发上限时的行为

| 层级 | 触发条件 | 行为 | 恢复 |
|------|---------|------|------|
| L0 | entries > 10_000 | 淘汰 LRU | 自动恢复 |
| L1 | entries > 50_000 | 触发 GC（淘汰 Low + 最老） | 自动恢复 |
| L1 | 磁盘空间 < 100MB | 拒绝写入（返回 507） | 清理磁盘 |
| L2 | entries > 100_000 | 停止接收新 write，返回错误 | 管理员清理 |
| L2 | 向量持久化失败 | 降级为纯内存，不拒绝写入 | 自动恢复 |
| L3 | 单文件 > 10MB | 拆分为多个文件 | 无需恢复 |
| API | 请求体 > 1MB | 返回 413 Payload Too Large | 减小请求 |

### 容量告警阈值

```rust
impl MemorySystem {
    fn check_capacity_alerts(&self) {
        let stats = self.stats();

        if stats.l1_entries > 45_000 {  // 90% of 50_000
            notify("L1 容量告警：{} / 50_000".format(stats.l1_entries));
        }
        if stats.l2_pending > 5_000 {
            notify("L2 向量积压告警：{} pending".format(stats.l2_pending));
        }
        if let Ok(df) = df("/tmp") {
            if df.available < 100 * 1024 * 1024 {
                notify("磁盘空间告警：剩余 {} MB".format(df.available / 1024 / 1024));
            }
        }
    }
}
```

---

## 37. 向量 Embedding 后端降级链

### 为什么需要降级链

Gemini API 可能因网络、限速、Key 过期等原因不可用。必须有不依赖外部 API 的兜底方案。

### 降级链设计

```
召回请求
    │
    ▼
L2.embed_backend = Gemini ?
    │
  Yes│                              No
    ▼                                ▼
 Gemini API 调用              embed_backend = OnnxLocal ?
    │                                   │
  成功│返回向量                          Yes│
    ▼                                  ▼
 纯量召回                           OnnxLocal 模型推理
    │                                      │
  失败│                                      失败│
    ▼                                        ▼
  circuit breaker 开启？              embed_backend = None
    │                                         │
  是│(连续5次失败)                            ▼
    ▼                                   返回纯 BM25
 circuit breaker 开启
 (60s 内降级为纯 BM25)
```

### Gemini API 健康检查

每 5 分钟发一个 test embedding，测量延迟和可用性：

```rust
impl L2 {
    async fn health_check_gemini(&self) {
        let start = Instant::now();
        match self.get_gemini_embedding("health check test", &self.gemini_key).await {
            Ok(vec) if vec.is_empty() == false => {
                let latency = start.elapsed();
                if latency > 2_000 {
                    tracing::warn!("Gemini 健康检查延迟过高: {}ms", latency.as_millis());
                }
                self.gemini_healthy.store(true, Ordering::SeqCst);
            }
            _ => {
                tracing::warn!("Gemini 健康检查失败");
                self.gemini_healthy.store(false, Ordering::SeqCst);
                self.open_circuit_breaker();
            }
        }
    }
}
```

---

## 38. 版本管理与 Schema 升级

### 数据库 Schema 版本

```sql
CREATE TABLE schema_version (
    version   INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL,
    note      TEXT
);

-- 初始版本
INSERT INTO schema_version VALUES (1, unixepoch(), 'initial');
```

### Schema 迁移脚本

```rust
fn migrate_schema(db: &Connection, from: u32, to: u32) -> Result<()> {
    match (from, to) {
        (1, 2) => {
            // v1→v2：添加 seq 字段
            db.execute("ALTER TABLE entries ADD COLUMN seq INTEGER NOT NULL DEFAULT 0", [])?;
            db.execute("UPDATE schema_version SET version=2, applied_at=unixepoch() WHERE version=1", [])?;
        }
        (2, 3) => {
            // v2→v3：FTS5 迁移（如有需要）
            db.execute("INSERT INTO entries_fts(entries_fts) VALUES('rebuild')", [])?;
        }
        _ => anyhow::bail!("不支持的迁移路径: {} → {}", from, to),
    }
    Ok(())
}
```

### 升级检查流程

```bash
# 启动时检查 schema 版本
$ cargo run -- serve

[memory-system] 检测到 schema 版本 1，正在升级到版本 2...
[memory-system] 升级完成，schema_version = 2
[memory-system] 启动正常
```

### Rollback 策略

**不支持在线回滚**。Schema 升级是单向的。

- 升级前自动备份（cp 原 db 为 `.db.v1.backup`）
- 升级后启动失败 → 停止服务，人工回滚到备份文件
- 降级版本号无效（不提供从 v2 降回 v1 的自动工具）

---

## 39. 安全加固

### 潜在攻击面

| 攻击面 | 风险 | 防护 |
|-------|------|------|
| API Key 暴力破解 | 高 | 5次失败后 IP 封禁 15 分钟 |
| SQL 注入（key 参数） | 高 | 所有 key 参数作 `'` 转义 |
| YAML/Prompt 注入 | 中 | value 中含 `\x00` 过滤 |
| 内存耗尽（大 entry） | 高 | 1MB 写入上限硬限制 |
| 磁盘耗尽 | 高 | 启动前检查 100MB 可用空间，不足则拒绝启动 |
| 向量 API Key 泄露 | 高 | `GEMINI_EMBEDDINGS_TOKEN` 不写入日志 |
| 私有数据写入 Public 层 | 低 | API 层加提示，但不禁用（信任用户） |

### Key 注入防护

```rust
impl L1 {
    fn sanitize_key(key: &str) -> String {
        // 去除 null byte 和控制字符
        key.replace('\0', "")
           .chars()
           .filter(|c| !c.is_control())
           .collect()
    }

    fn validate_key(key: &str) -> Result<(), MemoryError> {
        if key.len() > 512 {
            return Err(MemoryError::InvalidKey("key 超过 512 字节".into()));
        }
        if key.contains('\0') {
            return Err(MemoryError::InvalidKey("key 包含 null byte".into()));
        }
        Ok(())
    }
}
```

---

## 40. 文档与变更追踪

### ADR（Architecture Decision Records）

每个重大设计变更以 ADR 形式记录，保存在 `docs/adr/` 目录：

```
docs/adr/
  001-use-sqlite-over-sled.md
  002-event-bus-over-polling.md
  003-four-layer-over-three-layer.md
  004-vector-persistence-strategy.md
  ...
```

### ADR 模板

```markdown
# ADR-001: 使用 SQLite 而非 sled 作为 L1 存储

## 状态：已接受

## 背景
v1 使用 sled 作为 L1 存储，但存在 WAL 外挂、mmap flush 时机不明确等问题。

## 决策
使用 SQLite WAL 模式替代 sled。

## 后果
- **正面**：崩溃恢复可靠，fsync 精确控制，社区成熟
- **负面**：引入 SQLite 外部依赖（非纯 Rust）

## 替代方案
- RocksDB：复杂度更高
- redb：API 更简洁但生态较小
```

### CHANGELOG 规范

```
## [0.2.0] - 2026-04-01

### 新增
- L2 向量持久化（JSON dump，每小时增量）
- FTS5 全文搜索支持
- /metrics 端点（Prometheus 格式）

### 修复
- R4: 向量索引单点问题（持久化后已解决）
- R5: NaN/Inf 向量污染（is_finite 校验）

### 变更
- L1 GC 触发阈值：50_000 → 45_000（90% 告警）
- BM25 weight: 0.6 → 0.5（平衡调优）

### 废弃
- `embed_all()` 方法（已由后台同步替代）
```

---

## 41. 验收标准（上线 checklist）

上线前必须通过的检查项：

### 功能验收

- [ ] `write()` 后 `recall()` 能找到（所有层）
- [ ] L1 崩溃后重启，数据不丢失（模拟 SIGKILL）
- [ ] L2 崩溃后重启，向量从持久化恢复
- [ ] `delete()` 后 `recall()` 不返回已删除 entry
- [ ] `layer=private` 的 entry 其他 agent 无法通过 `/recall` 搜到
- [ ] 语义召回（semantic=true）在 Gemini 可用时返回向量结果
- [ ] Gemini 不可用时自动降级为纯 BM25

### 性能验收

- [ ] L1 write P99 < 50ms（100 并发写入）
- [ ] L2 BM25 recall P99 < 100ms（1万条数据）
- [ ] 内存占用不超过 2.5GB（10万条数据规模）

### 安全验收

- [ ] `value` 超过 1MB 时返回 400
- [ ] `key` 含 `\0` 时返回 400
- [ ] 无 API Key 时访问返回 401（除非环境变量未配置）

### 运维验收

- [ ] `/health` 端点正常返回 OK
- [ ] `/metrics` 返回 Prometheus 格式指标
- [ ] 磁盘空间不足时拒绝写入并返回 507
- [ ] Graceful shutdown 30s 内完成，所有数据落盘

