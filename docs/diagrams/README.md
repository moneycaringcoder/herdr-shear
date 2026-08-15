# Diagram sources

Sources live here, rendered output in `docs/img/`. Both are committed, so a
reader never needs a toolchain to see the picture and a maintainer never has to
reverse-engineer an SVG to change one.

Regenerate after editing a source:

```sh
d2 --theme 0 --dark-theme 200 --pad 24 docs/diagrams/verdicts.d2 docs/img/verdicts.svg
```

The `--dark-theme` flag matters: it emits one SVG carrying both palettes behind
a `prefers-color-scheme` query, so a single file reads correctly on GitHub's
light and dark themes.

`docs/img/logo.svg` is hand-authored rather than generated. Its colours are
mid-tone on purpose so it needs no dark variant.

The data-flow diagram in the README is inline Mermaid rather than a file, since
GitHub renders that natively and re-themes it per reader.
