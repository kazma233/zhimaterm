# 终端字符样式与宽度探针（Windows/PowerShell 版）：SGR 属性、颜色、宽度
# 口径、制表/块/DEC 图形。跑在被测终端里肉眼对照；第 3 节的对齐表与光标
# 落点是本仓三后端口径分歧的关键判据（alacritty/termwiz 逐码点定宽，
# ghostty 簇口径，ADR-050/051/060）——在设置里切换终端核心各跑一遍即可对照。
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File term-style-probe.ps1
# 注意：本文件带 UTF-8 BOM（Windows PowerShell 5.1 依赖 BOM 才按 UTF-8 解析）。

$esc = [char]0x1b
$comb = "e$([char]0x301)"   # e + U+0301 组合尖音，验证组合符挂零宽

# 直接写控制台输出流（走 Console.OutputEncoding，不被管道格式化动过）
function W([string]$s) { [Console]::Out.Write($s) }

# 终端期望 UTF-8 字节流；ConPTY/现代终端默认即 UTF-8，这里显式钉死
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()

function Section([string]$title) { W "`n──────── $title ────────`n" }

# ---------- 1. SGR 文字属性 ----------
Section '1. SGR 文字属性'
W "$esc[1m粗体 bold$esc[0m | $esc[2m暗淡 dim$esc[0m | $esc[3m斜体 italic$esc[0m`n"
W "$esc[4m下划线 4$esc[0m | $esc[4:0m无 4:0$esc[0m | $esc[4:1m直线 4:1$esc[0m | $esc[4:2m双线 4:2$esc[0m`n"
W "$esc[4:3m波浪 4:3$esc[0m | $esc[4:4m点线 4:4$esc[0m | $esc[4:5m虚线 4:5$esc[0m`n"
# SGR 21 是已知口径分歧：alacritty 上游当 CancelBold，termwiz 后端当双下划线
# （ADR-050 §4 分歧表）
W "$esc[1m$esc[21mSGR21（alacritty=取消粗体，termwiz=双下划线）$esc[0m`n"
W "$esc[5m慢闪 5$esc[0m | $esc[7m反色 7$esc[0m | $esc[8m隐藏 8（应不可见）$esc[0m | $esc[9m删除线 9$esc[0m`n"
W "$esc[1m$esc[3m$esc[4m组合：粗 + 斜 + 下划线$esc[0m`n"
W "$esc[4:3m$esc[58;2;255;80;80m彩色波浪下划线（58;2）$esc[0m`n"

# ---------- 2. 颜色 ----------
Section '2. 颜色'
W '前景 16 色: '
30..37 + 90..97 | ForEach-Object { W "$esc[${_}m██$esc[0m" }
W "`n背景 16 色: "
40..47 + 100..107 | ForEach-Object { W "$esc[${_}m  $esc[0m" }
W "`n256 色（每格 48;5;n）:`n"
0..255 | ForEach-Object {
  if ($_ % 16 -eq 0) { W "`n" }
  W "$esc[48;5;${_}m$('{0,3}' -f $_)$esc[0m"
}
W "`n真彩渐变（48;2，红→蓝）:`n"
0..47 | ForEach-Object {
  $r = $_ * 5
  $b = 255 - $_ * 5
  W "$esc[48;2;$r;64;${b}m $esc[0m"
}
W "`n"

# ---------- 3. 宽度与对齐（关键：簇口径 vs 逐码点） ----------
Section '3. 宽度与对齐（簇口径 vs 逐码点）'
W ('{0,-8}|{1}' -f '', '0123456789'); W "`n"
W ('{0,-8}|{1}|   ← 各端一致：第 3 格' -f 'ascii', 'ab'); W "`n"
W ('{0,-8}|{1}|   ← 各端一致：第 3 格（CJK 占 2）' -f 'cjk', '中'); W "`n"
W ('{0,-8}|{1}|   ← 簇口径第 3 格；逐码点第 2 格' -f 'vs16', '⚠️'); W "`n"
W ('{0,-8}|{1}|   ← 簇口径第 3 格；逐码点第 5 格' -f 'skin', '👍🏽'); W "`n"
W ('{0,-8}|{1}|   ← 簇口径第 3 格；逐码点第 9 格' -f 'zwj', '👨‍👩‍👧‍👦'); W "`n"
W ('{0,-8}|{1}|   ← 组合符挂零宽：各端一致第 2 格' -f 'comb', $comb); W "`n"

W "`n对照表 A：ASCII / CJK / 2 宽 emoji —— 三后端都应对齐`n"
W "+----------+`n"
W "| abcdef   |`n"
W "| 中文测试 |`n"
W "| ✅❌👍🎉 |`n"
W "+----------+`n"

W "`n对照表 B：簇口径按 2 格摆的 emoji —— 簇口径后端右边框都在第 12 列；`n"
W "逐码点后端漂移（期望列：vs16/keycap=7、skin=19、zwj=17）`n"
W "+----------+`n"
W "| ⚠️⚠️⚠️⚠️ |`n"
W "| 1️⃣1️⃣1️⃣1️⃣ |`n"
W "| 👍🏽👍🏽👍🏽👍🏽 |`n"
W "| 👨‍👩‍👧‍👦       |`n"
W "+----------+`n"

# ---------- 4. 制表符、块元素与 DEC 图形 ----------
Section '4. 制表符、块元素与 DEC 图形'
W "┌─────┬─────┐`n"
W "│ A   │ 中  │`n"
W "├─────┼─────┤`n"
W "│ abc │ 你  │`n"
W "└─────┴─────┘`n"
W "块元素（ADR-034 几何绘制路径）: ▀▄█▌▐░▒▓▔▁▏`n"
W "横竖虚线: ─━┄┅┆┇┈┉┊┋╌╍╎╏═║╔╗╚╝`n"
W "DEC 特殊图形（ESC(0，逐行重置）:`n"
W "$esc(0lqqqk$esc(B`n"
W "$esc(0x   x$esc(B`n"
W "$esc(0mqqj$esc(B`n"
