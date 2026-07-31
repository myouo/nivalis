# Nivalis Terminal Design QA

审查日期：2026-07-29

## Source of truth

- 选定方向：ImageGen 第三个方案
- 参考图：`.audit/source-option-3-1440x900.png`
- 实现截图：`.audit/implementation-option-3-final-960-qa.png`
- 移动端截图：`.audit/implementation-option-3-final-960-mobile.png`
- 完整并排对比：`.audit/comparison-option-3-final-960.png`
- 核心区域对比：`.audit/comparison-option-3-final-960-focus.png`
- 对比视口：1440 × 900 CSS px，截图密度 1×
- 对比状态：`nivalis` 主题、欢迎信息、`hits` 输出和活动提示符

## Fidelity surfaces

- Typography：等宽终端字体、居中的窗口标题、清晰但克制的字重层级。
- Spacing：标题栏 46px；桌面终端最大宽度 960px 并水平居中；内容区保持稳定留白。
- Color：深靛蓝页面背景、冷色深蓝终端、石墨灰半透明标题栏和冰色语义色。
- Assets：保留 macOS 风格三色窗口控制点，不引入桌面、Dock 或装饰性图形。
- Copy：`guest@nivalis`、欢迎文案、`hits` 命令和 Unix 风格提示符与选定方向一致。

## Comparison history

1. Pass 1
   - 问题：窗口边界、标题栏比例、正文尺寸和上下留白与选定方向不够接近。
   - 修复：重做框架尺寸、标题栏网格、字体与正文间距。
2. Pass 2
   - 问题：整体方向已接近，但仍需服从用户最终交互要求。
   - 修复：移除输入行背景、边框和阴影；保持普通终端输入行。
3. Final
   - 用户覆盖：桌面终端固定为最大 960px 并居中，移动端保持 8px 安全边距。
   - 用户覆盖：目录输出统一使用 700 字重。
   - 这些覆盖相对参考图是有意差异，不作为 fidelity 缺陷。

## Verification

- 1440 × 900：窗口实测 `x=240`、`width=960`、中心偏差 `0px`。
- 390 × 844：窗口实测 `x=8`、`width=374`，页面横向溢出 `0px`。
- 输入行：透明背景、`0px` 边框、无阴影。
- `ls -la` 与 `tree`：目录元素计算字重均为 `700`。
- `github`：输出 `https://github.com/myouo`，链接在新标签页打开并使用 `noopener noreferrer`。
- `sites`：列出 3 个 Nivalis 子域名；390px 视口下终端与链接均无横向溢出。
- 命令解析：严格区分大小写；`ls` 可执行，`LS` 与 `GitHub` 返回 `command not found`，Tab 补全不会修正大小写。
- `cd` 补全：首次 Tab 在输入行下方列出有效目录并选中第一项；Tab / Shift+Tab 可正反向循环，输入、Escape、执行命令或历史导航会关闭列表；普通文件不会进入候选。
- 滚轮：长输出中 `deltaY=-520` 后 `scrollTop` 精确减少 `520px`，事件已被终端滚动区接管。
- 主题：`nivalis` 使用冰色主题；原 `nivalis` 配色以 `noir` 名称提供；`frost` 不再是有效主题。
- 访问计数：页面加载触发 `POST /api/hits`；`hits` 触发 `GET /api/hits`，本地均返回 200。
- 浏览器控制台：无 error、warning 或 issue。
- 工程检查：生产构建和 TypeScript 检查通过。

## Final result

passed
