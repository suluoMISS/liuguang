# 流光（Liuguang）应用图标

[`liuguang.svg`](liuguang.svg) 是唯一矢量源文件，其余 `liuguang-<尺寸>.png` 均由其栅格化生成。

设计元素：暗色圆角方形（squircle）+ 极光丝带 + 中央播放标识。

## 修改后重新渲染

```bash
for s in 16 24 32 48 64 128 256 512; do
    rsvg-convert -w "$s" -h "$s" liuguang.svg -o "liuguang-$s.png"
done
```

需安装 `librsvg`（Arch: `pacman -S librsvg`；Fedora: `dnf install librsvg2-tools`）。
