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
    [ValidateSet("menu", "delete", "toggle", "rename", "renamecancel", "f2", "renameoutside", "projection", "displayedit", "cursorkeys", "blockcaret", "export", "shot", "exportstress", "quotehit", "search", "searchreal", "searchempty", "biashit", "biasweep", "groupenter", "groupfold", "searchcount", "searchprogress", "searchautorequery")]
    [string]$Scenario = "menu",
    [string]$Screenshot = "",
    [string]$Vault = "",
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
  [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
  [DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vKey);
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
if ($Vault) { $env:MENGHUNOTE_DEV_VAULT = $Vault }
$env:MENGHUNOTE_TRACE_MENU = "1"
$env:MENGHUNOTE_TRACE_MOUSE = "1"
if ($Scenario -in @('search', 'searchreal', 'searchempty', 'searchcount', 'searchprogress', 'searchautorequery', 'groupenter', 'groupfold')) { $env:MENGHUNOTE_SEARCH_ON_START = "1" }

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
    # PrintWindow 可能拿到上一帧 surface：抓两次，用第二次的位图。
    foreach ($pass in 1, 2) {
        $bmp = New-Object System.Drawing.Bitmap($w, $hh)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $hdc = $g.GetHdc()
        [UiSmoke]::PrintWindow($h, $hdc, 2) | Out-Null
        $g.ReleaseHdc($hdc)
        $g.Dispose()
        if ($pass -eq 2) {
            $bmp.Save($path)
        }
        $bmp.Dispose()
        if ($pass -eq 1) { Start-Sleep -Milliseconds 250 }
    }
    Write-Output "shot=$path"
}

function TypeText([string]$text) {
    # 逐字符发 WM_CHAR（走平台字符输入路径；不是 IME 组合）
    foreach ($ch in $text.ToCharArray()) {
        [UiSmoke]::PostMessage($h, 0x0102, [IntPtr][int][char]$ch, [IntPtr]0) | Out-Null
        Start-Sleep -Milliseconds 30
    }
}

