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

#4456 的影响：Windows 上 GUI 只把 `gui-sock-<pid>` 这个文件名发布到共享内存，新启动的 `wezterm-gui` 和 `wezterm cli` 按自己的工作目录去解析，于是任务栏、开始菜单、右键菜单每次启动都会新开一个进程（每个进程有几百 MB 的基础开销），在 WezTerm 外运行 `wezterm cli` 也会连不上。

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
cargo build --release -p wezterm -p wezterm-gui -p wezterm-mux-server
```

安装目录是 `~/.local/opt/wezterm`。其中运行时文件（`conpty.dll`、`OpenConsole.exe`、`libEGL.dll`、`libGLESv2.dll`、`mesa/opengl32.dll`、`strip-ansi-escapes.exe`）从官方安装目录复制，三个 exe 用 `target/release` 下的构建产物覆盖。覆盖前要先关掉所有从这个目录启动的 WezTerm，否则 exe 被占用，无法覆盖。

复用已有进程要求启动器和正在运行的 GUI 是**同一个 exe 路径**，所以任务栏、开始菜单、右键菜单要全部指向同一个安装目录。

验证方法：用 `--class` 指定一个测试类名，从两个不同的工作目录各执行一次 `wezterm-gui.exe start --class <测试类名>`，应该始终只有一个进程；在 WezTerm 外执行 `wezterm cli --class <测试类名> list`，应该能列出窗口。
