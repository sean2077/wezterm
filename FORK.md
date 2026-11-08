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
| gui: make the fancy tab bar border width configurable | 自用 | — | 上游提供等价的标签栏边框宽度选项后可删 |
| gui: ring tabs whose panes are waiting for input | 自用 | — | 上游提供按 user var 高亮标签栏边框的能力后可删 |
| gui: outline the active tab and light up waiting tabs | 自用 | — | 上游提供活动 tab 门框/标签栏分隔线与按状态高亮的能力后可删 |
| gui: color the tab bar separator with the active tab's accent | 自用 | — | 同上（依赖上一条） |
| gui: draw macOS-style traffic light window buttons | 自用 | — | 上游在非 macOS 平台支持 `integrated_title_button_style = "MacOsNative"` 时可删 |
| cli: report tab/window indices and focus the owning GUI window | 自用 | — | 上游的 `list` 输出索引且 `activate-*` 会前台化窗口后可删 |
| mux: generic workspace save/restore and agent user vars | 自用 | — | 上游提供等价的 session persistence/CLI 后可删 |
| gui: expose tab budgets, mergeable overrides, CWD links and status coalescing | 自用 | — | 上游提供等价 API 后可删 |

#4456 的影响：Windows 上 GUI 只把 `gui-sock-<pid>` 这个文件名发布到共享内存，新启动的 `wezterm-gui` 和 `wezterm cli` 按自己的工作目录去解析，于是任务栏、开始菜单、右键菜单每次启动都会新开一个进程（每个进程有几百 MB 的基础开销），在 WezTerm 外运行 `wezterm cli` 也会连不上。

自用清理补丁：GUI 正常退出会在 `listener.run()` 返回后删除自己的 `gui-sock-<pid>`（`wezterm-gui/src/main.rs`），但崩溃、强杀和隔离测试实例都不走这条路径；这些标记文件放在 `~/.local/share/wezterm`，重启电脑也不会清。上游只在自动更新路径调用 `discover_gui_socks()`，日常启动不调用，死文件会一直留着。补丁在 `async_run_terminal_gui` 开头调用一次 `discover_gui_socks()`；该函数只删除连接失败且超过 1 秒的 socket，正在启动的实例不受影响。`agent.<pid>`（SSH agent 符号链接）和旧命名 `sock` 不在清理范围。

#8166 的影响：Windows 上 `TermWindow::update_text_cursor` 在每帧绘制时都调用 `ImmSetCandidateWindow`（Builtin preedit）或 `ImmSetCompositionWindow`（System preedit），而 IME 每次收到调用都会销毁并重建候选窗，于是组合输入时候选窗每敲一键闪一次（Google 日语输入法、微软 IME、Rime/Weasel 都如此；Claude Code、Codex CLI 这类高频重绘的 TUI 更明显）。补丁缓存上次上报给 IME 的光标矩形，位置不变就跳过调用；在 `WM_SETFOCUS`、`WM_IME_SETCONTEXT`、`WM_IME_STARTCOMPOSITION`、`WM_IME_ENDCOMPOSITION` 以及配置变化时清缓存，保证重新获得焦点、每次组合开始时至少重新上报一次。

## Quake 下拉模式（自用）

`config.quake` 让一个专用 WezTerm 实例作为 Guake 式下拉终端：窗口在登录时隐藏启动，注册全局热键（例如 F3），按热键在“隐藏”和“工作区顶部矩形”之间切换；支持置顶、失焦收起、关闭即隐藏。实现位于 `window/src/os/windows/quake.rs`，使用 `RegisterHotKey` + `WM_HOTKEY`，窗口样式设为 `WS_EX_TOOLWINDOW`，因此不出现在任务栏或 Alt+Tab。

用法：用 `--config-file` 指定一个 `quake.lua`，并用 `--class` 给专用实例一个独立类名。`quake.lua` 应设置 `config.quake`、`window_decorations = "NONE"`（外框与客户区一致，几何才准确）、`tab_bar_at_bottom = true`。热键注册失败时窗口会回落为可见状态，而不是留下隐形实例；`WM_CLOSE` 在 quake 模式下只收起窗口，显式退出仍用 `QuitApplication`。全屏状态由 `QuakeState` 在窗口进程内维护：quake 模式下原生的 `ToggleFullScreen` 动作改为在“配置的下拉矩形”和“整个显示器”之间切换，F3 收起再呼出后保持当前状态；Lua 只需绑定 `wezterm.action.ToggleFullScreen`，不需要计算几何或写 per-window override。

