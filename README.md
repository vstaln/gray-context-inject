<div align="center">
  <img alt="gray-context-inject" src="assets/icon.svg" width="120" height="120" />
  <h1>gray-context-inject</h1>
  <p><strong>One sidecar for every "put text into context" plugin.</strong></p>
  <p>
    <a href="https://gray.alignment.id">Website</a> ·
    <a href="https://github.com/vstaln/gray-context-inject">Source</a> ·
    <a href="https://github.com/vstaln/gray">gray</a>
  </p>
  <p>
    <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-1c1c20?style=flat-square&labelColor=0a0a0b" /></a>
    <a href="https://www.rust-lang.org"><img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-rust-1c1c20?style=flat-square&labelColor=0a0a0b&logo=rust&logoColor=d4a373" /></a>
    <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-1c1c20?style=flat-square&labelColor=0a0a0b&color=7aa2f7" />
  </p>
</div>

<br/>

```bash
gray plugin install context-inject
```

`gray-context-inject` merges seven micro-plugins that all splice text into
the prompt/context into a single sidecar process. Each module keeps its own
`~/.gray/<orig>/` state files and slash commands — existing setups keep
working after you uninstall the standalone versions.

## Replaces

| Module | Former plugin | Hook | Commands |
|---|---|---|---|
| header | `gray-header` | `prompt/context` | `/header` |
| pin | `gray-context-pin` | `prompt/context` | `/pin` `/unpin` `/pins` |
| resources | `gray-resources` | `context/build` | `/resources` |
| rules | `gray-rules` | `agent/before_start` | `/rules` |
| include | `gray-include` | `prompt/context` | `/include` |
| dirty | `gray-dirty-guard` | `prompt/context` | `/dirty` |
| ghissues | `gray-gh-issues` | `input/submit` | `/ghissues` |

`prompt/context` fans out to header + pin + include + dirty and joins their
`{text}` results; `/inject` prints an overview of all seven sources.

## Build from source

```sh
cargo build --release
gray plugin check ./target/release/context-inject
```

## License

MIT
