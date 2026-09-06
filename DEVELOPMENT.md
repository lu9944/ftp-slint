# ftp-slint 开发文档

> 目标：基于 Rust + Slint 打造一个类似 FileZilla Client 的桌面 FTP 管理工具。

## 1. 项目现状

| 项 | 说明 |
|---|---|
| 技术栈 | Rust (edition 2024) + Slint 1.x |
| FTP 库 | `suppaftp 11`（已在 dev-dependencies 中） |
| 已有验证 | `tests/ftp_check.rs` 已通过真实服务器验证：连接 → 登录 → LIST；服务器在 NAT 后，PASV 返回私网 IP，需使用 **EPSV（ExtendedPassive）** 模式 |
| 现有代码 | `src/main.rs` 仅为 Slint Hello World；`src/ui.slint` 为空窗口 |
| 凭据 | `.env` 中有 `FTP_SERVER_URL / FTP_PORT / FTP_USER / FTP_PWD`，用于集成测试 |

## 2. 产品目标

对标 FileZilla Client 的核心使用体验：

- **快速连接**：顶栏输入主机/端口/用户名/密码，一键连接
- **站点管理器**：保存/分组/管理多个站点，记住最近目录
- **双面板浏览**：左侧本地文件系统，右侧远程 FTP 目录，均支持树形导航、排序、刷新、进/退目录
- **文件传输**：上传/下载，进度显示、速度显示、队列管理、断点续传、冲突处理
- **文件操作**：远程/本地的新建目录、删除、重命名、多选批量操作、CHMOD
- **消息日志**：FTP 命令与响应的实时日志面板
- **传输队列**：暂停/恢复/重试/取消，持久化，限速

**明确不做（第一版）**：SFTP/SCP（协议不同）、FTP 代理链、远程编辑（可后期加）。

## 3. 技术选型

| 领域 | 选型 | 理由 |
|---|---|---|
| GUI | `slint 1`（已有） | 声明式 UI，Rust 原生，双面板列表性能足够 |
| FTP | `suppaftp 11` | 纯 Rust，支持 FTP/FTPS(EPSV/PASV/REST)，现成可用 |
| FTPS/TLS | `suppaftp` + `rustls` feature | 后期开启，不影响前期架构 |
| 异步/并发 | **每连接一个 worker 线程**（同步 suppaftp）+ `std::sync::mpsc` / `slint::invoke_from_event_loop` | FTP 控制连接本就是单线程会话；避免过早引入 async 复杂度。若后期需要多任务高并发再评估 suppaftp 的 async feature（tokio） |
| 持久化 | `serde` + `serde_json`，站点文件存于 `directories` 的 config 目录 | 简单可靠；后期可平滑迁移 SQLite |
| 密码存储 | `keyring`（OS 凭据管理器），失败时降级为"仅本次会话记住" | 安全第一，不落地明文密码 |
| 错误处理 | `thiserror` + `anyhow` | 库错误 vs 应用错误分层 |
| 日志 | `tracing` + `tracing-subscriber`（滚动的文件 + 控制台） | 命令/响应日志与 UI 日志面板共用一套事件 |
| 目录枚举 | `std::fs` + 自写磁盘枚举（Windows 盘符 / Unix 挂载点） | 无需重依赖 |

## 4. 总体架构

### 4.1 线程模型

```
┌───────────────────────────── UI 线程 (Slint 事件循环) ─────────────────────────────┐
│  MainWindow 状态: 远程列表 / 本地列表 / 队列模型 / 日志缓冲 / 连接状态             │
│  通过 invoke_from_event_loop 接收 worker 事件并更新 UI 模型                        │
└───────────────▲───────────────────────────────────────────┬──────────────────────┘
                │ Event (FtpEvent)                          │ Command (FtpCommand)
┌───────────────┴───────────────────────────────────────────▼──────────────────────┐
│  FTP Worker 线程（每个站点连接一个）                                               │
│  - 持有 suppaftp::FtpStream（控制连接）                                            │
│  - 循环: 收 Command → 执行 → 回发 Event(list_ok / progress / log / error ...)      │
│  - 传输子任务: 为每个大文件开单独数据传输（顺序执行即可，队列控制并发数）           │
└──────────────────────────────────────────────────────────────────────────────────┘
```

