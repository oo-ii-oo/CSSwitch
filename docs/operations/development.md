# 开发与维护

本文说明当前源码树的开发入口。安全、Git / worktree 和证据措辞分别以 [`.agents/rules/`](../../.agents/rules/) 为准。

## 环境

- macOS Apple Silicon（当前公开桌面发布目标），或 Intel Mac（本源码提供构建入口，尚未建立 Intel 真机验收证据）；
- Node.js / npm（Tauri 前端与构建）；
- Rust / Cargo（desktop backend 与 Rust gateway）；
- Python 3（测试驱动与 mock 使用，**不是** CSSwitch runtime proxy 依赖）；
- Claude Science App（只在隔离 runtime / 真机验收时需要）。

## 本地启动

```bash
cd desktop
npm install
npm run tauri dev
```

## Intel Mac 构建与启动

Intel 构建使用原有 UI、Rust Desktop、Gateway、Provider、Codex 与 Skill 实现，
只改变目标架构和打包入口。CSSwitch 不包含 Claude Science 本体，需先安装可在
当前 Intel Mac 上运行的 Claude Science。Science 的上游系统要求与下载版本也必须满足。

准备 macOS 13 或更高版本、Xcode Command Line Tools、Node.js 22 / npm、Rust stable
（含 Cargo 和 rustup）。缺少工具时使用官方入口：
[Node.js](https://nodejs.org/en/download)、[Rust](https://rustup.rs/)；Apple 命令行工具通过
`xcode-select --install` 安装。构建需要网络下载依赖，不需要真实 API Key 或 Apple Developer 账号。

在源码根目录双击 `Build-Intel.command`，或在终端执行：

```bash
bash scripts/build-macos-intel.sh
```

也可以从 `desktop` 执行 `npm run build:intel`。脚本自动安装锁定的 npm 依赖、添加
`x86_64-apple-darwin` Rust target，并将构建缓存放到 `desktop/src-tauri/target/intel`。
Desktop 与 Gateway 都从当前源码构建，嵌套 Gateway 构建也使用自己的 lockfile。
开发模式优先使用本次构建 staged 的 Gateway；带 target 后缀的候选只接受与
Desktop 编译目标完全一致的文件，保留无后缀打包及独立 Gateway 回退。
Intel 配置只覆盖 macOS 最低系统版本（13.0）与 ad-hoc 签名，不改变产品名、
bundle ID、配置格式或功能开关。原有 Apple Silicon 构建入口保持可用。

成功输出为 `dist/intel/CSSwitch_<version>_x64.dmg`、对应的 `.sha256` 与
`build-verification.json`。脚本先检查 `.app` 中主程序和 Gateway 均为 `x86_64`、
版本与资源匹配，然后从空 staging 目录生成 DMG、只读挂载并复核一个正式 app 和
指向 `/Applications` 的链接。只有复核成功的 DMG 才替换同名输出。
ad-hoc 签名完整性不等于 Developer ID、公证或 Gatekeeper 放行；首次启动按 macOS
「隐私与安全性」提供的允许打开流程操作。

仅开发启动可执行：

```bash
cd desktop
npm ci
rustup target add x86_64-apple-darwin
npm run dev:intel
```

Intel 打包可在 Intel Mac 上原生构建，也可在 Apple Silicon Mac 上交叉构建；
交叉构建结果仍需在 Intel Mac 验证。Linux / Windows 不能通过该脚本生成 macOS 安装包。
安装后在 CSSwitch 新增 DeepSeek 配置、设为当前，再点击「一键开始」。真实 Science 启动、
模型推理、工具调用、Skill 和 Codex 等端到端行为必须在 Intel Mac 上另行验证；构建成功
不证明所有运行时能力已经验收。

## 快速 WIP、候选与 source closure

先按[代码作者规则](../../.agents/rules/code-authoring.md)确认 owner、边界和最高风险：`F3 > F2 > F1 > F0`。同一有界 WIP 先完成，再运行完整 diff 和有明确信号的 owning check；`F0` / `F1` 默认不启动正式独立审查或完整 gate。稳定的 `F2` / `F3` candidate 先冻结边界，再按[独立审查规则](../../.agents/rules/reviewing.md)集中审查；文件、进程或 network 副作用的目标、授权、时序、提交点、timeout、cleanup、补偿或失败报告变化一律按 `F3` 处理。

candidate 不自动等于 source closure。只有明确需要声明 exact candidate 的 source closure、修改 quality kernel / gate authority，或影响无法由聚焦检查可信界定时，才在 clean exact-`HEAD` 候选上运行唯一完整 gate：

```bash
GATE_ROOT="$(mktemp -d /private/tmp/csg.XXXXXX)"
chmod 700 "$GATE_ROOT"
bash test/run_all.sh --output-root "$GATE_ROOT"
```

固定 16-suite 选择、输出目录约束和判定边界见[测试文档](testing.md)。无参数调用和
旧 `--require-release-ready` 已不再是有效入口。任何 WIP 或 focused check 的 PASS 都不建立
`SOURCE-GREEN`；没有运行完整 gate 时报告 `SOURCE-GREEN: NOT-RUN`。

## 组件级检查

五个 Rust manifest 都是独立入口；从仓库根选择目标，不把单个 crate 结果外推到其他 crate：

| 组件 | manifest | 聚焦命令形态 |
| --- | --- | --- |
| Skill 安装核心 | `desktop/skill-package/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/skill-package/Cargo.toml` |
| Codex network 库 | `desktop/codex-network/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/codex-network/Cargo.toml` |
| 共享 provider contract | `desktop/provider-contracts/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/provider-contracts/Cargo.toml` |
| Gateway | `desktop/gateway/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/gateway/Cargo.toml` |
| Tauri desktop | `desktop/src-tauri/Cargo.toml` | `cargo <fmt / clippy / test> --manifest-path desktop/src-tauri/Cargo.toml` |

`fmt` 使用 `cargo fmt --check --manifest-path <manifest>`；`clippy` 使用 `cargo clippy --manifest-path <manifest> --all-targets -- -D warnings`；`test` 使用 `cargo test --manifest-path <manifest>`。Python 与前端的常用聚焦入口仍为：

```bash
python3 -m unittest discover -s test -p 'test_*.py' -v
node --check desktop/src/main.js
```

组件命令和单个 `test/run-*.sh` 只适合聚焦诊断；当前完整门禁是上述
`GATE-SOURCE`，不能由组件结果拼成 `SOURCE-GREEN`。

## 远端协作与手动 Intel 构建

本源码提供 `.github/workflows/build-macos-intel.yml`，仅通过 `workflow_dispatch`
手动触发，使用 GitHub 的 `macos-15-intel` runner 与上述同一构建脚本。将本修改版放入
自己的 GitHub 仓库后，可在 Actions 中选择 **Build Intel macOS app → Run workflow**，
成功后下载 **CSSwitch-macos-intel** artifact。流程仅有 `contents: read` 权限，
不发布 Release、不读取账号凭证，也不设置 required check。

该流程尚未在远端运行；其存在不代表 CI、Intel artifact 或 runtime 验收已经通过。
runner 的当前架构标签以 [GitHub 官方文档](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
为准。source closure 仍只在明确 closure 目标时使用现有本地 exact-candidate gate；
PR 模板中的本地结果不构成远端执行证据。

## Science 相邻功能工作法

1. 在隔离环境确认上游 runtime 事实；
2. 明确 source of truth 与所有权；
3. 先验证不增加存储 / 状态机的最短路径；
4. 跑一条完整 E2E，并分别记录 copy、discover、attach、load、trigger、功能执行与重启；
5. 最后再决定是否需要 UI、catalog、cache 或新存储。

Science 已拥有的能力不应在 CSSwitch 再造一套 installer、目录所有权或生命周期。

## 隔离 runtime 开发

- 使用临时外层 `HOME`、临时持久 data-dir、动态端口和假 `security`；
- 不使用真实 `~/.claude-science`、端口 `8765` 或 `/Applications/CSSwitch.app`；
- installed-App candidate、缓存 candidate、实际 PID、版本 runtime 与 data-dir 分别取证；
- live provider、真实账号或真实 SSH server 测试需要额外授权。

详细步骤见[真机验收](real-machine-acceptance.md)。

## 文档维护

文档类型、权威位置、默认阅读预算，以及临时 Plan / Draft Spec / Handoff 的晋升、过期与删除，统一见[文档治理合同](document-lifecycle.md)。

发布或重要 upstream runtime 变化后，应复核 architecture、功能限制、known issues 和 release evidence，而不是把新事实只留在聊天或 handoff。
