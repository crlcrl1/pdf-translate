# pdf-translate 设计与实现方案

> 目标：用 Rust 写一个单文件可执行程序，翻译 PDF 论文时保留原版面、图片、表格和公式，只把正文换成译文。使用体验参考
> [zotero-paper-translator](https://github.com/daniel5u/zotero-paper-translator)（底层是 BabelDOC）。

本文写于项目起步阶段，是一份工程方案，不是 API 文档。Stage 0–2 的部分内容后来已经落地，个别选型在实现时改过；文中的目录结构和阶段划分反映设计意图，实际代码以 `src/` 和 README 为准。

默认场景：

- 输入以数字版英文论文为主（arXiv、IEEE、ACM、Springer、Nature 等）。
- 输出单语译文 PDF，版面、图、表、公式保持原位；也可用 `--bilingual` 输出原文/译文左右并排的双语 PDF。
- 翻译走任意 OpenAI 兼容接口（OpenAI、DeepSeek、GLM、Qwen、Ollama、vLLM 等）。
- 运行时只有一个二进制，不依赖 Python、uv、外部脚本或系统 PDF 工具。
- 第一阶段做 CLI 和核心引擎；GUI、Zotero 插件等核心稳定后再接。

---

## 1. 先明确难点

把一段文字换成译文，本身不难。难的是 PDF 的表示方式：它不是流式文档，而是一组绘制指令。

- 一段文字可能拆在多个 text object、多种字体和多个 Form XObject 里。
- 文字位置由 `Tm`/`Td`/`TJ` 和字体宽度共同决定，光看字符不够。
- 公式可能是普通文字加数学字体，也可能是 Type3 字体、矢量路径或图片。
- 图片、表格线、背景、水印和正文共用同一个 content stream。
- 直接删文字，很容易连图上的标注、公式和页眉页脚一起删掉；改成盖白块，又会把图形挡住。
- 中文的换行、字体子集、字宽和行距都与英文差别很大。

所以这个项目真正要解决的是下面这件事：

> **保留原页面图元和未翻译的文字，只删除确实被替换掉的原文字形，再把译文画上去；行内公式走独立矢量路径，不把整页渲染成快照。**

BabelDOC 的“解析 → 清理原文 → 重排版 → 生成 PDF”也是这个思路，区别在于这里的实现要适合纯 Rust。

---

## 2. 总体架构

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              pdf-translate                              │
├───────────┬───────────┬──────────────┬───────────┬───────────┬──────────┤
│ ingest    │ extract   │ layout       │ translate │ typeset   │ compose  │
├───────────┼───────────┼──────────────┼───────────┼───────────┼──────────┤
│ 页面树    │ 字形/图片 │ 段落/栏/公式 │ LLM/缓存  │ 行宽/字号 │ 清空文字 │
│ 加密/旋转 │ 字体/颜色 │ 阅读顺序     │ 术语表    │ 公式占位  │ 贴回公式 │
│ 扫描检测  │           │ 表格/页眉    │ 校验      │ 字体子集  │ 写字体   │
└───────────┴───────────┴──────────────┴───────────┴───────────┴──────────┘
      │
      └──► JSON 进度事件 ──► GUI / Zotero 插件 / 本地 HTTP 服务
```

流水线是单向的：先读页面树，提取字形、字体、颜色、图片和裁剪信息；再把字形拼成行和段落，判断分栏、公式、页眉页脚；然后逐段翻译；最后排版并写回页面。GUI、Zotero 插件和本地服务只通过 JSON 进度事件与核心交互，不直接碰 PDF 处理逻辑。

最初计划把代码拆成 library 加多个 bin target，发布时仍然只给一个主二进制。目标目录结构如下：

```
src/
  main.rs                 CLI 入口
  cli.rs                  clap 参数
  config.rs               TOML + 环境变量
  pdf/
    parse.rs              hayro 页面树、页面框、旋转、元数据
    extract.rs            自定义 Device：收集字形、图片、路径、颜色
    content.rs            content stream 解析、去文字、重编码
    rewrite.rs            按字形改写内容流、公式独立路径、旧对象清理
    fonts.rs              字体识别、子集化、Type0/CIDFont 嵌入
    writer.rs             页面内容、资源字典、双语页面
    render.rs             页面渲染（预览、背景采样、OCR）
  layout/
    lines.rs              glyph → span → line
    columns.rs            XY-cut、分栏、阅读顺序
    paragraphs.rs         行合并成段落、断段规则
    classify.rs           标题/正文/图/表/公式/页眉页脚/参考文献
    model.rs              可选 DocLayout-YOLO（ONNX）
  translate/
    openai.rs             OpenAI 兼容客户端
    prompt.rs             提示词、占位符规则
    cache.rs              译文缓存
    glossary.rs           术语表
    jobs.rs               并发、重试、限流、进度
  typeset/
    measure.rs            字体度量、字形宽度
    wrap.rs               CJK/Latin 断行、标点规则
    fit.rs                字号/行距/字距拟合
    draw.rs               输出 text operators
  compose/
    clean.rs              “选择删除原文 + 保留原图元 + 重绘译文”
    overlay.rs            扫描件/兜底：背景块覆盖 + 重绘
    dual.rs               双语/对照页
  server.rs               本地 HTTP API + SSE 进度
  gui/                    可选 eframe/egui
  model/                  可选布局/OCR 模型封装
```

（当前代码的模块比这里少，实际划分以 `src/` 为准。）

---

## 3. 技术选型

### 3.1 PDF 读取、渲染、文字提取

读取和渲染选 **hayro**（`hayro`、`hayro-syntax`、`hayro-interpret`、`hayro-font`）。它是纯 Rust，MIT/Apache 双许可，交叉编译省事；渲染质量在纯 Rust 引擎里也够用，预览、背景采样和扫描检测都能胜任。

`hayro-interpret` 的 `Device` trait 可以拿到：

- 每个字形的 glyph ID、Unicode、字符码，以及字体元数据（PostScript 名、粗体、斜体、衬线、等宽）；
- 字形的变换矩阵和轮廓，用来算精确 bbox；
- 图片绘制、路径、裁剪、颜色和透明度；
- `DrawMode::Invisible`，可以处理 OCR 隐藏文字层。

风险也要提前说清楚：hayro 的 API 还年轻（0.9.x），只能读不能改，文字收集需要自己实现一个 `TextCollector` Device。版本要锁住，回归测试要跟上。

备选方案：

- `pdfium-render` 加静态链接的 PDFium。文字提取最成熟，但要自己构建 PDFium，跨平台发布麻烦，二进制也大。适合做成可选的 fallback feature，不适合当第一版主路径。
- MuPDF。AGPL，C 依赖也重；除非项目本身接受 AGPL，否则不考虑。

所以默认走纯 Rust。如果语料测试发现某些字体或编码提取不准，再加一个只负责文字提取的 `pdfium` feature。

### 3.2 PDF 编辑与写出

编辑和写出用 **lopdf**（MIT）。它可以直接加载原文档、改对象、读写 content stream 和资源字典、处理 xref，适合“在原文档上做增量修改”。

需要从零构造对象，或者写字体、内容流时，可以配合 **pdf-writer**；两者混用要小心对象编号。大部分修改直接操作 lopdf 的字典和流就够了，字体嵌入则用 `subsetter` 裁子集，再手写 Type0 字体对象。

### 3.3 字体与排版

- `subsetter`：按实际用字裁剪 OpenType 字体，支持 TrueType 和 CFF。
- `ttf-parser` 或 `skrifa`：读 cmap、字形 advance、bbox 和字体度量。
- `rustybuzz`：需要精细的拉丁文 shaping/kerning 时再引入。中文不依赖复杂 shaping，第一版可以先不做。
- `unicode-linebreak` 或 `icu_segmenter`：提供断行机会。中文标点的行首/行尾禁则需要另外实现，不能只靠 Unicode 断行表。

内置字体用：

- 正文：Noto Serif CJK SC 或 Source Han Serif SC（OFL）；
- 标题和无衬线：Noto Sans CJK SC 或 Source Han Sans SC（OFL）；
- 等宽：Noto Sans Mono CJK SC，或者保留原字体。

运行时按文档实际用字做子集，输出 PDF 通常只带几千个字，增加 1–3 MB，而不是把 15–25 MB 的整个字体塞进去。源字体可以用 `include_bytes!` 编译进二进制，也可以用 feature 控制是否嵌入。

### 3.4 布局分析

第一版用几何启发式，不急着上模型：

- 行聚类：同一基线、同一旋转角、间距小于阈值、字号接近。
- 段落合并：水平重叠、行距、缩进、字号和字重变化、项目符号。
- 分栏与阅读顺序：递归 XY-cut，先按竖向空白分栏，再按横向空白分段。
- 页眉页脚：页面上下边缘，加上跨页重复的文本。
- 图表和公式：字体名、字符分布、图片 bbox、线框和对齐特征。

如果启发式在复杂版面上不够用，第二版可以接 **DocLayout-YOLO**（ONNX）补图表、表格和公式区域：

- 纯 Rust 推理用 `tract-onnx`。不要默认依赖 ONNX Runtime 动态库，否则“单二进制”就破了。
- 模型大约 20–40 MB，做成 feature，或者首次运行时下载并校验 SHA-256；如果坚持严格单文件发行，再考虑嵌入。
- 许可证要留意：DocLayout-YOLO 是 AGPL-3.0，BabelDOC 也是。项目要保持 MIT 的话，不能直接移植它们的代码，也不能随意分发模型。

### 3.5 翻译

HTTP 客户端用 `reqwest` + `rustls`，避免 OpenSSL 动态库。协议只依赖 OpenAI 兼容的 `/chat/completions`，`base_url`、`api_key`、`model` 三项可配。

并发最初考虑 tokio，实际实现改成了 blocking `reqwest` + `rayon`：翻译是 IO 等待型任务，线程池已经够用，还能省掉一个 async 运行时。请求结果用 `serde`/`serde_json` 解析；结构化输出优先让模型返回 JSON，接口不支持时再用分隔符加解析兜底。

限流和重试不复杂：`governor` 可以用，自己写一个 token bucket 也够；429/5xx 做指数退避，超长和截断单独检测。缓存可以用 `redb` 这样的嵌入式 KV，也可以直接落文件；当前实现是一个 JSON 文件，key 由 endpoint、model、prompt 版本、目标语言和原文哈希组成。以后接入术语表，也要把术语表算进 key。

术语表用 CSV 或 TOML，命中术语后注入提示词；自动术语抽取可以后面再加一轮 LLM。公式不发给模型，先用占位符替换，模型只翻译自然语言。

### 3.6 CLI / GUI / 服务

- CLI 用 `clap`。
- 日志和进度用 `tracing`。需要给 Zotero 插件解析时，输出 JSON lines。
- GUI 计划用 `eframe`/`egui` + `egui_extras`：页面缩略图、bbox 高亮、段落右键翻译、进度条。单二进制，不依赖 WebView。
- 本地服务用 `axum` + SSE，给 Zotero 插件或未来的 Web UI 用。
- 文件对话框用 `rfd`（Linux 走 xdg-desktop-portal）或 `egui-file-dialog`。

### 3.7 其他

图片处理用 `image`/`png`，主要服务于预览、扫描件背景采样和可选的 OCR。哈希用 `sha2`，缓存 key 和模型校验都靠它。错误处理分两层：库代码用 `thiserror`，CLI 层用 `anyhow`。发布用 `cargo-dist`；Linux CLI 可以做 musl 静态构建，GUI 仍依赖系统的 X11/Wayland/GL 动态库，但不需要额外脚本。

---

## 4. 核心数据模型

坐标统一用 **PDF 用户空间**：单位是 point，原点在页面左下，y 向上，不应用页面旋转和 CropBox 偏移。这样提取出的坐标可以直接写回 content stream，页面 `/Rotate` 交给阅读器处理。

下面是概念模型，不是最终代码：

```rust
use kurbo::Rect;

pub struct Document {
    pub pages: Vec<Page>,
    pub metadata: Metadata,
    pub source_hash: [u8; 32],
}

pub struct Page {
    pub index: u32,
    pub media_box: Rect,
    pub crop_box: Rect,
    pub rotation: i32,          // 0/90/180/270
    pub blocks: Vec<Block>,     // 按阅读顺序
    pub images: Vec<ImageBox>,
    pub scanned: bool,
}

pub enum BlockKind {
    Title,
    Heading { level: u8 },
    Paragraph,
    Formula { display: bool },
    Figure,
    Table,
    Caption,
    Header,
    Footer,
    PageNumber,
    Reference,
    Unknown,
}

pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    pub bbox: Rect,
    pub lines: Vec<Line>,
    pub style: TextStyle,
    pub protected: bool,        // true = 不翻译，原位保留
}

