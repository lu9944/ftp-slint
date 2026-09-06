# ftp-slint 开发文档

> 目标：基于 Rust + Slint 打造一个类似 FileZilla Client 的桌面 FTP 管理工具。

## 1. 项目现状

| 项 | 说明 |
|---|---|
| 技术栈 | Rust (edition 2024) + Slint 1.x |
| FTP 库 | `suppaftp 11`（已在 dev-dependencies 中） |
| 已有验证 | `tests/ftp_check.rs` 已通过真实服务器验证：连接 → 登录 → LIST；服务器在 NAT 后，PASV 返回私网 IP，需使用 **EPSV（ExtendedPassive）** 模式 |
| 现有代码 | **M0 + M1 已完成**（2026-09）：模块骨架（`app/ftp/core/store`）+ 双 Worker 线程模型 + 五区 UI 布局 + 远程浏览（Auto=EPSV→PASV、MLSD→LIST）+ 三格式 LIST 解析 + mock/沙盒/真实三轨测试；近期任务清单见 §10 |
| 凭据 | `.env` 有 `FTP_SERVER_URL / FTP_PORT / FTP_USER / FTP_PWD`（真实服务器，NAT 后需 EPSV）；`.env.local` 为**自部署沙盒服务器**（局域网，日常开发调试用），变量名与 `.env` 相同，**存在时优先加载**。两者均已 gitignore |

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
| 异步/并发 | **控制 / 传输 / 本地 IO 分线程**（同步 suppaftp）+ `std::sync::mpsc` / `slint::invoke_from_event_loop` | 主连接只做浏览与文件操作；每个传输任务开**独立控制连接**（FileZilla 模式），并发数可配、可随时取消；同步栈即可，不过早引入 async |
| 持久化 | `serde` + `serde_json`，站点文件存于 `directories` 的 config 目录 | 简单可靠；后期可平滑迁移 SQLite |
| 密码存储 | `keyring`（OS 凭据管理器），失败时降级为"仅本次会话记住" | 安全第一，不落地明文密码 |
| 错误处理 | `thiserror` + `anyhow` | 库错误 vs 应用错误分层 |
| 日志 | `tracing` + `tracing-subscriber`（滚动的文件 + 控制台） | 命令/响应日志与 UI 日志面板共用一套事件 |
| 目录枚举 | `std::fs` + 自写磁盘枚举（Windows 盘符 / Unix 挂载点） | 无需重依赖 |

## 4. 总体架构

### 4.1 线程模型

```
┌────────────────────────── UI 线程 (Slint 事件循环) ──────────────────────────┐
│  MainWindow 状态: 远程列表 / 本地列表 / 队列模型 / 日志缓冲 / 连接状态       │
│  通过 invoke_from_event_loop 接收事件并更新 UI 模型，自身不做任何 IO          │
└──────────▲──────────────────▲──────────────────────────────┬───────────────┘
           │ Event            │ Event (本地/任务)             │ Command
┌──────────┴──────────┐ ┌─────┴─────────────┐   ┌──────────────▼─────────────┐
│ 控制 Worker (每站点1) │ │ Local IO Worker   │   │ 队列调度器 core::queue      │
│ 持有主 FtpStream     │ │ 目录枚举/删除/改名  │   │ 按全局并发数派发:            │
│ LIST/CWD/MKD/DELE/  │ │ 等本地磁盘操作      │   │  └→ 传输 Worker ×N          │
│ RNFR/CHMOD          │ │ (后台线程)          │   │    每任务独立 FtpStream      │
└─────────────────────┘ └───────────────────┘   │    取消 = drop 该连接        │
                                                └────────────────────────────┘
```

要点：
- **主连接不承载传输**：浏览/文件操作走控制 Worker 的主 `FtpStream`；每个传输任务由队列调度器另开一条独立控制连接 + 数据连接，因此并发传输、传输中浏览互不干扰。
- **取消 = 断开该任务的独立连接**：每任务持有 `Arc<AtomicBool>` 取消标志，传输 Worker 在读写循环中检查，命中即 drop 连接、标记 Canceled。不依赖服务器支持 `ABOR`，主连接也不受影响。控制连接另设读超时兜底，防止单条命令永久卡死。
- FTP 控制连接**不允许跨线程并发使用**：主连接操作串行化到控制 Worker，传输任务各自独占自己的连接。
- UI 永远不阻塞：远程 FTP、本地磁盘 IO 全部在 worker 线程；UI 只发 Command、收 Event。
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

/// 主连接命令（浏览与文件操作；传输不占用主连接）
pub enum FtpCommand {
    Connect(ConnectConfig),
    List { path: String },
    Navigate { path: String },
    Mkdir { path: String },
    Delete { paths: Vec<String> },
    Rename { from: String, to: String },
    Chmod { path: String, mode: String },
    /// 提交给队列调度器：由调度器按并发数开独立连接执行
    EnqueueTransfers { tasks: Vec<TransferRequest> },
    /// 取消任务：经队列调度器置位该任务的 CancelHandle
    CancelTask { task_id: u64 },
    Quit,
}

