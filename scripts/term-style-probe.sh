#!/usr/bin/env bash
# 终端字符样式与宽度探针：SGR 属性、颜色、宽度口径、制表/块/DEC 图形。
# 跑在被测终端里肉眼对照；第 3 节的对齐表与光标落点是本仓三后端口径
# 分歧的关键判据（alacritty/termwiz 逐码点定宽，ghostty 簇口径，
# ADR-050/051/060）——在设置里切换终端核心各跑一遍即可对照。
#
# 用法：bash term-style-probe.sh
# 约束：文件须以 UTF-8 保存；ESC 一律用八进制 \033，兼容 macOS 自带 bash 3.2
# （其 printf 不支持 \u 转义）。

set -u

section() { printf '\n──────── %s ────────\n' "$1"; }

# ---------- 1. SGR 文字属性 ----------
section "1. SGR 文字属性"
printf '\033[1m粗体 bold\033[0m | \033[2m暗淡 dim\033[0m | \033[3m斜体 italic\033[0m\n'
printf '\033[4m下划线 4\033[0m | \033[4:0m无 4:0\033[0m | \033[4:1m直线 4:1\033[0m | \033[4:2m双线 4:2\033[0m\n'
printf '\033[4:3m波浪 4:3\033[0m | \033[4:4m点线 4:4\033[0m | \033[4:5m虚线 4:5\033[0m\n'
# SGR 21 是已知口径分歧：alacritty 上游当 CancelBold，termwiz 后端当双下划线
# （ADR-050 §4 分歧表）
printf '\033[1m\033[21mSGR21（alacritty=取消粗体，termwiz=双下划线）\033[0m\n'
printf '\033[5m慢闪 5\033[0m | \033[7m反色 7\033[0m | \033[8m隐藏 8（应不可见）\033[0m | \033[9m删除线 9\033[0m\n'
printf '\033[1m\033[3m\033[4m组合：粗 + 斜 + 下划线\033[0m\n'
printf '\033[4:3m\033[58;2;255;80;80m彩色波浪下划线（58;2）\033[0m\n'

# ---------- 2. 颜色 ----------
section "2. 颜色"
printf '前景 16 色: '
for code in 30 31 32 33 34 35 36 37 90 91 92 93 94 95 96 97; do
  printf '\033[%dm██\033[0m' "$code"
done
printf '\n背景 16 色: '
for code in 40 41 42 43 44 45 46 47 100 101 102 103 104 105 106 107; do
  printf '\033[%dm  \033[0m' "$code"
done
printf '\n256 色（每格 48;5;n）:\n'
for n in $(seq 0 255); do
  [ $((n % 16)) -eq 0 ] && printf '\n'
  printf '\033[48;5;%dm%3d\033[0m' "$n" "$n"
done
printf '\n真彩渐变（48;2，红→蓝）:\n'
for i in $(seq 0 47); do
  printf '\033[48;2;%d;64;%dm \033[0m' "$((i * 5))" "$((255 - i * 5))"
done
printf '\n'

# ---------- 3. 宽度与对齐（关键：簇口径 vs 逐码点） ----------
section "3. 宽度与对齐（簇口径 vs 逐码点）"
printf '%-8s|%s\n' '' '0123456789'
printf '%-8s|%s|   ← 各端一致：第 3 格\n' 'ascii' 'ab'
printf '%-8s|%s|   ← 各端一致：第 3 格（CJK 占 2）\n' 'cjk' '中'
printf '%-8s|%s|   ← 簇口径第 3 格；逐码点第 2 格\n' 'vs16' '⚠️'
printf '%-8s|%s|   ← 簇口径第 3 格；逐码点第 5 格\n' 'skin' '👍🏽'
printf '%-8s|%s|   ← 簇口径第 3 格；逐码点第 9 格\n' 'zwj' '👨‍👩‍👧‍👦'
printf '%-8s|e\0314\0201|   ← 组合符挂零宽：各端一致第 2 格\n' 'comb'

printf '\n对照表 A：ASCII / CJK / 2 宽 emoji —— 三后端都应对齐\n'
printf '+----------+\n'
printf '| abcdef   |\n'
printf '| 中文测试 |\n'
printf '| ✅❌👍🎉 |\n'
printf '+----------+\n'

printf '\n对照表 B：簇口径按 2 格摆的 emoji —— 簇口径后端右边框都在第 12 列；\n'
printf '逐码点后端漂移（期望列：vs16/keycap=7、skin=19、zwj=17）\n'
printf '+----------+\n'
printf '| ⚠️⚠️⚠️⚠️ |\n'
printf '| 1️⃣1️⃣1️⃣1️⃣ |\n'
printf '| 👍🏽👍🏽👍🏽👍🏽 |\n'
printf '| 👨‍👩‍👧‍👦       |\n'
printf '+----------+\n'

# ---------- 4. 制表符、块元素与 DEC 图形 ----------
section "4. 制表符、块元素与 DEC 图形"
printf '┌─────┬─────┐\n'
printf '│ A   │ 中  │\n'
printf '├─────┼─────┤\n'
printf '│ abc │ 你  │\n'
printf '└─────┴─────┘\n'
printf '块元素（ADR-034 几何绘制路径）: ▀▄█▌▐░▒▓▔▁▏\n'
printf '横竖虚线: ─━┄┅┆┇┈┉┊┋╌╍╎╏═║╔╗╚╝\n'
printf 'DEC 特殊图形（ESC(0，逐行重置）:\n'
printf '\033(0lqqqk\033(B\n'
printf '\033(0x   x\033(B\n'
printf '\033(0mqqj\033(B\n'
