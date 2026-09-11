mermaid(思维导图渲染库)
================================

本目录存放前端第三方库,由构建前手动下载(不入版本库)。

需要的文件
----------
  mermaid.min.js    约 5.4 MB

来源
----
优先用国内镜像(npmmirror),它比 unpkg / jsdelivr 稳定:

  curl.exe -L --ssl-no-revoke -o ui\vendor\mermaid.min.js ^
    https://registry.npmmirror.com/mermaid/latest/files/dist/mermaid.min.js

备用地址:

  https://unpkg.com/mermaid@11/dist/mermaid.min.js
  https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js

下载后校验(文件应是有效 JS,且含 mindmap 支持):

  $t = [System.IO.File]::ReadAllText("ui\vendor\mermaid.min.js", [System.Text.Encoding]::UTF8)
  $t.Contains("mindmap")     # 应为 True
  $t.Length                  # 应为数百万字符级别

如果没有这个文件
----------------
程序仍能运行,只是「思维导图」标签页会:

  * 在状态栏显示「未加载 mermaid 库(ui/vendor/mermaid.min.js)」
  * 自动降级显示 mindmap-outline.md 里的文本大纲

也就是说缺库不会让功能不可用 —— 只是看不到图。
