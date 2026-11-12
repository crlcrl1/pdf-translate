pdf-translate-menu-translate = 翻译 PDF
pdf-translate-menu-check = PDF Translate：检查命令

reader-button = 翻译
reader-button-title = 使用 pdf-translate 翻译此 PDF

pref-binary-heading = pdf-translate 命令
pref-binary = 可执行文件
pref-binary-input =
    .placeholder = 留空则从 PATH 查找
pref-detect = 自动检测
pref-test = 测试
pref-open-data = 打开数据目录

pref-translation-heading = 翻译
pref-target-lang = 目标语言
pref-pages = 页码范围
pref-mode = 合成模式
pref-mode-clean = Clean（替换原文字形）
pref-mode-overlay = Overlay（覆盖原文）
pref-translate-references = 翻译参考文献列表
pref-keep-annotations = 保留链接注释
pref-fake = 假翻译模式（不调用 API，给每段加 [译]）
pref-extra-args = 额外参数
pref-extra-args-hint = 会追加到 `pdf-translate translate` 命令后；支持引号和反斜杠。
pref-config = 配置文件
pref-config-input =
    .placeholder = 默认：~/.config/pdf-translate/config.toml

pref-output-heading = 输出
pref-bilingual = 生成原文/译文并排的双语 PDF（左原文，右译文）
pref-import = 将译文 PDF 作为附件导入文献库
pref-open-when-done = 完成后打开译文 PDF

status-checking = 正在检查…
status-ok = 可用：{ $version }
status-missing = 未找到 pdf-translate

env-ok = pdf-translate 可用。
    { $version }
    { $path }
env-missing = pdf-translate 不可用：{ $detail }

error-no-pdf = 请选择一个 PDF 附件，或选择一个包含 PDF 的条目。
error-no-file = 找不到 PDF 文件，链接文件可能已不可用。
error-already-running = 这个 PDF 正在翻译中。
error-binary-missing = 配置的 pdf-translate 可执行文件不存在：{ $path }
error-binary-not-found = 未找到 pdf-translate。请先构建并加入 PATH，或在设置中填写可执行文件路径。
error-version = { $path } 的 pdf-translate 无法报告版本。
error-spawn = 无法启动 pdf-translate：{ $detail }
error-no-output = pdf-translate 已结束，但没有写出译文 PDF。
error-failed = 翻译失败：{ $detail }
error-failed-with-log = 翻译失败：{ $detail }
    日志：{ $log }
error-unknown = 未知错误

progress-headline = PDF Translate
progress-binary = 正在定位 pdf-translate…
progress-cached = 使用缓存译文…
progress-starting = 正在启动…
progress-stage-layout = 正在分析版面…
progress-stage-translate = 正在翻译…
progress-stage-translation = 翻译完成…
progress-stage-typeset = 正在排版…
progress-stage-write = 正在写入 PDF…
progress-stage-error = 错误
progress-stage-log = 正在处理…
progress-finished = 正在导入译文 PDF…
progress-done = 完成：{ $title }

attachment-title = 翻译（{ $lang }）
attachment-title-bilingual = 双语对照（{ $lang }）
untitled = 未命名 PDF