function Focus-App {
    # Windows 前台锁：attach 到当前前台线程再 SetForegroundWindow，否则后台进程抢不到前台。
    for ($i = 0; $i -lt 40; $i++) {
        $fg = [UiSmoke]::GetForegroundWindow()
        if ($fg -eq $h) { return $true }
        $fgThread = [UiSmoke]::GetWindowThreadProcessId($fg, [IntPtr]::Zero)
        $me = [UiSmoke]::GetCurrentThreadId()
        $attached = $false
        if ($fgThread -ne 0 -and $fgThread -ne $me) {
            $attached = [UiSmoke]::AttachThreadInput($me, $fgThread, $true)
        }
        [UiSmoke]::SetForegroundWindow($h) | Out-Null
        [UiSmoke]::BringWindowToTop($h) | Out-Null
        if ($attached) { [UiSmoke]::AttachThreadInput($me, $fgThread, $false) | Out-Null }
        Start-Sleep -Milliseconds 50
    }
    return ([UiSmoke]::GetForegroundWindow() -eq $h)
}
function PressCtrlKey([int]$vk) {
    # 本机前台被 DSH GUI 占着，keybd_event 的按键会落到前台窗口而不是被测窗口。
    # 折中：keybd_event 只按 **Ctrl**（更新全局键状态），K 的 WM_KEYDOWN/UP 直接
    # PostMessage 给被测窗口。必须先确认 Ctrl 真的按下去了再发 K，否则会偶发
    # 收到 ctrl=false（实测过一次，按太快）。
    [UiSmoke]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero)
    for ($i = 0; $i -lt 40; $i++) {
        # `GetKeyState` 只反映**本线程**队列里的键状态，注入的按键看不到；必须用全局异步状态。
        if (([UiSmoke]::GetAsyncKeyState(0x11) -band 0x8000) -ne 0) { break }
        Start-Sleep -Milliseconds 20
    }
    Start-Sleep -Milliseconds 80
    [UiSmoke]::PostMessage($h, 0x0100, [IntPtr]$vk, [IntPtr]0) | Out-Null
    Start-Sleep -Milliseconds 80
    [UiSmoke]::PostMessage($h, 0x0101, [IntPtr]$vk, [IntPtr]0) | Out-Null
    Start-Sleep -Milliseconds 60
    [UiSmoke]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 60
}
function PressRealKey([int]$vk) {
    [UiSmoke]::keybd_event([byte]$vk, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 30
    [UiSmoke]::keybd_event([byte]$vk, 0, 2, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 40
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
    "projection" {
        # 打开演示笔记后：点正文空白处（caret 离开标题/强调）→ 截图；再点进强调行 → 截图
        LeftClick 60 1000
        Start-Sleep -Milliseconds 600
        PressKey 0x7B   # F12：dump 当前选区（用于核对 caret 落点）
        Start-Sleep -Milliseconds 700
        Save-Shot ($Screenshot -replace '\.png$', '-a-hidden.png')
        # 再点进「**粗体** / `代码`」那一行，标记应显现
        # 点进 `**粗体**` 内部（文字区左边界 ≈ 物理 351，粗体在 351..400）
        LeftClick 362 138
        Start-Sleep -Milliseconds 500
        PressKey 0x7B
        MoveTo 700 500          # 移动鼠标触发一次重绘，避免 PrintWindow 拿到旧帧
        Start-Sleep -Milliseconds 900
        Save-Shot ($Screenshot -replace '\.png$', '-b-shown.png')
        Write-Output "projection shots ok"
    }
    "displayedit" {
        # ① caret 停在「**粗体**」的"粗"之前 → Delete 应删"粗"而不是隐藏的 *
        LeftClick 352 138
        Start-Sleep -Milliseconds 400
        PressKey 0x2E
        Start-Sleep -Milliseconds 600
        # ② 先点远处让标记隐藏，再把 caret 放到「粗体」在**隐藏态**的末尾
        #    （此时刚过"体"的下一个可见字符是空格，不是 *）→ Delete 应删空格
        LeftClick 60 1000
        Start-Sleep -Milliseconds 400
        LeftClick 404 138
        Start-Sleep -Milliseconds 400
        PressKey 0x2E
        Start-Sleep -Milliseconds 600
        # ③ 退格：caret 放在"斜体"之前 → 应删前一个可见字符而不是标记
        LeftClick 352 178
        Start-Sleep -Milliseconds 400
        PressKey 0x08
        Start-Sleep -Milliseconds 600
        MoveTo 700 500
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-after.png')
        Write-Output "displayedit ok"
    }
    "cursorkeys" {
        # caret 放到构造之后（隐藏态），按左键应落在"体"之后而不是两个 * 之间
        LeftClick 60 1000
        Start-Sleep -Milliseconds 400
        LeftClick 352 138          # 停在「粗」之前（隐藏态）
        Start-Sleep -Milliseconds 400
        PressKey 0x27              # Right：过"粗"
        Start-Sleep -Milliseconds 300
        PressKey 0x27              # Right：过"体" → 应在 ** 之前
        Start-Sleep -Milliseconds 300
        PressKey 0x27              # Right：跳过隐藏的 **，落到空格之后
        Start-Sleep -Milliseconds 300
        PressKey 0x25              # Left：回到"体"之后（不应回到 * 之间）
        Start-Sleep -Milliseconds 600
        MoveTo 700 500
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-keys.png')
        Write-Output "cursorkeys ok"
    }
    "blockcaret" {
        # 点标题行内部：caret 高度应与标题行高一致；同时验证命中不偏移
        LeftClick 470 55
        Start-Sleep -Milliseconds 600
        MoveTo 700 500
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-title.png')
        Write-Output "blockcaret ok"
    }
    "export" {
        # 点底栏「导出」→ 等日志里的导出完成行
        LeftClick 275 1126        # 底栏第三格（导出）中心，物理坐标（客户端）
        $ms = Wait-ForLog "导出完成" 15000
        $clickWatch.Stop()
        if ($ms -lt 0) { Fail "点导出后 15s 内没有完成日志" }
        Write-Output ("点击→导出完成 用时 {0:N0}ms" -f $ms)
        Start-Sleep -Milliseconds 500
        MoveTo 700 500
        Start-Sleep -Milliseconds 400
        Save-Shot ($Screenshot -replace '\.png$', '-done.png')
    }
    "shot" {
        MoveTo 700 500
        Start-Sleep -Milliseconds 400
        Save-Shot $Screenshot
        Write-Output "shot ok"
    }
    "exportstress" {
        # ① 点「导出」后**立刻**动鼠标、点侧栏：证明 UI 不冻结
        $t0 = [System.Diagnostics.Stopwatch]::StartNew()
        LeftClick 275 1126
        Start-Sleep -Milliseconds 60
        MoveTo 260 260
        Start-Sleep -Milliseconds 60
        LeftClick 90 120           # 点侧栏第 0 行（切笔记）
        Start-Sleep -Milliseconds 60
        MoveTo 500 700
        $ms = Wait-ForLog "导出完成" 120000
        $t0.Stop()
        if ($ms -lt 0) { Fail "120s 内没有导出完成日志" }
        Write-Output ("点导出→完成 用时 {0:N0}ms（UI 全程可交互见日志顺序）" -f $t0.Elapsed.TotalMilliseconds)
        Start-Sleep -Milliseconds 400
        Save-Shot ($Screenshot -replace '\.png$', '-stress.png')
    }
    "quotehit" {
        # 在引用行内点击 + 拖选：落点必须与文字对齐（缩进不能只加在绘制上）
        LeftClick 520 645
        Start-Sleep -Milliseconds 400
        PressKey 0x7B          # F12 dump 选区/光标
        Start-Sleep -Milliseconds 400
        # 拖选：按下 → 移动 → 抬起（跨引用行）
        MoveTo 420 645
        Start-Sleep -Milliseconds 120
        [UiSmoke]::mouse_event(0x0002, 0, 0, 0, [IntPtr]0)
        Start-Sleep -Milliseconds 80
        MoveTo 640 690
        Start-Sleep -Milliseconds 120
        [UiSmoke]::mouse_event(0x0004, 0, 0, 0, [IntPtr]0)
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-select.png')
        Write-Output "quotehit ok"
    }
    "search" {
        # 自绘输入（IME 版）的 Esc 路径：浮层打开后打字 → Esc 关闭 → 编辑器继续可输入。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 800
        TypeText "menghunote"
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-typed.png')
        PressKey 0x1B              # Esc：组合外 → 关浮层
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "Esc 没有关闭搜索浮层" }
        Start-Sleep -Milliseconds 400
        TypeText "ZZ"
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-after.png')
        Write-Output "search ok"
    }
    "biasweep" {
        # 在 `source_text()` 那一行横向扫一遍，记录每个落点的 bias（取证用）。
        $xs = @(560, 600, 640, 680, 720, 760, 800, 840, 880, 920, 960, 1000, 1040, 1080)
        foreach ($x in $xs) {
            LeftClick $x 785
            Start-Sleep -Milliseconds 120
        }
        Save-Shot ($Screenshot -replace '\.png$', '-sweep.png')
        Write-Output "biasweep ok"
    }
    "biashit" {
        # 命中偏置取证：点 `source_text()` 行内构造的末尾 `)` 右半，日志应 bias=Left、落在构造内。
        # 演示文档里 `source_text()` 那行大约在 y=785（物理），先点它的右半。
        LeftClick 900 785
        Start-Sleep -Milliseconds 400
        Save-Shot ($Screenshot -replace '\.png$', '-right.png')
        # 再点同一行走开一点（左半）
        LeftClick 700 785
        Start-Sleep -Milliseconds 400
        Save-Shot ($Screenshot -replace '\.png$', '-left.png')
        Write-Output "biashit ok"
    }
    "searchautorequery" {
        # 构建中先输入查询，构建完成后应**自动重查**并显示结果数（不需要再敲键）。
        if ((Wait-ForLog "打开搜索浮层" 15000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 400
        TypeText "000"
        Start-Sleep -Milliseconds 15000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchautorequery ok"
    }
    "groupfold" {
        # 折叠态截图 → 点分组**标题行**（y≈345 物理 = 230 逻辑）展开 → 展开态截图。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        Save-Shot ($Screenshot -replace '\.png$', '-collapsed.png')
        LeftClick 700 345
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-expanded.png')
        Write-Output "groupfold ok"
    }
    "groupenter" {
        # 预填查询（MENGHUNOTE_SEARCH_QUERY）→ Enter 应打开当前（第一）个分组。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        PressKey 0x0D
        if ((Wait-ForLog "打开搜索结果" 5000) -lt 0) { Fail "Enter 没有打开分组结果" }
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "打开后浮层没有关闭" }
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-opened.png')
        Write-Output "groupenter ok"
    }
    "searchcount" {
        # 总数 + 单行摘要 + 两个开关的截图（不点开关，点击不稳）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 900
        TypeText "menghunote"
        Start-Sleep -Milliseconds 1200
        Save-Shot ($Screenshot -replace '\.png$', '-count.png')
        Write-Output "searchcount ok"
    }
    "searchprogress" {
        # 首建进度：批量物化后 2000 篇首建只要 2–3s，所以连拍覆盖整个窗口期
        # （拍前把鼠标移进窗口，尽量逼一帧重绘；PrintWindow 偶尔返回缓存 surface）。
        Start-Sleep -Milliseconds 900
        foreach ($i in 0..5) {
            MoveTo 700 500
            Start-Sleep -Milliseconds 120
            Save-Shot ($Screenshot -replace '\.png$', "-f$i.png")
            Start-Sleep -Milliseconds 300
        }
        Start-Sleep -Milliseconds 3000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchprogress ok"
    }
    "searchautorequery" {
        # 构建中先输入查询，构建完成后应**自动重查**并显示结果数（不需要再敲键）。
        if ((Wait-ForLog "打开搜索浮层" 15000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 400
        TypeText "000"
        Start-Sleep -Milliseconds 15000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchautorequery ok"
    }
    "groupfold" {
        # 折叠态截图 → 点分组**标题行**（y≈345 物理 = 230 逻辑）展开 → 展开态截图。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        Save-Shot ($Screenshot -replace '\.png$', '-collapsed.png')
        LeftClick 700 345
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-expanded.png')
        Write-Output "groupfold ok"
    }
    "groupenter" {
        # 预填查询（MENGHUNOTE_SEARCH_QUERY）→ Enter 应打开当前（第一）个分组。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        PressKey 0x0D
        if ((Wait-ForLog "打开搜索结果" 5000) -lt 0) { Fail "Enter 没有打开分组结果" }
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "打开后浮层没有关闭" }
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-opened.png')
        Write-Output "groupenter ok"
    }
    "searchcount" {
        # 总数 + 单行摘要 + 两个开关的截图（不点开关，点击不稳）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 900
        TypeText "menghunote"
        Start-Sleep -Milliseconds 1200
        Save-Shot ($Screenshot -replace '\.png$', '-count.png')
        Write-Output "searchcount ok"
    }
    "searchprogress" {
        # 连拍；每次拍前把鼠标移到窗口内，逼一帧重绘（否则 PrintWindow 可能拿到旧 surface）。
        Start-Sleep -Milliseconds 900
        foreach ($i in 0..11) {
            MoveTo 700 500
            Start-Sleep -Milliseconds 120
            Save-Shot ($Screenshot -replace '\.png$', "-g$i.png")
            Start-Sleep -Milliseconds 260
        }
        Write-Output "searchprogress ok"
    }
    "searchautorequery" {
        # 构建中先输入查询，构建完成后应**自动重查**并显示结果数（不需要再敲键）。
        if ((Wait-ForLog "打开搜索浮层" 15000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 400
        TypeText "000"
        Start-Sleep -Milliseconds 15000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchautorequery ok"
    }
    "groupfold" {
        # 折叠态截图 → 点分组**标题行**（y≈345 物理 = 230 逻辑）展开 → 展开态截图。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        Save-Shot ($Screenshot -replace '\.png$', '-collapsed.png')
        LeftClick 700 345
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-expanded.png')
        Write-Output "groupfold ok"
    }
    "groupenter" {
        # 预填查询（MENGHUNOTE_SEARCH_QUERY）→ Enter 应打开当前（第一）个分组。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        PressKey 0x0D
        if ((Wait-ForLog "打开搜索结果" 5000) -lt 0) { Fail "Enter 没有打开分组结果" }
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "打开后浮层没有关闭" }
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-opened.png')
        Write-Output "groupenter ok"
    }
    "searchcount" {
        # 总数 + 单行摘要 + 两个开关的截图（不点开关，点击不稳）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 900
        TypeText "menghunote"
        Start-Sleep -Milliseconds 1200
        Save-Shot ($Screenshot -replace '\.png$', '-count.png')
        Write-Output "searchcount ok"
    }
    "searchprogress" {
        # 不依赖日志（构建很快，日志/输出有延迟）：启动后直接连拍，覆盖整个首建窗口。
        Start-Sleep -Milliseconds 1200
        foreach ($i in 0..11) {
            Save-Shot ($Screenshot -replace '\.png$', "-f$i.png")
            Start-Sleep -Milliseconds 350
        }
        Write-Output "searchprogress ok"
    }
    "searchautorequery" {
        # 构建中先输入查询，构建完成后应**自动重查**并显示结果数（不需要再敲键）。
        if ((Wait-ForLog "打开搜索浮层" 15000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 400
        TypeText "000"
        Start-Sleep -Milliseconds 15000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchautorequery ok"
    }
    "groupfold" {
        # 折叠态截图 → 点分组**标题行**（y≈345 物理 = 230 逻辑）展开 → 展开态截图。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        Save-Shot ($Screenshot -replace '\.png$', '-collapsed.png')
        LeftClick 700 345
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-expanded.png')
        Write-Output "groupfold ok"
    }
    "groupenter" {
        # 预填查询（MENGHUNOTE_SEARCH_QUERY）→ Enter 应打开当前（第一）个分组。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        PressKey 0x0D
        if ((Wait-ForLog "打开搜索结果" 5000) -lt 0) { Fail "Enter 没有打开分组结果" }
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "打开后浮层没有关闭" }
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-opened.png')
        Write-Output "groupenter ok"
    }
    "searchcount" {
        # 总数 + 单行摘要 + 两个开关的截图（不点开关，点击不稳）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 900
        TypeText "menghunote"
        Start-Sleep -Milliseconds 1200
        Save-Shot ($Screenshot -replace '\.png$', '-count.png')
        Write-Output "searchcount ok"
    }
    "searchprogress" {
        # 首建进度：开浮层后按多个时间点连拍，总能抓到"构建中"的一帧。
        if ((Wait-ForLog "打开搜索浮层" 15000) -lt 0) { Fail "搜索浮层没有自动打开" }
        foreach ($delay in @(0, 200, 400, 700, 1100)) {
            Start-Sleep -Milliseconds $delay
            Save-Shot ($Screenshot -replace '\.png$', "-t$delay.png")
        }
        Start-Sleep -Milliseconds 4000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchprogress ok"
    }
    "searchautorequery" {
        # 构建中先输入查询，构建完成后应**自动重查**并显示结果数（不需要再敲键）。
        if ((Wait-ForLog "打开搜索浮层" 15000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 400
        TypeText "000"
        Start-Sleep -Milliseconds 15000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchautorequery ok"
    }
    "groupfold" {
        # 折叠态截图 → 点分组**标题行**（y≈345 物理 = 230 逻辑）展开 → 展开态截图。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        Save-Shot ($Screenshot -replace '\.png$', '-collapsed.png')
        LeftClick 700 345
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-expanded.png')
        Write-Output "groupfold ok"
    }
    "groupenter" {
        # 预填查询（MENGHUNOTE_SEARCH_QUERY）→ Enter 应打开当前（第一）个分组。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1500
        PressKey 0x0D
        if ((Wait-ForLog "打开搜索结果" 5000) -lt 0) { Fail "Enter 没有打开分组结果" }
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "打开后浮层没有关闭" }
        Start-Sleep -Milliseconds 500
        Save-Shot ($Screenshot -replace '\.png$', '-opened.png')
        Write-Output "groupenter ok"
    }
    "searchcount" {
        # 总数 + 单行摘要 + 两个开关的截图（不点开关，点击不稳）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 900
        TypeText "menghunote"
        Start-Sleep -Milliseconds 1200
        Save-Shot ($Screenshot -replace '\.png$', '-count.png')
        Write-Output "searchcount ok"
    }
    "searchprogress" {
        # 首次建索引进度：开浮层后立刻截一张（构建中），再过几秒截一张（就绪）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 350
        Save-Shot ($Screenshot -replace '\.png$', '-building.png')
        Start-Sleep -Milliseconds 6000
        Save-Shot ($Screenshot -replace '\.png$', '-ready.png')
        Write-Output "searchprogress ok"
    }
    "searchempty" {
        # 打开浮层后**不输入**：验证空输入框也有 caret（user 真机报的 bug）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 900
        Save-Shot ($Screenshot -replace '\.png$', '-empty.png')
        Write-Output "searchempty ok"
    }
    "searchreal" {
        # 真实索引：后台线程先对账，再查（打字用 ASCII，CJK 查询由集成测试覆盖）。
        if ((Wait-ForLog "打开搜索浮层" 8000) -lt 0) { Fail "搜索浮层没有自动打开" }
        Start-Sleep -Milliseconds 1200
        TypeText "menghunote"
        Start-Sleep -Milliseconds 1200
        Save-Shot ($Screenshot -replace '\.png$', '-hits.png')
        PressKey 0x0D              # Enter 打开第一条
        if ((Wait-ForLog "打开搜索结果" 5000) -lt 0) { Fail "Enter 没有打开选中结果" }
        if ((Wait-ForLog "关闭搜索浮层" 5000) -lt 0) { Fail "打开后浮层没有关闭" }
        Start-Sleep -Milliseconds 600
        Save-Shot ($Screenshot -replace '\.png$', '-opened.png')
        Write-Output "searchreal ok"
    }
    "delete" {
        # 笔记菜单自 task-37 起是「重命名 / 删除」两项：删除是**第二项**，
        # 菜单在 (60,40) 打开 → 表头 24 + 第一项 26 → 第二项中心 y≈103 逻辑（=154 物理）。
        RightClick 90 60
        if ((Wait-ForLog "菜单渲染" 3000) -lt 0) { Fail "右键后菜单没有渲染出来" }
        Start-Sleep -Milliseconds 120
        LeftClick 201 154
        # 应用侧的日志文案是「删除待确认」（task-33 改过），不是旧脚本里写的「确认删除」
        $ms = Wait-ForLog "删除待确认" 3000
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