要点：
- FTP 控制连接**不允许跨线程并发使用**，所有远程操作串行化到 worker。
- UI 永远不阻塞：所有 FTP 调用都在 worker；UI 只发 Command、收 Event。
- 进度事件节流（如每 100ms 或每 64KB 汇报一次），避免刷爆事件循环。

### 4.2 目录结构规划

```
src/
├── main.rs              # 入口：日志初始化 → AppState 构建 → 启动 UI
├── app.rs               # AppState：UI 桥接层（Command 发送、Event 分发、模型更新）
├── ftp/
│   ├── mod.rs
│   ├── client.rs        # FtpSession：封装 suppaftp（connect/login/list/download/upload/...）
│   ├── parser.rs        # LIST 行解析（UNIX / Windows-IIS / MLSD）→ Vec<FileEntry>
│   └── types.rs         # FileEntry、ConnectConfig 等
├── core/
│   ├── mod.rs
│   ├── queue.rs         # 传输队列：任务状态机、并发控制、重试、持久化
│   └── local.rs         # 本地文件系统枚举、盘符/挂载点、回收站删除
├── store/
│   ├── mod.rs
│   ├── sites.rs         # 站点管理（JSON 持久化 + keyring 密码）
│   └── settings.rs      # 全局设置（并发数、限速、编码、主题）
└── ui/
    ├── mod.rs
    ├── theme.slint          # 颜色/字号/间距
    ├── main_window.slint    # 整体布局
    └── components/
        ├── toolbar.slint        # 快速连接栏 + 工具按钮
        ├── log_panel.slint      # 消息日志
        ├── file_panel.slint     # 可复用文件面板（本地/远程共用）
        ├── site_manager.slint   # 站点管理器对话框
        └── queue_panel.slint    # 传输队列
```

### 4.3 核心数据模型（Rust 侧）

```rust
pub struct FileEntry {
    pub name: String,        // 显示名
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>, // unix 时间戳
    pub perms: Option<String>, // "rw-r--r--"
}

pub struct ConnectConfig {
    pub host: String,
    pub port: u16,           // 默认 21
    pub user: String,
    pub password: String,    // 快速连接明文；站点走 keyring
    pub mode: PassiveMode,   // Auto / Passive / ExtendedPassive（EPSV）
    pub tls: TlsMode,        // Plain / ExplicitFtps / ImplicitFtps
    pub encoding: Encoding,  // Utf8 / Gbk（国内老服务器）
}

pub enum FtpCommand {
    Connect(ConnectConfig),
    List { path: String },
    Navigate { path: String },
    Download { remote: String, local: PathBuf, size: u64, task_id: u64 },
    Upload   { local: PathBuf, remote: String, task_id: u64 },
    Mkdir { path: String },
    Delete { paths: Vec<String> },
    Rename { from: String, to: String },
    Chmod { path: String, mode: String },
    AbortCurrent,          // 通过控制连接 ABOR 或直接断开
    Quit,
}

pub enum FtpEvent {
    Connected,
    Listing { path: String, entries: Vec<FileEntry> },
    LogLine { dir: LogDir, text: String },      // 喂给日志面板
    TaskProgress  { task_id: u64, done: u64, total: u64 },
    TaskFinished  { task_id: u64, outcome: Outcome },
    Disconnected { reason: String },
    Error { context: String, message: String },
}
```

### 4.4 传输任务状态机

```
Pending → Running ⇄ Paused → Done
             │                 
             ├→ Failed ──(retry)──→ Pending
             └→ Canceled
```

- 队列按顺序执行，全局并发数可配（默认 2~3）。
- 每个 Task 记录 `{local, remote, direction, size, done, status, retries}`。
- 冲突策略（目标已存在）：询问 / 覆盖 / 跳过 / 续传 / 重命名 —— 默认弹确认框，记住本次选择。

