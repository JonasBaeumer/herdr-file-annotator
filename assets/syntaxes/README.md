# Bundled syntax grammars

syntect's default syntax set predates TypeScript, so `.ts`/`.tsx` files
fell back to plain text (no highlighting). These two grammars are layered
onto the defaults at startup (see `bundled_syntax_set` in `src/ui.rs`),
embedded via `include_str!` — the pane runs with cwd set to the repo under
review, so a runtime-relative asset path would not resolve.

## Provenance

Machine conversions of Microsoft's MIT-licensed
[TypeScript-TmLanguage](https://github.com/Microsoft/TypeScript-TmLanguage)
grammars, vendored via [bat](https://github.com/sharkdp/bat)
(`assets/syntaxes/02_Extra/TypeScript.sublime-syntax` and
`assets/syntaxes/02_Extra/TypsecriptReact.sublime-syntax` — note bat's
filename typo, not preserved here). No manual changes; `TSX.sublime-syntax`
is a rename only. bat itself uses these files with syntect, which is the
compatibility signal that matters here.

Vendored from bat at commit `7323a7514f7601737640e7172be115127d6db08c`
(2026-09-04). The grammars inside are Microsoft's, MIT-licensed; the
copyright + permission notice from
[TypeScript-TmLanguage's LICENSE.txt](https://github.com/microsoft/TypeScript-TmLanguage/blob/master/LICENSE.txt)
applies:

> Copyright (c) Microsoft Corporation. All rights reserved. MIT License —
> permission is hereby granted, free of charge, to any person obtaining a
> copy of this software and associated documentation files (the "Software"),
> to deal in the Software without restriction, including without limitation
> the rights to use, copy, modify, merge, publish, distribute, sublicense,
> and/or sell copies of the Software [...]. The above copyright notice and
> this permission notice shall be included in all copies or substantial
> portions of the Software.

## Why not sublimehq/Packages?

Sublime's canonical `JavaScript/TypeScript.sublime-syntax` (and `TSX`)
`extends` a `(Plain)` base syntax across multiple files, which syntect's
YAML loader does not honor — loading it standalone silently loses the base
grammar. The standalone conversions vendored here have no such dependency.

## Refreshing

```sh
scripts/update-syntaxes.sh
```

Re-downloads both files from the pinned bat commit above. To move to a
newer bat commit, edit the `PIN` in that script, re-run it, update the
commit hash recorded here, and run `cargo test` — the `ts_and_tsx_*` and
`highlighted_ts*` tests in `src/ui.rs` pin the extension resolution and
will fail loudly if an upstream reshuffle changes behavior.

## Known limitation

TSX rendering under syntect is imperfect for some expressions (upstream
syntect issue #97: e.g. string-concatenation inside JSX attribute
expressions can throw off the tokenizer past that point, where Sublime
Text stays correct). Strictly more informative than the previous
monochrome plain-text fallback; do not re-report upstream's bug as ours.
