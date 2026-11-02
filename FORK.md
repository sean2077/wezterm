# 自用 fork 说明

这是 [wezterm/wezterm](https://github.com/wezterm/wezterm) 的自用 fork，只在 Windows 上构建、自己使用。

## 分支

| 分支 | 用途 |
|---|---|
| `main` | 上游 `main` 的纯镜像，只做快进，不放任何自己的提交 |
| `patched` | 自用分支 = `main` + 下面的补丁，每个补丁一个独立提交 |

本地 remote：`origin` 是本 fork，`upstream` 是官方仓库（push URL 设成了 `DISABLED`，防止误推）。

## 补丁清单

| 提交主题 | 来源 | 上游状态 | 何时可以删 |
|---|---|---|---|
| windows: resolve the published gui socket name against the runtime dir | cherry-pick 自 [#8181](https://github.com/wezterm/wezterm/pull/8181)（作者 Yoshiyuki Kumon） | 未合并；同类 PR 还有 #7896、#8201 | 上游合并任一修复 [#4456](https://github.com/wezterm/wezterm/issues/4456) 的 PR 之后 |
| windows: only notify the IME when the cursor rect actually changes<br>windows: reset the cached IME cursor rect on focus/context/composition start | cherry-pick 自 [#8166](https://github.com/wezterm/wezterm/pull/8166)（作者 Yoshiyuki Kumon） | 未合并 | 上游合并 #8166 之后 |
| gui: prune stale gui-sock files when the GUI starts | 自用 | — | 上游在 GUI 启动路径也清理死 socket 后可删 |
| windows: add a config-driven quake dropdown mode | 自用 | — | 上游提供等价的全局热键/quake 模式后可删 |
| windows: toggle quake bounds from the native fullscreen action | 自用 | — | 上游提供等价的 quake 模式后可删 |
| gui: add an opt-in uniform fancy tab bar width | 自用 | — | 上游提供等价的标签栏布局选项后可删 |
| cli: report tab/window indices and focus the owning GUI window | 自用 | — | 上游的 `list` 输出索引且 `activate-*` 会前台化窗口后可删 |
| gui: expose tab budgets, mergeable overrides, CWD links and status coalescing | 自用 | — | 上游提供等价 API 后可删 |

#4456 的影响：Windows 上 GUI 只把 `gui-sock-<pid>` 这个文件名发布到共享内存，新启动的 `wezterm-gui` 和 `wezterm cli` 按自己的工作目录去解析，于是任务栏、开始菜单、右键菜单每次启动都会新开一个进程（每个进程有几百 MB 的基础开销），在 WezTerm 外运行 `wezterm cli` 也会连不上。

自用清理补丁：GUI 正常退出会在 `listener.run()` 返回后删除自己的 `gui-sock-<pid>`（`wezterm-gui/src/main.rs`），但崩溃、强杀和隔离测试实例都不走这条路径；这些标记文件放在 `~/.local/share/wezterm`，重启电脑也不会清。上游只在自动更新路径调用 `discover_gui_socks()`，日常启动不调用，死文件会一直留着。补丁在 `async_run_terminal_gui` 开头调用一次 `discover_gui_socks()`；该函数只删除连接失败且超过 1 秒的 socket，正在启动的实例不受影响。`agent.<pid>`（SSH agent 符号链接）和旧命名 `sock` 不在清理范围。

#8166 的影响：Windows 上 `TermWindow::update_text_cursor` 在每帧绘制时都调用 `ImmSetCandidateWindow`（Builtin preedit）或 `ImmSetCompositionWindow`（System preedit），而 IME 每次收到调用都会销毁并重建候选窗，于是组合输入时候选窗每敲一键闪一次（Google 日语输入法、微软 IME、Rime/Weasel 都如此；Claude Code、Codex CLI 这类高频重绘的 TUI 更明显）。补丁缓存上次上报给 IME 的光标矩形，位置不变就跳过调用；在 `WM_SETFOCUS`、`WM_IME_SETCONTEXT`、`WM_IME_STARTCOMPOSITION`、`WM_IME_ENDCOMPOSITION` 以及配置变化时清缓存，保证重新获得焦点、每次组合开始时至少重新上报一次。

## Quake 下拉模式（自用）

`config.quake` 让一个专用 WezTerm 实例作为 Guake 式下拉终端：窗口在登录时隐藏启动，注册全局热键（例如 F3），按热键在“隐藏”和“工作区顶部矩形”之间切换；支持置顶、失焦收起、关闭即隐藏。实现位于 `window/src/os/windows/quake.rs`，使用 `RegisterHotKey` + `WM_HOTKEY`，窗口样式设为 `WS_EX_TOOLWINDOW`，因此不出现在任务栏或 Alt+Tab。

用法：用 `--config-file` 指定一个 `quake.lua`，并用 `--class` 给专用实例一个独立类名。`quake.lua` 应设置 `config.quake`、`window_decorations = "NONE"`（外框与客户区一致，几何才准确）、`tab_bar_at_bottom = true`。热键注册失败时窗口会回落为可见状态，而不是留下隐形实例；`WM_CLOSE` 在 quake 模式下只收起窗口，显式退出仍用 `QuitApplication`。全屏状态由 `QuakeState` 在窗口进程内维护：quake 模式下原生的 `ToggleFullScreen` 动作改为在“配置的下拉矩形”和“整个显示器”之间切换，F3 收起再呼出后保持当前状态；Lua 只需绑定 `wezterm.action.ToggleFullScreen`，不需要计算几何或写 per-window override。

`config.tab_bar_uniform_width = true` 让 fancy 标签栏的每个 tab 等宽，并把 `+` 按钮从均分分母中拿出来、按固定宽度布局；标题短于份额时补齐、超出时截断，标签栏几何不再随标题长度变化。均分预算优先使用 `terminal_size.pixel_width`，并扣除右侧状态栏和集成窗口按钮的占用。`config.tab_bar_side_reserve_ratio`（默认 0.6）控制这部分预留的比例，调参不需要重新编译。

`format-tab-title` 的第 6 个参数（`max_width`）在 fancy bar 下现在按标题字体宽度和 tab 数换算成真实预算，不再是恒定的 `tab_max_width`。`window:set_config_override(key, value)` / `clear_config_override(key)` 用于增量修改 per-window override；hyperlink URI 中的 `$CWD` 在点击时展开为当前 pane 的工作目录；`update-status` / `update-right-status` 统一按 `status_update_interval` 调度，不再随高频标题变化每帧触发。

`format-tab-title` 的返回值可以用 U+001F 分成左右两段：右段作为右浮动元素渲染在关闭按钮左侧。比例字体下标题文字的实际 advance 小于单元格预算，靠空格/EM SPACE 填充无法精确对齐；右浮动后缀不依赖字体宽度。

`wezterm cli list` 增加 `tab_index`、`window_index`、`is_active_tab`（表格末尾的 `ACTIVE` 列）；新增 `wezterm cli activate-window --window-id <WINID>`，通过 `SetFocusedPane` 切到该窗口的活动 tab 并前台化。`ListPanesResponse` 增加了 `active_tab_ids` 字段，因此 `CODEC_VERSION` 升到 47；升级后需要重启所有 GUI 进程，避免新旧 codec 混用。

该功能仅 Windows 有实现；上游目前没有全局热键/quake 模式。补丁同时在 `window/Cargo.toml` 显式声明了 `winapi/shellapi` feature：窗口代码原本就使用 `DragAcceptFiles`，此前只是靠其他 crate 的 feature 合并才编过，单独构建 `window` crate 会失败。

## 日常维护入口

日常维护由 dotfiles 仓库（`sean2077/dotfiles`）里的 `scripts/wezterm-fork.ps1` 完成。它负责回拉上游、rebase、推送、构建、安装和切换启动入口；Windows 上每次同步配置时都会运行它。下面是它背后的手动步骤，用于脚本停下（例如 rebase 冲突）时人工处理。

## 回拉上游

```sh
git fetch upstream
git switch main && git merge --ff-only upstream/main && git push origin main
git switch patched && git rebase main
# 冲突时逐个解决；补丁已被上游合并的话，rebase 会提示提交为空，用 git rebase --skip 跳过，并更新上面的补丁清单
git push --force-with-lease origin patched
```

## 构建与安装（Windows）

前提：VS 2022 Build Tools（MSVC）、Rust stable MSVC 工具链、原生 Windows Perl（上游 CI 同款 Strawberry Perl；MSYS 的 perl 无法构建 vendored OpenSSL）。本机用的是便携版 `~/.local/opt/strawberry-perl`。

```powershell
$env:PATH = "$env:USERPROFILE\.local\opt\strawberry-perl\perl\bin;" + $env:PATH
cargo build --release -p wezterm -p wezterm-gui -p wezterm-mux-server -p strip-ansi-escapes
```

安装目录是 `~/.local/opt/wezterm`。构建会把运行时文件（`conpty.dll`、`OpenConsole.exe`、`libEGL.dll`、`libGLESv2.dll`、`mesa/opengl32.dll`）一起放进 `target/release`，与 4 个 exe 一并复制过去即可。运行中的 WezTerm 会占用 exe，但 Windows 允许重命名被占用的文件：先把旧文件改名，再复制新文件（脚本就是这么做的）。

复用已有进程要求启动器和正在运行的 GUI 是**同一个 exe 路径**，所以任务栏、开始菜单、右键菜单要全部指向同一个安装目录。

验证方法：用 `--class` 指定一个测试类名，从两个不同的工作目录各执行一次 `wezterm-gui.exe start --class <测试类名>`，应该始终只有一个进程；在 WezTerm 外执行 `wezterm cli --class <测试类名> list`，应该能列出窗口。