## 5. UI 布局设计

```
┌──────────────────────────────────────────────────────────────────────┐
│ 菜单栏:  [文件] [站点] [传输] [查看] [帮助]                            │
├──────────────────────────────────────────────────────────────────────┤
│ 工具栏:  主机[____] 端口[__] 用户[____] 密码[____] [快速连接] [站点管理]│
├──────────────────────────────┬───────────────────────────────────────┤
│ ◎ 本地面板                    │ ◎ 远程面板                             │
│ [C:\ > users > me ▾]  [↻][↑] │ [/ > www ▾]                    [↻][↑] │
│ ┌─ 名称─┬─大小──┬─修改时间──┐ │ ┌─ 名称─┬─大小──┬─修改时间──┬权限───┐ │
│ │ ...   │       │           │ │ │ ...   │       │          │       │ │
│ └───────┴───────┴───────────┘ │ └───────┴───────┴──────────┴───────┘ │
├──────────────────────────────────────────────────────────────────────┤
│ 消息日志:  220 Welcome ... / USER xx / 230 Login successful ...       │
├──────────────────────────────────────────────────────────────────────┤
│ 队列:  文件 │ 方向 │ 大小 │ 进度▓▓▓░░ 45% │ 速度 2.1MB/s │ 状态      │
│        [开始] [暂停] [取消] [清已完成]         并发: 2   限速: 关     │
└──────────────────────────────────────────────────────────────────────┘
```

交互约定：
- 双击目录进入；双击文件=下载到对侧当前目录；右键上下文菜单（上传/下载/删除/重命名/新建目录/属性）。
- 两个面板结构共用 `FilePanel` 组件，仅数据源不同。
- 状态栏显示：连接状态 / 当前站点 / 队列摘要。

## 6. 里程碑规划

每个里程碑以"可运行 + 可验证"收口。

### M0 项目骨架（1 周）
- 搭建 `app/ftp/core/store/ui` 模块骨架与 `FtpCommand/FtpEvent` 通道
- worker 线程 + `invoke_from_event_loop` 事件回流跑通（ping/pong 验证）
- tracing 日志接入；UI 布局骨架（五区占位）
- **验收**：启动后可见完整布局；日志文件输出正常

### M1 连接与远程浏览（1 周）
- `FtpSession`：connect/login/pwd/list/cwd/cdup/quit，Auto 模式下先 EPSV 失败再退 PASV
- MLSD 优先、LIST 兜底；实现 UNIX 与 Windows 两种 LIST 格式解析（纯函数，单测覆盖）
- 快速连接栏 + 远程面板渲染 + 双击进入/返回上级/刷新/路径栏
- **验收**：用 `.env` 中真实服务器成功浏览目录；`cargo test` 覆盖 parser

### M2 本地面板 + 基础传输（1~2 周）
- 本地目录枚举（含盘符/挂载点）、本地树与列表
- 下载/上传（单文件）：进度、速度、取消（ABOR/断开兜底）
- 双击文件=传输到对侧当前目录；右键菜单（传输/删除/重命名/新建目录）
- **验收**：真实服务器上传下载 1KB~1GB 文件，进度正确，可中途取消

### M3 传输队列（1 周）
- 队列模型 + 状态机 + 并发控制 + 失败重试（指数退避）
- 断点续传：REST + 本地 `.part` 临时文件；`SIZE`/`MDTM` 校验
- 冲突对话框（覆盖/跳过/续传/重命名，可"本次全部应用"）
- 队列持久化到磁盘，重启恢复 Pending 任务
- **验收**：拔网线中断 → 恢复网络 → 续传成功；重启应用队列仍在

### M4 站点管理器（1 周）
- 站点 CRUD、分组、排序、协议/编码/被动模式配置
- 密码入 keyring；不勾"记住密码"则每次询问
- 记住每个站点的最后浏览目录，连接后自动进入
- **验收**：保存 3 个站点，重启后一键连接

