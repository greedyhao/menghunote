# UI 冒烟脚本（真实鼠标事件 + 首帧就绪轮询）
#
# 设计约束（都是踩过的坑，别为了快删掉）：
#  1. 启动前必须先 Stop-Process：Windows 上残留实例会占住 exe，cargo build 会
#     报「拒绝访问 (os error 5)」，脚本若继续跑就会拿旧二进制截图/断言。
#  2. cargo build 必须检查 EXIT=0，失败立即退出。
#  3. 点击用真实鼠标事件（SetCursorPos + mouse_event）；PostMessage 注入会丢事件。
#  4. 就绪判定改为轮询应用打出的「首帧就绪」日志（50ms 一次，最多 15s），
#     不再固定 sleep。
param(
    [string]$Exe = "target\debug\menghunote-app.exe",
    [ValidateSet("menu", "delete", "toggle", "rename", "renamecancel", "f2", "renameoutside")]
    [string]$Scenario = "menu",
    [string]$Screenshot = "",
    [int]$ReadyTimeoutMs = 15000,
    [int]$PollIntervalMs = 50
)

$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$logDir = Join-Path $repo "target\tmp"
$errPath = Join-Path $logDir "ui-smoke.log.err"
$outPath = Join-Path $logDir "ui-smoke.log"
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class UiSmoke {
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern void SwitchToThisWindow(IntPtr hWnd, bool fAltTab);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, IntPtr pid);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT p);
  [DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, uint dwData, IntPtr dwExtraInfo);
}
public struct POINT { public int X, Y; }
public struct RECT { public int Left, Top, Right, Bottom; }
"@
[UiSmoke]::SetProcessDPIAware() | Out-Null

function Stop-App {
    Get-Process menghunote-app -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 300
}

function Fail([string]$message) {
    Write-Output "FAIL: $message"
    if (Test-Path $errPath) {
        Write-Output "---- 日志尾部 ----"
        Get-Content $errPath -Encoding UTF8 -Tail 30 | ForEach-Object { Write-Output $_ }
    }
    Stop-App
    exit 1
}

# ---- 0. 清旧进程 ----
Stop-App

# ---- 1. 构建（必须 EXIT=0）----
$buildWatch = [System.Diagnostics.Stopwatch]::StartNew()
# cargo 往 stderr 打进度；`$ErrorActionPreference = Stop` 会把它当终止错误，
# 这里临时放宽，只用退出码判定成败。
$prevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& cargo build -p menghunote-app 2>&1 | Out-Null
$buildExit = $LASTEXITCODE
$ErrorActionPreference = $prevEap
$buildWatch.Stop()
if ($buildExit -ne 0) { Fail "cargo build 退出码 $buildExit" }
Write-Output ("build ok 用时 {0:N0}ms" -f $buildWatch.Elapsed.TotalMilliseconds)

# ---- 2. 启动 + 轮询首帧就绪 ----
Remove-Item $errPath -ErrorAction SilentlyContinue
Remove-Item $outPath -ErrorAction SilentlyContinue
$env:MENGHUNOTE_TRACE_MENU = "1"
$env:MENGHUNOTE_TRACE_MOUSE = "1"

$exePath = Join-Path $repo $Exe
$launchWatch = [System.Diagnostics.Stopwatch]::StartNew()
$proc = Start-Process -FilePath $exePath -RedirectStandardOutput $outPath -RedirectStandardError $errPath -PassThru

# 「首帧就绪」是中文；脚本本身是 ASCII，所以从这里拼出来，避免 PS 5.1 的 ANSI 解析坑
$readyMarker = -join @([char]0x9996, [char]0x5E27, [char]0x5C31, [char]0x7EEA)