pub struct Line {
    pub bbox: Rect,
    pub baseline: f32,
    pub spans: Vec<Span>,
}

pub struct Span {
    pub text: String,
    pub bbox: Rect,
    pub font: FontMeta,
    pub size: f32,
    pub color: Rgba,
    pub formula: bool,
    pub glyphs: Vec<GlyphBox>,
}

pub struct FontMeta {
    pub postscript_name: Option<String>,
    pub serif: bool,
    pub italic: bool,
    pub bold: bool,
    pub monospace: bool,
}

pub struct GlyphBox {
    pub ch: Option<char>,
    pub bbox: Rect,
    pub advance: f32,
    pub char_code: u32,
    pub glyph_id: u32,
}

pub struct Translation {
    pub source_hash: [u8; 32],
    pub text: String,                    // 含 <f1> 占位符
    pub formulas: Vec<FormulaRef>,
    pub status: TranslationStatus,
}
```

`Block` 是翻译和排版的基本单位；`Line`/`Span` 保留原文几何，用来计算段落盒子、公式位置和样式。

---

## 5. 分阶段实现

### Stage 0：地基（约 1 周）

先把工程骨架搭起来：Cargo 单 crate 多模块（不建 workspace），`clap` 参数，`tracing` 日志。然后用 hayro 加载 PDF，遍历页面，把页面尺寸、旋转、字体、文字行和图片 bbox 导出成 JSON。

测试语料要在这时准备好：arXiv 单栏和双栏、IEEE、ACM、Springer、Nature、中文论文、扫描件各若干，记录来源和预期结果。最后做一个调试渲染，把提取到的行、段、公式 bbox 画到页面预览 PNG 上，用肉眼检查。

验收：拿 10 篇论文，文字行 bbox 基本贴合，能看出分栏和段落。

### Stage 1：布局与端到端最小闭环（2–3 周）

这一阶段的目标是让整条链路先跑通：

1. `lines.rs`：glyph → line/span，处理字距、连字、上下标和旋转文字。
2. `columns.rs`：XY-cut 分栏和阅读顺序，先支持单栏、双栏。
3. `paragraphs.rs`：行合并、断段、去连字符。
4. `classify.rs`：按字号和字重识别标题，按位置和跨页重复识别页眉页脚，识别图表 caption，公式先做粗筛。
5. `translate/openai.rs`：段落翻译、缓存、并发、重试和占位符校验。
6. `typeset` 最小版：按原段落宽度断行、字号拟合、嵌入 CJK 字体子集。
7. `compose/overlay.rs`：先用“背景色块覆盖原文 + 重绘译文”跑通，不碰 content stream 删除。这个模式也适合扫描件。

验收：单栏论文输出可读，双栏不串栏，图片和公式不被覆盖，中文可以搜索和复制。

### Stage 2：真正的“保留版式”合成（3–4 周，核心）

这一阶段解决 clean 模式的核心问题：

1. `pdf/content.rs`：用 lopdf 解析内容流，保留图元、状态、文字裁剪和 inline image。
2. `pdf/rewrite.rs`：通过内存探针解释，按字形删除译文对应的原文；共享 Form 按调用分别改写；输出前清理旧对象。
3. `compose/clean.rs`：保留其余内容，插入独立的公式路径和译文，不生成整页快照。
4. `classify/formula`：数学字体、字符模式和行内公式占位符；display 公式整块保护。
5. `typeset/fit.rs`：字号、行距、字距联合拟合；段落盒子可以向空白处扩展，但不能越过栏间距和受保护区域；溢出要告警。
6. 双栏论文、跨栏标题、摘要、图注和表格保护。
7. `--parse-only` / `--no-translate` 重建模式：不翻译，只验证“文字位置恢复 + 保留图元”后的页面是否与原页面接近。

验收：

- 对测试集，重建页面与原页面在 150 DPI 下 SSIM > 0.99，只忽略抗锯齿差异。
- 译文不压图、不压公式、不串栏；需要缩到 0.7 倍以下或溢出的段落少于 2%。
- 输出文字可以复制和搜索，字体子集嵌入正确。

### Stage 3：质量与工程化（2–3 周）

1. 双语输出：原文页和译文页并排或交替，两页都作为 Form XObject 组合。（已实现第一版：`--bilingual` 将原文/译文页左右并排。）
2. 选择翻译范围：`--pages`、正文或全部、跳过参考文献和附录。
3. 断点续传：每段译文落盘，失败重跑不重复请求；会话状态写进 `session.json`。
4. 术语表、自动术语抽取，以及标题、章节、前后段之类的上下文。
5. 布局模型可选接入；表格翻译做实验，默认仍然保护。
6. 性能：页面级并行、翻译并发、缓存和内存上限。
7. 测试：round-trip、字体、断行、content stream fuzz。

### Stage 4：APP / Zotero（2–3 周）

- `eframe` GUI：拖入 PDF、页面预览、bbox 高亮、段落右键翻译、进度和导出。
- `pdf-translate serve`：本地 HTTP + SSE，给 Zotero 插件调用。
- Zotero 插件（JS）：
    - 启动或连接本地服务，打开 PDF 后后台解析；
    - 在 PDF.js 阅读器上叠加可点击的段落框；
    - 右键段落，调用 `POST /sessions/{id}/paragraphs/{pid}/translate`；
    - 译文写回同一页，或导出整本译文 PDF。
- 当前进度：可选 `eframe` GUI 已支持拖入 PDF、整篇翻译、结构化进度、取消、原文/译文预览、结果导出和系统/浅色/深色主题；段落 bbox 高亮与单段翻译尚未实现。插件（`zotero/`）直接调用 CLI 做整篇翻译、显示进度，并导入单独的双语对照 PDF 子附件；再次点击时优先打开已有双语附件或缓存，不重复翻译。本地服务、段落框和段落级翻译尚未实现。
- 发布：GitHub Releases 提供多平台二进制；Zotero XPI 里内置对应平台的二进制，或者首次使用时下载。

### Stage 5（可选）：扫描件 OCR（3–4 周）

- 检测：页面文字 glyph 很少，且有大图覆盖，就当作扫描页。
- 渲染 300 DPI，然后做文本检测和识别：
    - 纯 Rust 的 `ocrs` 目前只支持拉丁字母，适合英文扫描件；
    - 中文和多语言用 PP-OCR ONNX + `tract`（或静态 `ort`），模型可以嵌入，也可以首次下载；
    - 版面分析复用 DocLayout-YOLO。
- OCR 模式不做“删除文字”，直接走 overlay：背景采样覆盖原文区域，再重绘译文；公式区域保持像素。
- 扫描件里的公式通常当图片保留，不做公式识别。

---

## 6. 关键算法

### 6.1 文字行重建

对每个字形：

1. 用 `page_transform * glyph.transform()` 得到页面坐标；
2. 用字形轮廓 bbox，或者 advance × 字号算 bbox；
3. 按下面的条件聚类成行：
    - 旋转角量化后相同；
    - 基线 y 差小于 0.2 × 字号；
    - 相邻字形水平间距小于 0.35 × 字号（拉丁）或 0.5 em（中文）；
    - 字号差小于 10%。
4. 行内按 x 排序，合并 span；间距大于阈值且没有空格字符时，补一个空格。
5. 行按 y 降序、x 升序排列。

几个细节：

- 连字（ﬁ → fi）按 ToUnicode 展开；
- 上标和下标要保留垂直偏移，公式检测会用到；
- 旋转文字（侧边栏、图注）第一版标记为 protected，不翻译。

### 6.2 分栏与阅读顺序

用递归 XY-cut：

```
cut_region(region):
  找竖向空白带（在 x 投影上连续为空，宽度 > 3% 页宽，覆盖 > 60% 高度）
  如果有：按空白带切成左右栏，递归
  否则找横向空白带（高度 > 2% 页高），切成上下块，递归
  否则：region 是一个 block，按行排序