`config.tab_bar_uniform_width = true` 让 fancy 标签栏的每个 tab 等宽，并把 `+` 按钮从均分分母中拿出来、按固定宽度布局；标题短于份额时补齐、超出时截断，标签栏几何不再随标题长度变化。均分预算优先使用 `terminal_size.pixel_width`，并扣除右侧状态栏和集成窗口按钮的占用。`config.tab_bar_side_reserve_ratio`（默认 0.6）控制这部分预留的比例，调参不需要重新编译。

`config.tab_bar_tab_border_width` 控制 fancy 标签栏每个 tab 的四边边框宽度，默认 `1px`（与上游一致），支持 `"2px"`、`"1.5pt"` 等 Dimension 写法，`pt` 会按显示 DPI 缩放。上游只有非活动 tab 的右边框可见（颜色取 `colors.tab_bar.inactive_tab_edge`），其余三边使用 tab 自身背景色；调大后该分隔线随之加粗。hover 时这条边线现在也会保留，颜色改用此前已定义但从未被渲染读取的 `colors.tab_bar.inactive_tab_edge_hover`。改宽度只需改配置，不需要重新编译。

`colors.tab_bar.waiting_tab_edge`（默认 `#ff9500`）让任意 pane 的 `agent_state` user var 为 `waiting`（可带 `:epoch` 后缀）的 tab 显示一圈细边框（线宽同 `tab_bar_tab_border_width`）；远离终端内容的一侧（顶部标签栏为上边）在两个圆角之间加一条约 3 逻辑像素的指示灯条，框外再画最多 3 层逐渐变淡的光晕（不透明度 0.5/0.26/0.12，每层约 1 逻辑像素，层数受 tab 与标签栏边缘之间的空间限制）。灯条和光晕的颜色由 `colors.tab_bar.waiting_tab_glow` 控制，默认与 `waiting_tab_edge` 相同。活动、非活动和 hover 三种状态都生效，`running`/`done`/`error` 不触发。判定与 Lua 徽章一致：遍历 mux tab 的真实 pane（`iter_panes_ignoring_zoom`，忽略 zoom 和 GUI overlay），任一 waiting 即高亮。状态变化会进入 `TabEntry` 并参与缓存比较，user var 更新后立即触发重绘。

`colors.tab_bar.active_tab_edge`（默认不设置，即保持上游外观）设置后，标签栏靠近终端内容的一侧画一条贯穿整个窗口的分隔线，并在活动 tab 处折成左、上、右三边的门框（Chrome 式）；底边断开，活动 tab 与内容区连在一起。活动 tab 同时处于 waiting 时，门框改用 `waiting_tab_edge` 的颜色，并加上灯条和光晕。门框两侧与分隔线相接处不是直角，而是一个向外张开的圆弧过渡（半径约为标题字体格高的 0.3 倍，且不小于线宽），分隔线在离门柱一个半径处停住，再用实心四分之一圆补上转角。`tab_bar_at_bottom = true` 时上下镜像。

分隔线和门框的颜色可以按 tab 指定：`format-tab-title` 返回值中，U+001D 标记处生效的前景色就是该 tab 的强调色，写法为 `{ Foreground = { Color = '#rrggbb' } }, { Text = '\x1d' }`。标记本身会被剥掉，不占列宽，只有显式 RGB 颜色才算。tab 处于活动状态时，用它的强调色画整条分隔线和门框，所以切换 tab 时边界线跟着换色。没有声明强调色时，用 `active_tab_edge`。活动且 waiting 的 tab，门框仍为橙色，分隔线仍用强调色。强调色随 `TabEntry` 参与缓存比较，变化后立即重绘。`config.tab_bar_active_edge_width`（Dimension，默认 0，表示跟随 `tab_bar_tab_border_width`）单独设置分隔线和门框的线宽，可以比 tab 之间的分隔线和等待环更粗；`pt` 随 DPI 缩放。