$readyMs = -1
while ($launchWatch.Elapsed.TotalMilliseconds -lt $ReadyTimeoutMs) {
    if (Test-Path $errPath) {
        $content = Get-Content $errPath -Encoding UTF8 -Raw
        if ($content -and $content.Contains($readyMarker)) {
            $readyMs = $launchWatch.Elapsed.TotalMilliseconds
            break
        }
    }
    Start-Sleep -Milliseconds $PollIntervalMs
}
if ($readyMs -lt 0) { Fail ("等待首帧就绪超时（{0}ms）" -f $ReadyTimeoutMs) }
Write-Output ("首帧就绪 用时 {0:N0}ms（轮询间隔 {1}ms）" -f $readyMs, $PollIntervalMs)

$h = $proc.MainWindowHandle
if ($h -eq [IntPtr]::Zero) { Fail "拿不到主窗口句柄" }
[UiSmoke]::ShowWindow($h, 9) | Out-Null
[UiSmoke]::BringWindowToTop($h) | Out-Null
# 等窗口**真的**拿到前台：SetForegroundWindow 可能被系统拒绝（切前台有前台锁），
# 不确认就点，点击会落到别的窗口上（实测过一次「点了没反应、日志里连 mouse down 都没有」）。
$focusWatch = [System.Diagnostics.Stopwatch]::StartNew()
while ($focusWatch.Elapsed.TotalMilliseconds -lt 5000) {
    # Windows 有前台锁：先把本线程 attach 到当前前台线程，再 SetForegroundWindow，
    # 否则从后台（例如 CI / 后台任务）启动的脚本永远抢不到前台。
    $fg = [UiSmoke]::GetForegroundWindow()
    $fgThread = [UiSmoke]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
    $me = [UiSmoke]::GetCurrentThreadId()
    $attached = $false
    if ($fgThread -ne 0 -and $fgThread -ne $me) {
        $attached = [UiSmoke]::AttachThreadInput($me, $fgThread, $true)
    }
    [UiSmoke]::SetForegroundWindow($h) | Out-Null
    [UiSmoke]::SwitchToThisWindow($h, $false)
    if ($attached) { [UiSmoke]::AttachThreadInput($me, $fgThread, $false) | Out-Null }
    if ([UiSmoke]::GetForegroundWindow() -eq $h) { break }
    Start-Sleep -Milliseconds 50
}
# 无论如何都置顶一次：桌面可能被别的窗口（例如本会话的 GUI）抢占焦点，
# 置顶能保证点击落在被测窗口上，不依赖前台锁。
[UiSmoke]::SetWindowPos($h, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002) | Out-Null
if ([UiSmoke]::GetForegroundWindow() -ne $h) {
    # 抢不到前台（后台任务里很常见）：把窗口置顶，保证点击落在它身上。
    # 第一次点击可能只用于激活窗口，脚本会重试一次。
    Write-Output "warning: 没拿到前台，改用 HWND_TOPMOST 置顶"
    [UiSmoke]::SetWindowPos($h, [IntPtr](-1), 0, 0, 0, 0, 0x0001 -bor 0x0002) | Out-Null
} else {
    Write-Output ("前台就绪 用时 {0:N0}ms" -f $focusWatch.Elapsed.TotalMilliseconds)
}

$pt = New-Object POINT
[UiSmoke]::ClientToScreen($h, [ref]$pt) | Out-Null

function MoveTo([int]$x, [int]$y) {
    [UiSmoke]::SetCursorPos($pt.X + $x, $pt.Y + $y) | Out-Null
    Start-Sleep -Milliseconds 60
}
function LeftClick([int]$x, [int]$y) {
    MoveTo $x $y
    [UiSmoke]::mouse_event(0x0002, 0, 0, 0, [IntPtr]0)
    Start-Sleep -Milliseconds 30
    [UiSmoke]::mouse_event(0x0004, 0, 0, 0, [IntPtr]0)
}
function RightClick([int]$x, [int]$y) {
    MoveTo $x $y
    [UiSmoke]::mouse_event(0x0008, 0, 0, 0, [IntPtr]0)
    Start-Sleep -Milliseconds 30
    [UiSmoke]::mouse_event(0x0010, 0, 0, 0, [IntPtr]0)
}

