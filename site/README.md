# getrad.dev

Static site, one file: `index.html` (English and Chinese in the same page, switched by the `EN / 中` button). No build step, no JavaScript dependencies. Fonts come from Google Fonts (Geist, JetBrains Mono, and Sora for the six letters of the wordmark).

Preview: `python3 -m http.server -d site 8000`.

## The terminal figure

Figure 1 is a frame rendered by the TUI prototype script, pasted into `index.html` twice (`.frame-light`, `.frame-dark`). To regenerate:

```sh
for t in light dark; do
  bash docs/research/2026-10-16-render-prototype/render-screen.sh --width 120 --panel off --theme $t > /tmp/screen-$t.ansi
done
python3 -I site/tools/ansi2html.py /tmp/screen-light.ansi 116,89,180   # paste into <pre class="frame-light">
python3 -I site/tools/ansi2html.py /tmp/screen-dark.ansi 167,152,215   # paste into <pre class="frame-dark">
```

The two numbers are the theme's accent RGB; the script maps it to the page's `var(--accent)`. Box-drawing characters are drawn by CSS (`.bx`), not by the font, because the Google Fonts subset of JetBrains Mono has none and a fallback font breaks the lines.

## Before launch

- `brew install radian` is a placeholder (marked "soon"); fix the formula name or remove the row.
- The frame's model name, costs and quota are invented by the prototype script.
- The two speed numbers come from `docs/perf-baseline.md` (Apple M4, before ADR 0040); re-measure.
- Deployment (host, `CNAME`) is not decided.