这些线条都画在 zindex 20 的叠加层上（标签栏之上、模态浮层之下），不参与布局，`tab_bar_uniform_width` 的几何不变。tab 元素自身的边框保持填充色：box model 的圆角是用边框色填充的实心四分之一圆，边框一旦换色，圆角就会变成色块。所以门框和等待环的圆角不改边框色，而是叠两层实心四分之一圆：外层是线条色，内层是 tab 当前的填充色（悬停时取悬停色），同圆心、半径小一个线宽。两条边缘都是抗锯齿填充，弧线和直线的粗细、亮度一致。1px 的描边弧线在实测中又淡又断，所以不用。填充色不是纯色时退回描边。光晕的弧线是半透明的，仍然用描边。tab 圆角改为宽高相同的像素半径（标题字体格高的 0.4 倍）：上游用 0.5 格宽 × 0.5 格高，格子高于宽，实际是椭圆，描边后看起来像斜切。几何计算在纯函数 `tab_bar_decorations` 中完成，单测位于 `fancy_tab_bar.rs` 的 `decoration_tests`。

`format-tab-title` 的第 6 个参数（`max_width`）在 fancy bar 下现在按标题字体宽度和 tab 数换算成真实预算，不再是恒定的 `tab_max_width`；第二遍渲染在 uniform 模式下会扣掉回调声明的左块和右浮动后缀/关闭按钮，把剩下的宽度当作标题块预算。回调可以用 U+001E 标记左块（索引/徽章）与标题块的分界。`window:set_config_override(key, value)` / `clear_config_override(key)` 用于增量修改 per-window override；hyperlink URI 中的 `$CWD` 在点击时展开为当前 pane 的工作目录；`update-status` / `update-right-status` 统一按 `status_update_interval` 调度，不再随高频标题变化每帧触发。

`format-tab-title` 的返回值可以用 U+001F 分成左右两段：右段作为右浮动元素渲染在关闭按钮左侧。比例字体下标题文字的实际 advance 小于单元格预算，靠空格/EM SPACE 填充无法精确对齐；右浮动后缀不依赖字体宽度。

`config.integrated_title_button_style = "MacOsNative"` 原本只在 macOS 生效（按钮由系统绘制），现在任何平台都接受该值：macOS 仍由系统绘制，其他平台由 fork 自绘三个圆点（关=红 `#ff5f57`、最小化=黄 `#febc2e`、最大化=绿 `#28c840`），顺序按 `integrated_title_buttons`，位置按 `integrated_title_button_alignment`（默认右侧），悬停时同色相提亮。圆点直径约为标题字体格高的一半（本机约 13px），三个合计约 60px，比 Windows 字形按钮（每个约 46px）窄一半以上，右侧状态栏因此多出约 70px。fancy 标签栏按实际像素宽度为它们预留空间（`reserved_side_width`），均分 tab 不会压到按钮下面。`window_buttons_at_left` 的「macOS 按钮总在左边」逻辑、以及标签栏最左侧为原生按钮预留的 4 格边距，都只对真正的 macOS 生效；否则 tab 会被无谓地往右推。

`wezterm cli list` 增加 `tab_index`、`window_index`、`is_active_tab`（表格末尾的 `ACTIVE` 列）；新增 `wezterm cli activate-window --window-id <WINID>`，通过 `SetFocusedPane` 切到该窗口的活动 tab 并前台化。`ListPanesResponse` 增加了 `active_tab_ids` 字段，因此 `CODEC_VERSION` 升到 47；升级后需要重启所有 GUI 进程，避免新旧 codec 混用。

该功能仅 Windows 有实现；上游目前没有全局热键/quake 模式。补丁同时在 `window/Cargo.toml` 显式声明了 `winapi/shellapi` feature：窗口代码原本就使用 `DragAcceptFiles`，此前只是靠其他 crate 的 feature 合并才编过，单独构建 `window` crate 会失败。

## 通用工作区持久化（自用）