function Save-Shot([string]$path) {
    if (-not $path) { return }
    Add-Type -AssemblyName System.Drawing
    $rect = New-Object RECT
    [UiSmoke]::GetWindowRect($h, [ref]$rect) | Out-Null
    $w = $rect.Right - $rect.Left
    $hh = $rect.Bottom - $rect.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $hh)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    [UiSmoke]::PrintWindow($h, $hdc, 2) | Out-Null
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    $bmp.Save($path)
    $bmp.Dispose()
    Write-Output "shot=$path"
}

function TypeText([string]$text) {
    # 逐字符发 WM_CHAR（走平台字符输入路径；不是 IME 组合）
    foreach ($ch in $text.ToCharArray()) {
        [UiSmoke]::PostMessage($h, 0x0102, [IntPtr][int][char]$ch, [IntPtr]0) | Out-Null
        Start-Sleep -Milliseconds 30
    }
}
function PressKey([int]$vk) {
    [UiSmoke]::PostMessage($h, 0x0100, [IntPtr]$vk, [IntPtr]0) | Out-Null
    Start-Sleep -Milliseconds 30
    [UiSmoke]::PostMessage($h, 0x0101, [IntPtr]$vk, [IntPtr]0) | Out-Null
}

function Wait-ForLog([string]$needle, [int]$timeoutMs) {
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    while ($watch.Elapsed.TotalMilliseconds -lt $timeoutMs) {
        $content = Get-Content $errPath -Encoding UTF8 -Raw
        if ($content -and $content.Contains($needle)) { return $watch.Elapsed.TotalMilliseconds }
        Start-Sleep -Milliseconds 20
    }
    return -1
}

# 行坐标（逻辑像素 → 物理像素，按 1.5 倍缩放）：行 0 y=40、行 1 y=64、行 2 y=88
# 侧栏宽 220 逻辑；这里按物理坐标点击（客户端坐标 = 逻辑 * 1.5）。
$menuOpened = "菜单打开" # 中文标记，同样在这里拼
$menuOpenedMarker = -join @([char]0x83DC, [char]0x5355, [char]0x6253, [char]0x5F00)

$clickWatch = [System.Diagnostics.Stopwatch]::StartNew()