```

实际 PDF 里，空白带经常被标题、跨栏图打断，所以还要补几条规则：

- 先用整页或大区域切分，再对每个子区域切；
- 跨栏 block（标题、摘要、大图）按 bbox 宽度判断，单独处理；
- 阅读顺序：栏内自上而下，栏间从左到右；跨栏元素按 y 插入。

### 6.3 段落合并

相邻两行属于同一段，通常要满足下面这些条件中的大部分：

- 水平重叠大于 50%，或左边界差小于 1.5 × 字号；
- 垂直间距小于 0.7 × 行高（双倍行距可以放宽）；
- 字号差小于 15%，字重和斜体一致；
- 不是标题、列表项或图注；
- 行尾不是句号加一个大间距（那可能是段末）。

断段信号：

- 首行缩进，或者左右边界变化；
- 上一行明显短于栏宽，下一行又顶格；
- 字号或字重变化；
- 项目符号或编号；
- 很大的垂直间距；
- 栏切换或页面切换。

去连字符：行尾是 `-`，下一行以小写字母开头，就合并；专有名词或大写开头的情况保守处理，宁可不合。

### 6.4 公式识别

第一版只做规则，不追求 100% 准确。

**字体特征**（置信度最高）：

- PostScript 名包含 `CMMI`、`CMSY`、`CMEX`、`MSAM`、`MSBM`、`STIX`、`XITS`、`LatinModernMath`、`TeXGyre`、`Asana`、`Libertinus`、`Math`、`Symbol`、`MT` 等；
- 字体名里带 `Math` 或 `Symbol`；
- Type3 字体，很多老论文用它排公式。

**字符特征**：

- 希腊字母、数学运算符、积分/求和/箭头/集合符号占比高；
- 上标下标、斜体单字母、非 ASCII 符号密集；
- 一行里大部分 glyph 来自数学字体。

**布局特征**：

- 居中、单独成行、垂直高度大于正文行；
- 与正文行有明显上下间距；
- 行内公式通常夹在文字 span 之间，字体切换很明显。

处理策略：

- **display 公式**：整块标记为 protected，不翻译，原位保留；段落上下文里留一个占位符，让模型知道这里有个公式。
- **inline 公式**：span 级别标 `formula=true`，翻译时替换成 `<f1>`、`<f2>`；排版时当作固定尺寸的 inline box，公式的独立路径跟着占位符移动。
- **判断不了**：保守保护整个 line 或 block，只翻译周围文字。宁可少翻，也别把公式翻坏。
- 提供 `formula_font_pattern`、`formula_char_pattern` 两个配置，方便针对特定出版社调整。

### 6.5 翻译提示词与占位符

当前实现是逐段请求，一次翻一段。如果以后要按 section 批量请求，请求结构可以设计成：

```json
{
  "paper_title": "...",
  "section": "3.2 Method",
  "target_language": "zh-CN",
  "glossary": [
    {
      "source": "transformer",
      "target": "变换器"
    }
  ],
  "paragraphs": [
    {
      "id": "p12",
      "text": "We propose <f1> to ..."
    },
    {
      "id": "p13",
      "text": "..."
    }
  ]
}
```

要求模型返回：

```json
{
  "translations": [
    {
      "id": "p12",
      "text": "我们提出 <f1> 来……"
    },
    {
      "id": "p13",
      "text": "……"
    }
  ]
}
```

规则：

- 只翻译自然语言，不翻译公式占位符、引用编号、URL、代码和变量名；
- 占位符原样保留，数量和顺序都要一致；
- 不加解释，不合并段落，不省略句子；
- 术语表优先；
- 译文长度尽量接近原文，方便排版；
- 输出被截断（`finish_reason=length`）时，提高 `max_tokens` 重试，或者拆分段落；
- 占位符校验失败时重试一次，再失败就跳过该段，保留可见原文并记录。

上下文用标题、当前章节和前后各一段就够了。不要把整篇塞进一个请求，容易串段，也容易截断。按 section 批量时可以每段带独立 id。

### 6.6 排版拟合

对每个待翻译段落：

```
box = 原段落 bbox
box.width = 栏宽或原段落最大行宽（取较宽者，但不跨栏）
box.height = 到下一个 block 的可用高度（可向空白扩展，上限 1.5×原高）
protected_boxes = 公式/图/表等禁止覆盖区域