`mux` 新增通用 `SessionState`，保存窗口/tab/活动 pane、cwd、原始 spawn argv、标题和 user vars；不保存分屏树、颜色、zoom、窗口几何。每个 tab 只恢复活动 pane。新增 CLI：

```text
wezterm cli save-workspace [--path FILE]
wezterm cli restore-workspace --path FILE [--window-id ID] [--replace] [--legacy-claude-pending]
wezterm cli close-agent-sessions [--timeout-seconds N] [--force]
wezterm cli set-user-var --pane-id ID --name NAME [--value VALUE | --unset]
```

Agent 集成通过通用 user vars 完成：`agent.resume_args`（JSON argv 追加片段）、`agent.shutdown`（非空表示参与优雅关闭）、`agent.session_id`（诊断用）。`close-agent-sessions` 只处理带 `agent.shutdown` 的 pane，发送 Esc → Ctrl+C → `/exit\r`，不识别具体 agent 名称。`restore-workspace` 只把 `agent.resume_args` 追加到原始 argv；mux/CLI 不包含 Claude 专用字段。

`--legacy-claude-pending` 是首次迁移用的一次性适配器，读取旧 `pending.json` 并转换成新格式；它位于 CLI 层，不进入 mux 核心。

该改动修改了 `codec`，`CODEC_VERSION` 升到 48；安装后必须重启所有 GUI/mux 进程。首次从 47 升级时，新 CLI 不能连接旧 GUI，需要保留旧 tabs 脚本完成一次保存/优雅关闭，再用新 GUI 的 `restore-workspace --legacy-claude-pending` 恢复；确认后即可删除旧脚本。

## 日常维护入口

日常维护由 dotfiles 仓库（`sean2077/dotfiles`）里的 `scripts/wezterm-fork.ps1` 完成。它负责回拉上游、merge、推送、构建、安装和切换启动入口；Windows 上每次同步配置时都会运行它。下面是它背后的手动步骤，用于脚本停下（例如 merge 冲突）时人工处理。

## 回拉上游

一律用 merge，不 rebase：补丁提交保持原哈希，`patched` 历史只增不改，推送不需要 force。2026-11-08 之前 `patched` 是按 rebase 维护的，那段历史保持原样。

```sh
git fetch upstream
git switch main && git merge --ff-only upstream/main && git push origin main
git switch patched && git merge --no-ff main
# 冲突时逐个解决后 git commit；补丁已被上游合并的话，确认后用 git revert 去掉它，并更新上面的补丁清单
git push origin patched
```

查看当前补丁（跳过 merge 提交和内容已与上游等价的提交）：`git log --oneline --no-merges --cherry-pick --right-only main...patched`。```

## 构建与安装（Windows）

前提：VS 2022 Build Tools（MSVC）、Rust stable MSVC 工具链、原生 Windows Perl（上游 CI 同款 Strawberry Perl；MSYS 的 perl 无法构建 vendored OpenSSL）。本机用的是便携版 `~/.local/opt/strawberry-perl`。

```powershell
$env:PATH = "$env:USERPROFILE\.local\opt\strawberry-perl\perl\bin;" + $env:PATH
cargo build --release -p wezterm -p wezterm-gui -p wezterm-mux-server -p strip-ansi-escapes
```

安装目录是 `~/.local/opt/wezterm`。构建会把运行时文件（`conpty.dll`、`OpenConsole.exe`、`libEGL.dll`、`libGLESv2.dll`、`mesa/opengl32.dll`）一起放进 `target/release`，与 4 个 exe 一并复制过去即可。运行中的 WezTerm 会占用 exe，但 Windows 允许重命名被占用的文件：先把旧文件改名，再复制新文件（脚本就是这么做的）。

复用已有进程要求启动器和正在运行的 GUI 是**同一个 exe 路径**，所以任务栏、开始菜单、右键菜单要全部指向同一个安装目录。

验证方法：用 `--class` 指定一个测试类名，从两个不同的工作目录各执行一次 `wezterm-gui.exe start --class <测试类名>`，应该始终只有一个进程；在 WezTerm 外执行 `wezterm cli --class <测试类名> list`，应该能列出窗口。