switch ($Scenario) {
    "menu" {
        # 右键目录行（第 2 行，y≈96 物理）
        RightClick 90 96
        $ms = Wait-ForLog $menuOpenedMarker 3000
        $clickWatch.Stop()
        if ($ms -lt 0) { Fail "右键目录后 3s 内没有出现菜单日志" }
        Write-Output ("点击→菜单打开 用时 {0:N0}ms" -f $ms)
    }
    "toggle" {
        LeftClick 90 96
        $ms = Wait-ForLog "折叠" 3000
        $clickWatch.Stop()
        if ($ms -lt 0) { Fail "左键点目录后 3s 内没有折叠日志" }
        Write-Output ("点击→折叠 用时 {0:N0}ms" -f $ms)
    }
    "rename" {
        # 右键目录 → 菜单第一项「重命名」→ 输入 → Enter
        RightClick 90 96
        # 关键：等菜单**画出来**再点，否则点击会落到还没被遮罩覆盖的行上（实测踩过）
        if ((Wait-ForLog "菜单渲染" 3000) -lt 0) { Fail "右键后菜单没有渲染出来" }
        Start-Sleep -Milliseconds 120
        # 菜单在 (60,64)、表头 24、每项 26 → 第一项「重命名」中心 y≈101 逻辑（=151 物理）
        LeftClick 201 151
        $editMs = Wait-ForLog "开始重命名" 2000
        if ($editMs -lt 0) { Fail "右键菜单点重命名后没有进入编辑态" }
        Write-Output ("点击→进入编辑 用时 {0:N0}ms" -f $editMs)
        TypeText "Renamed"
        Start-Sleep -Milliseconds 400
        Save-Shot $Screenshot
        PressKey 0x0D
        $commitMs = Wait-ForLog "已重命名" 3000
        $clickWatch.Stop()
        if ($commitMs -lt 0) { Fail "Enter 后没有提交（没有 已重命名 日志）" }
        Write-Output ("Enter→落库 用时 {0:N0}ms" -f $commitMs)
    }
    "renamecancel" {
        # 右键目录 → 重命名 → 输入 → Esc → 不应写库
        RightClick 90 96
        if ((Wait-ForLog "菜单渲染" 3000) -lt 0) { Fail "右键后菜单没有渲染出来" }
        Start-Sleep -Milliseconds 120
        LeftClick 201 151
        Start-Sleep -Milliseconds 300
        if ((Wait-ForLog "开始重命名" 2000) -lt 0) { Fail "没有进入编辑态" }
        TypeText "X"
        Start-Sleep -Milliseconds 300
        Save-Shot $Screenshot
        PressKey 0x1B   # Esc
        $cancelMs = Wait-ForLog "取消重命名" 3000
        $clickWatch.Stop()
        if ($cancelMs -lt 0) { Fail "Esc 没有取消编辑" }
        Write-Output ("Esc→取消 用时 {0:N0}ms" -f $cancelMs)
    }
    "f2" {
        # 左键点笔记行（打开并聚焦编辑器）→ F2 → 应进入重命名
        LeftClick 90 60
        Start-Sleep -Milliseconds 400
        if ((Wait-ForLog "打开笔记" 3000) -lt 0) { Fail "点击笔记没有打开" }
        PressKey 0x71   # F2
        $ms = Wait-ForLog "开始重命名" 3000
        $clickWatch.Stop()
        if ($ms -lt 0) { Fail "F2 没有触发重命名" }
        Write-Output ("F2→进入编辑 用时 {0:N0}ms" -f $ms)
    }
    "renameoutside" {
        # 右键目录 → 重命名（进入时截图应看到原名且全选）→ 输入 → 点正文 = 确认提交
        RightClick 90 96
        if ((Wait-ForLog "菜单渲染" 3000) -lt 0) { Fail "右键后菜单没有渲染出来" }
        Start-Sleep -Milliseconds 120
        LeftClick 201 151
        if ((Wait-ForLog "开始重命名" 2000) -lt 0) { Fail "没有进入编辑态" }
        Start-Sleep -Milliseconds 700
        Save-Shot $Screenshot            # 进编辑态的第一张：原名应可见且全选
        TypeText "Outside"
        Start-Sleep -Milliseconds 300
        LeftClick 800 700               # 点正文 = 点外部 = 确认
        $ms = Wait-ForLog "已重命名" 3000
        $clickWatch.Stop()
        if ($ms -lt 0) { Fail "点外部后没有提交" }
        Write-Output ("点外部→落库 用时 {0:N0}ms" -f $ms)
    }
    "delete" {
        RightClick 90 60
        Start-Sleep -Milliseconds 200
        LeftClick 201 114
        $ms = Wait-ForLog "确认删除" 3000
        $clickWatch.Stop()
        if ($ms -lt 0) { Fail "笔记右键删除第一步没有进入确认态" }
        Write-Output ("点击→确认删除 用时 {0:N0}ms" -f $ms)
    }
}

Write-Output ("点击阶段总耗时 {0:N0}ms" -f $clickWatch.Elapsed.TotalMilliseconds)

# ---- 3. 收尾 ----
[UiSmoke]::SetCursorPos($pt.X - 200, $pt.Y - 200) | Out-Null
Stop-App
Write-Output "---- 日志尾部 ----"
Get-Content $errPath -Encoding UTF8 -Tail 12 | ForEach-Object { Write-Output $_ }
Write-Output "OK"
exit 0