/// 本地文件系统命令（独立 Local IO Worker 执行，与远程对称）
pub enum LocalFsCommand {
    List { path: PathBuf },
    Mkdir { path: PathBuf },
    Delete { paths: Vec<PathBuf> },
    Rename { from: PathBuf, to: PathBuf },
}

/// 一次传输的描述：由队列调度器为其建立独立 FtpStream
pub struct TransferRequest {
    pub task_id: u64,
    pub direction: Direction, // Upload / Download
    pub remote: String,
    pub local: PathBuf,
    pub size: u64,
}

/// 每个传输任务一个取消标志；调度器持有，UI 取消时置位
pub struct CancelHandle(Arc<AtomicBool>);

pub enum FtpEvent {
    Connected,
    Listing { path: String, entries: Vec<FileEntry> },
    LocalListing { path: PathBuf, entries: Vec<FileEntry> },
    LogLine { dir: LogDir, text: String },      // 喂给日志面板
    TaskProgress  { task_id: u64, done: u64, total: u64, speed: u64 },  // speed 由 worker 节流窗口计算（B/s）
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

- 队列按顺序调度，全局并发数可配（默认 2~3）；每个 Running 任务占用一条独立控制连接。
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
- 本地目录枚举（含盘符/挂载点）、本地树与列表；本地操作走 Local IO Worker（§4.1/§4.3）
- 下载/上传（单文件）：独立连接执行，进度、速度、取消（置位 CancelHandle → drop 该任务连接）
- 双击文件=传输到对侧当前目录；右键菜单（传输/删除/重命名/新建目录）
- **验收**：真实服务器上传下载 1KB~1GB 文件，进度正确，可中途取消且浏览不中断
- **进展（2026-09）**：**上传/下载双向链路均已落地**——`core/queue` 队列调度器（默认并发 2、独立连接、CancelHandle 取消=drop 连接、失败/取消清理残留：本地删半成品、远端 best-effort 删未完成文件）、100ms 节流进度+速度事件、队列面板（进度条/速度/状态/选中取消/清已完成）、双击远程文件=下载到本地当前目录、双击本地文件=上传到远程当前目录；已实测真实服务器 1MB 上传（9s，28 个速度事件）+ 下载（4s，5 个速度事件）逐字节一致，且修复了 TYPE I（二进制模式）问题。右键菜单待续

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
- 前置 PoC：验证 Slint 能否接收 **OS 级文件拖入**（从系统资源管理器拖入并取得路径），结论写回 §7
- 多选（Ctrl/Shift）、全选、拖拽传输（面板间；面板↔系统资源管理器视 PoC 结论，不可行则降级为仅面板间拖拽 + 双击/按钮传输）
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
| 5 | 取消传输不生效（老服务器不支持 ABOR） | 传输走独立连接：置位 `CancelHandle` 后直接 drop 该任务连接并标记 Canceled，主连接不受影响；主连接命令设读超时防卡死 |
| 6 | 断点续传服务器不支持 REST | 探测 `REST 1` 支持性；不支持则该任务整传 |
| 7 | 大目录（万级条目）渲染卡顿 | Rust 侧分页/虚拟化：Slint 用 `ListView` + 只喂可见范围（先全量喂，卡则优化） |
| 8 | keyring 在部分 Linux 环境不可用 | 降级策略：会话内记忆 + 明确提示"密码未保存" |
| 9 | Slint 对 OS 级文件拖入（外部路径）的支持未证实 | **PoC 已完成（2026-09）**：Slint 1.17 内置 `DropArea` 仅支持**应用内**拖拽（`DataTransfer` 只携带 image/plain_text/user_data），winit 后端不处理 `WindowEvent::DroppedFile`，原生 API **拿不到 OS 文件路径**；但开启 `unstable-winit-030` feature 后，可通过 `WinitWindowAccessor::on_winit_window_event` 在 winit 层拦截 `DroppedFile(PathBuf)` 取得路径（`src/main.rs` 已实现该 PoC，拖入窗口即写入日志面板）。**结论：可行，但依赖 unstable API**（锁定 winit 后端与 winit 0.30 版本）。M5 采用此方案，同时保留降级路线（面板间拖拽 + 双击/按钮传输），若 unstable API 在后续版本移除则自动降级 |

## 8. 测试策略

**双轨制**：本地 mock 服务器保证可自动化（CI 可跑），真实服务器验证真实场景（手动）。

- **单元测试**：LIST/MLSD 解析（各平台样本行 ≥ 15 条）、队列状态机、路径拼接（`/a/b` + `c`、Windows 反斜杠等）、限速器
- **集成测试 - mock 轨（可自动化）**：用 `libunftp` 在 `127.0.0.1` 随机端口起本地 FTP 服务器（dev-dependencies），覆盖：登录/浏览/上传→下载→比对/删除/重命名等常规路径，CI 直接跑
  - 已落地（`tests/mock_ftp.rs`）：libunftp 0.20.3 **不支持 EPSV 与 MLSD**，恰好真实验证了客户端 Auto 降级（EPSV→PASV）与 MLSD→LIST 兜底两条路径
- **集成测试 - 沙盒轨（日常开发默认）**：自部署沙盒服务器（`.env.local`，局域网）。日常开发调试、反复上传删除验证优先用它，不污染真实服务器。测试在检测到 `.env.local` 时才启用，否则自动跳过（CI 天然跳过）
- **集成测试 - 真实轨（`#[ignore]` 手动跑）**：`.env` 真实服务器（NAT 后、需 EPSV，mock/沙盒无法覆盖）：
  - EPSV/PASV 降级连接（现有 `tests/ftp_check.rs` 扩展）
  - 大文件 1KB~1GB 传输与进度
  - 断点续传：上传一半断开再续（mock 模拟不了真实弱网）
- **手动回归清单**（对照 FileZilla 日常操作）：连接/断开、进退目录、刷新、上传、下载、覆盖提示、删除、重命名、队列暂停恢复、站点切换

## 9. 依赖清单（分阶段引入）

```toml
# M0
serde = { version = "1", features = ["derive"] }
serde_json = "1"
directories = "5"
dotenvy = "0.15"           # .env 读取（集成测试与开发环境）
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

# dev-dependencies（集成测试 mock 轨）
libunftp = "0.20"          # 本地 mock FTP 服务器
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }  # libunftp 运行时
```

## 10. 近期任务清单（从 M0 开始）

> 以下 7 项已于 2026-09-06 全部完成（M0 + M1 收口，双 PoC 落地）。

1. [x] 创建模块骨架与 `FtpCommand/FtpEvent/LocalFsCommand` 类型（含 `TransferRequest`/`CancelHandle`）
   - `src/{lib,app}.rs`、`src/ftp/{types,client,parser}.rs`、`src/core/{queue,local}.rs`、`src/store/env.rs`、`src/ui/*.slint`；lib + bin 双 target，集成测试直接复用库代码
2. [x] 实现控制 Worker + Local IO Worker + 事件回流 demo（UI 上点击"测试"能收到日志行）
   - `app.rs`：`std::sync::mpsc` 命令通道 + `slint::invoke_from_event_loop` 事件回流；工具栏"测试"按钮 → `FtpCommand::Ping` → 控制线程回 `LogLine("pong…")` 显示到日志面板
3. [x] 把 `tests/ftp_check.rs` 的连接逻辑迁入 `ftp/client.rs`，`.env` 读取逻辑抽成公共模块（dotenvy：先读 `.env`，再用 `.env.local` 覆盖，沙盒优先）
   - `store/env.rs`：`load()`/`ftp_env()`/`ftp_env_from_file()`；`FtpSession::connect` 含 TCP 连接超时（15s）与控制连接读写超时（30s，`set_read_timeout`）
4. [x] UI 五区布局骨架 + 空面板组件化（`FilePanel`）
   - 菜单栏占位 / `Toolbar`（快速连接）/ 本地+远程 `FilePanel`（路径栏、表头、ListView）/ `LogPanel` / `QueuePanel` 占位 + 状态栏；`theme.slint` 暗色主题
5. [x] M1：EPSV→PASV 降级连接 + 远程目录浏览可用
   - Auto 模式**连接时探测**：先 EPSV 发一次 LIST，失败即切 PASV（连接期一次性协商，后续传输/浏览共用）；MLSD 优先、LIST 兜底；UNIX/Windows-IIS/MLSD 三格式解析为纯函数，30 个单测覆盖（含中文/空格文件名、符号链接、无年份时间回推、分号文件名等边界）；UI 支持双击进目录、↑ 返回、↻ 刷新、路径栏回车跳转；真实服务器（`.env`，NAT 后）与沙盒（`.env.local`）均实测通过
6. [x] PoC：libunftp 本地 mock 服务器跑通自动化集成测试（§8 mock 轨基础设施）
   - `tests/mock_ftp.rs` 4 个用例：连接+列表+降级断言（`mode()==Passive`）、上传→下载逐字节比对、MKD/RNFR-RNTO/DELE/RMD、子目录 CWD/CDUP；libunftp 无 EPSV/MLSD，反向验证了降级路径
7. [x] PoC：Slint 接收 OS 级文件拖入，结论写回 §7 风险 9（M5 前置，最迟 M5 开工前完成）
   - 结论：内置 `DropArea` 仅应用内拖拽；需 `unstable-winit-030` 的 `on_winit_window_event` 拦截 `DroppedFile` 才能拿到 OS 路径（已在 `main.rs` 实现，拖入即记日志）。详见 §7 风险 9

**测试现状**（`cargo test`）：单元 30 通过；mock 轨 4 通过；沙盒轨 1 通过（有 `.env.local` 才跑）；真实轨 1 个 `#[ignore]`（`cargo test --test ftp_check -- --ignored --nocapture` 手动跑，断言 EPSV 协商）；`cargo clippy --all-targets` 零警告。