### M5 体验完善（1~2 周）
- 多选（Ctrl/Shift）、全选、拖拽传输（面板间、面板↔系统资源管理器）
- CHMOD、删除确认、批量删除队列化
- 目录比较高亮（本地/远程同名但更新）、目录同步浏览（可选开关）
- 文件名过滤器（隐藏 `.*`、按扩展名）
- **验收**：完成 FileZilla 日常操作对照清单（见 §8）

### M6 FTPS 与健壮性（1 周）
- FTPS 显式/隐式（rustls feature）、证书不校验选项（默认校验）
- 断线自动重连 + 会话恢复到原目录；编码 GBK 支持
- 传输限速（令牌桶）
- **验收**：FTPS 服务器连通收发；弱网重连场景演练

### M7 发布打磨（1 周）
- 图标/主题（亮暗）、中文 i18n、设置页（并发/限速/默认下载目录/日志级别）
- 打包：Windows NSIS/MSI + Linux AppImage，GitHub Release
- **验收**：全新机器安装即用

## 7. 关键技术点与风险

| # | 风险/要点 | 对策 |
|---|---|---|
| 1 | **服务器在 NAT 后，PASV 返回私网 IP**（已在 `tests/ftp_check.rs` 注释确认） | 默认 Auto：先 EPSV，失败退 PASV；设置里可强制模式 |
| 2 | UI 卡死 | 严禁在 UI 线程做任何网络/磁盘 IO；全部走 worker |
| 3 | LIST 格式千奇百怪（UNIX/IIS/罕见格式、无权限列） | 解析器做成纯函数 + 失败行原样保留在日志；用 `MLSD` 优先规避 |
| 4 | 中文/GBK 编码目录名乱码 | `MDTM`/`MLSD` 拿元数据；LIST 兜底时支持 UTF-8 与 GBK 切换（bytes → String 手动解码，引入 `encoding_rs`） |
| 5 | 取消传输不生效（老服务器不支持 ABOR） | 超时后直接 drop 控制连接并重连，状态标记 Canceled |
| 6 | 断点续传服务器不支持 REST | 探测 `REST 1` 支持性；不支持则该任务整传 |
| 7 | 大目录（万级条目）渲染卡顿 | Rust 侧分页/虚拟化：Slint 用 `ListView` + 只喂可见范围（先全量喂，卡则优化） |
| 8 | keyring 在部分 Linux 环境不可用 | 降级策略：会话内记忆 + 明确提示"密码未保存" |

## 8. 测试策略

- **单元测试**：LIST/MLSD 解析（各平台样本行 ≥ 15 条）、队列状态机、路径拼接（`/a/b` + `c`、Windows 反斜杠等）、限速器
- **集成测试**（沿用 `.env` 真实服务器，标记 `#[ignore]` 按需跑）：
  - 登录/EPSV/LIST（现有 `tests/ftp_check.rs` 扩展）
  - 上传→下载→比对哈希→删除
  - 断点续传：上传一半断开再续
- **手动回归清单**（对照 FileZilla 日常操作）：连接/断开、进退目录、刷新、上传、下载、覆盖提示、删除、重命名、队列暂停恢复、站点切换

## 9. 依赖清单（分阶段引入）

```toml
# M0
serde = { version = "1", features = ["derive"] }
serde_json = "1"
directories = "5"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# M1
# （suppaftp 从 dev-dependencies 移入 dependencies）

# M3
encoding_rs = "0.8"        # GBK 解码（如需要）

# M4
keyring = "3"

# M6
# suppaftp 开启 rustls / FTPS 相关 feature
```

## 10. 近期任务清单（从 M0 开始）

1. [ ] 创建模块骨架与 `FtpCommand/FtpEvent` 类型
2. [ ] 实现 worker 线程循环 + 事件回流 demo（UI 上点击"测试"能收到日志行）
3. [ ] 把 `tests/ftp_check.rs` 的连接逻辑迁入 `ftp/client.rs`，`.env` 读取逻辑抽成公共模块
4. [ ] UI 五区布局骨架 + 空面板组件化（`FilePanel`）
5. [ ] M1：EPSV→PASV 降级连接 + 远程目录浏览可用
