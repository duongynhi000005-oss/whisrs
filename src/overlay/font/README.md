# Overlay fonts

The agent overlay's text: Inter 4.1 Regular and SemiBold by The Inter Project
Authors, under the SIL Open Font License 1.1 (`OFL.txt`, no Reserved Font
Name). Source: `Inter-4.1.zip` from https://github.com/rsms/inter/releases/tag/v4.1
(`extras/ttf/Inter-Regular.ttf`, `extras/ttf/Inter-SemiBold.ttf`).

Subset to printable ASCII plus the middle dot and the ellipsis, with no
hinting and no layout tables (fontdue reads neither GSUB nor GPOS), with
fonttools 4.67.0, from the directory holding the two release TTFs:

```sh
mkdir -p out
for w in Regular SemiBold; do
  uvx --from fonttools==4.67.0 pyftsubset "Inter-$w.ttf" \
    --unicodes="U+0020-007E,U+00B7,U+2026" \
    --layout-features="" --drop-tables+="GSUB,GPOS,GDEF" \
    --no-hinting --output-file="out/Inter-$w.ttf"
done
```

A character outside the subset is drawn as `?`.