for scale in 1.00, 0.98, ..., 0.70:
    size = original_size * scale
    for leading in 1.05..1.45:
        lines = break_lines(text, font, size, box.width, formula_atoms)
        if lines.height <= box.height and no collision:
            choose
            break
if none:
    use scale=0.70, leading=1.05
    if still overflow: 记录警告并保留原段落，不绘制超出可用区域的译文
```

细节：

- 先试原字号。中文比英文“方”，不要按字符数估算，要实际测量。
- 行距用 `leading / font_size` 表示，正文常见 1.2–1.35。
- 字距：中文可以在 `-0.02em..+0.05em` 之间微调；英文调 word spacing。
- 对齐：两端对齐时，中文在字间分配余量（避开标点），英文在词间分配；最后一行左对齐。
- 首行缩进：中文 2em；原文是西文缩进的话，也可以沿用原样式。
- 行内公式作为不可拆分的 inline box，断行时允许在它前后断开；绘制时把公式路径平移到对应位置。
- 旋转文字第一版不翻译；后续用旋转后的局部坐标系处理。
- 颜色用提取到的文字颜色。原文是白字深底就保持白色，同时检查排版区域是否还在深色背景内。
- 碰撞检测：译文 bbox 与图、表、公式或其他段落相交时，缩小字号或回退。这一步能减少很多“压图”问题。

### 6.7 字体子集与嵌入

流程：

1. 收集整个文档实际用到的字符，按字体风格分组：正文衬线、标题无衬线、等宽。
2. 用 `ttf-parser` 或 `skrifa` 读 cmap，把 char 映射到 glyph id；用 `subsetter::GlyphRemapper` 建立新 GID 映射。
3. 调 `subsetter::subset(font_data, 0, &mapper)` 得到子集字体。
4. 在 PDF 里写 Type0 复合字体：
    - `/Subtype /Type0`
    - `/Encoding /Identity-H`
    - `/DescendantFonts [CIDFontType2 或 CIDFontType0]`
    - `/ToUnicode` CMap：CID(GID) → Unicode
    - `/W` 宽度数组、`/DW 1000`
    - `/FontDescriptor`：FontFile2（TrueType）或 FontFile3 `/OpenType`（CFF），以及 Ascent/Descent/CapHeight/BBox/Flags
5. 绘制时用 2 字节 GID 的 hex string：`<...> Tj`。
6. 每个风格只嵌入一次。输出通常增加 1–3 MB，而不是整个字体的 15–25 MB。

注意：

- CFF/OTF 字体用 CIDFontType0 + FontFile3；TrueType 用 CIDFontType2 + FontFile2。
- subsetter 会重排 GID，绘制前必须用 remap 后的 GID。
- 中英混排可以统一用 CJK 字体（Noto CJK 自带拉丁），也可以按 run 切换 Latin 字体；第一版统一用 CJK 字体更简单。
- 一定要写 ToUnicode，否则输出不可搜索、不可复制。

### 6.8 删除原文与保留原图元（核心）

clean 模式不再创建整页原文快照，实现在 `src/pdf/rewrite.rs`：

1. 用 lopdf 解析内容流，并按调用克隆共享 Form，避免同一个 Form 在不同位置被一起删掉。
2. 在只存在于内存的解释副本里，给每个文字显示操作加唯一标记和零宽 Type3 游标探针。字形位置、字符码和文字矩阵由 hayro 提供，改写器不重新实现字体编码和宽度计算。
3. 删除已成功排版段落对应的字形。完整保留的显示操作继续用原始字符串；混合操作只保留选中的字符码，并恢复每个字形的原始文字矩阵。这样 `TJ` 位移、字距、词距、升高和缩放不会因为前面删了字而错位。
4. 保留原有路径、图片、透明度、文字裁剪和未翻译的文字。行内公式只提取字形轮廓，生成独立的矢量路径 Form，不引用原页面或原正文字符串。
5. 清除被改写作用域里的 ActualText/Alt、未使用的属性资源和探针字体。写出前清理不可达对象，物理移除失去引用的原内容流和旧 Form。
6. 无法可靠改写时明确报错，不自动回退到会保留隐藏原文的 overlay。

受保护的公式、图表、作者信息、参考文献，以及失败或溢出的段落，仍然保留可见原文。这套流程不是清除元数据、附件、注释等所有通道的通用脱敏工具。

overlay 是用户显式选择的兼容模式：保留原内容并覆盖显示，公式同样走独立路径，不使用整页快照。

---

## 7. 单二进制策略

### 7.1 依赖

- 不用 Python、uv、BabelDOC、pdftotext、qpdf。
- PDF 处理用 hayro + lopdf，都是纯 Rust。
- TLS 用 `reqwest` + `rustls` + `webpki-roots` 或系统根证书，不要 OpenSSL。
- 如果用数据库，选 `redb` 这类纯 Rust 嵌入式 KV，避免 SQLite 的 C 依赖。
- 字体内嵌：`include_bytes!` 编译进二进制，运行时做子集；也可以提供 `--font-dir` 覆盖。
- 模型做成可选 feature。严格单文件发行时嵌入；否则首次运行下载到缓存并校验 SHA-256，仍然是一个二进制，只是多一份缓存资源。
- GUI 用 `eframe`，依赖系统图形库，但不需要额外脚本。发布 Linux 二进制时要说明 glibc 和桌面环境要求。

### 7.2 体积估算

| 组成                               | 估算      |
|------------------------------------|-----------|
| Rust 核心 + TLS + CLI              | 8–15 MB   |
| egui GUI（可选）                   | +8–15 MB  |
| 中文正文字体（2 个风格，压缩嵌入） | +20–35 MB |
| 布局模型（可选）                   | +20–40 MB |
| OCR 模型（可选）                   | +10–30 MB |

体积靠 feature 和首次下载控制，别让默认二进制冲到 100 MB 以上。

### 7.3 跨平台

- Linux：CLI 可以用 `x86_64-unknown-linux-musl` 做静态构建；GUI 用 glibc 构建。
- macOS：`aarch64-apple-darwin` + `x86_64-apple-darwin`，用 `lipo` 合成 universal binary，再做签名和公证。
- Windows：MSVC 构建；注意中文路径和系统字体。
- 用 `cargo-dist` 生成 GitHub Releases 的安装包和压缩包。

---

## 8. 测试与验收

### 8.1 语料

至少覆盖：

- arXiv 单栏和双栏 LaTeX（公式多、字体标准）；
- IEEE/ACM（双栏、图注、表格）；
- Springer/Nature（复杂排版、彩色、跨栏图）；
- 中文论文（中文断行、标点）；
- 扫描件（OCR 路径）；
- 特殊情况：旋转页、加密 PDF、损坏 xref、Type3 字体、整页包在 Form XObject 里的文档。

### 8.2 Round-trip 测试（最重要）

计划加一个 `--no-translate --rebuild`：

1. 解释原文，重写文字定位，保留所有原始文字和图元。
2. 分别渲染原页面和重建页面（150–200 DPI）。
3. 计算像素差和 SSIM。

目标是不翻译时，重建结果与原页面在视觉上一致（SSIM > 0.99）。这个测试不涉及翻译质量，能单独验证：

- content stream 的解析和重编码；
- 字体和图片资源；
- 混合文字操作、共享 Form 和独立公式路径；
- 页面旋转和 CropBox。

如果这一步过不了，先别继续做翻译。

### 8.3 其他指标

- 文字提取：与 pdfium/pdftotext 的人工标注对比，字符准确率 > 98%。
- 段落和分栏：人工抽查 50 页，段落边界正确率 > 90%。
- 公式检测：标注集上 precision/recall > 85%/80%（第一版目标）。
- 排版溢出：需要缩到 0.7 倍以下或溢出的段落少于 2%。
- 视觉：译文不压图、不压公式、不压表格，人工抽查。
- 性能：20 页论文解析加排版 < 2 s（不含翻译网络时间），内存 < 500 MB。
- 稳定性：对 content stream parser 做 fuzz，不能 panic。

---

## 9. 风险与对策

| 风险                        | 影响                         | 对策                                                               |
|-----------------------------|------------------------------|--------------------------------------------------------------------|
| content stream 写法千奇百怪 | 清空文字时误删图形或残留文字 | round-trip 测试兜底；按 `BT/ET` 块处理；失败时明确报错，不静默降级 |
| 字体编码/ToUnicode 缺失     | 提取乱码，无法翻译           | hayro 多级 fallback；可选 pdfium 提取；公式和未知文本保护          |
| 中文断行/标点               | 排版难看                     | 禁则、2em 缩进、两端对齐、按实际字体测量                           |
| 译文变长                    | 溢出、压图                   | 字号/行距/字距拟合；向下扩展；碰撞检测；跳过并警告                 |
| 公式识别误判                | 公式被翻译或丢失             | 原位保护；宁可不翻；字体和字符规则可配置                           |
| 双栏/跨栏                   | 串栏                         | XY-cut 加栏边界约束；跨栏 block 单独处理                           |
| Form XObject 包裹整页       | 文字删不掉                   | copy-on-write 递归处理 Form；round-trip 覆盖                       |
| 旋转页/CropBox              | 坐标错位                     | 统一 PDF 用户空间；提取和写入都用原始坐标；旋转页单独测试          |
| 加密/损坏 PDF               | 打不开                       | 支持空密码和用户密码；损坏 xref 给清晰错误；后续再加修复           |
| 模型/代码许可               | 不能闭源或按 MIT 发布        | BabelDOC、DocLayout-YOLO 是 AGPL；要宽松许可就自己实现或换模型     |
| 二进制过大                  | 分发困难                     | 字体和模型做成 feature；首次下载；strip/LTO                        |
| Zotero 集成                 | 平台和进程管理复杂           | 先做本地 HTTP 服务；插件只负责 UI 和进程管理                       |

---

## 10. 参考

- BabelDOC：<https://github.com/funstory-ai/BabelDOC>（AGPL-3.0，管线/排版思路参考）
- PDFMathTranslate：<https://github.com/PDFMathTranslate/PDFMathTranslate>（AGPL-3.0）
- hayro：<https://github.com/LaurenzV/hayro>（纯 Rust PDF 解析/渲染）
- lopdf：<https://github.com/J-F-Liu/lopdf>
- pdf-writer：<https://github.com/typst/pdf-writer>
- subsetter：<https://github.com/typst/subsetter>
- DocLayout-YOLO：<https://github.com/opendatalab/DocLayout-YOLO>（AGPL-3.0）
- zotero-paper-translator：<https://github.com/daniel5u/zotero-paper-translator>
