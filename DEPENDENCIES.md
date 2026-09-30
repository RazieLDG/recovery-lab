# Dependency license metadata review

**Reviewed:** 2026-09-30. **Project license selected by owner:** Apache-2.0.

This is a bounded review of declared Cargo package metadata, not a source audit, a complete third-party notice bundle, or a legal opinion. It does not guarantee license compatibility or redistribution compliance.

## Result

- **97 third-party package versions** (96 distinct names) in the locked all-platform graph: 4 direct and 93 transitive.
- **81 third-party package versions** in the `x86_64-unknown-linux-gnu` filtered graph; 16 are present only in the broader target graph.
- **0 missing license declarations, 0 unknown identifiers/expressions after the two legacy notations below are normalized, and 0 obvious strong-copyleft expressions** in this metadata screen.
- Every declaration offers a candidate permissive licensing path. This is an expression-level observation, not confirmation of source provenance or legal compliance.
- **2 legacy spellings:** `ctrlc 3.5.2` and `serde_urlencoded 0.7.1` declare `MIT/Apache-2.0`. The evidence preserves that text and treats it as `MIT OR Apache-2.0` for screening. Cargo documents slash notation as deprecated.
- **19 packages require Unicode-3.0:** 18 declare it alone; `unicode-ident 1.0.26` declares `(MIT OR Apache-2.0) AND Unicode-3.0`. Choosing Apache-2.0 or MIT does not remove that additional Unicode condition.

## What was checked

The review read `Cargo.lock` and collected metadata with Cargo 1.98.1 / rustc 1.98.1:

```sh
cargo metadata --locked --format-version 1
cargo metadata --locked --format-version 1 --filter-platform x86_64-unknown-linux-gnu
```

No dependency manifests or lock entries were changed, and no audit tools were installed. Cargo downloaded missing locked target-specific package metadata/source archives as part of metadata resolution. The unfiltered metadata and lockfile contain exactly the same 98 package identities including Recovery Lab, which is excluded from the third-party counts.

Lockfile SHA-256: `40554dea5ff4139b926765324662f86113d4f24589e78b30d6769ed3daae6070`.

The unfiltered command covers the resolved graph for the project’s current feature selection across target platforms, including relevant build dependencies and proc macros. It does **not** enable every possible optional feature. The Linux count is the filtered metadata graph, not a measured list of crates linked into a particular binary. See the [Cargo metadata documentation](https://doc.rust-lang.org/cargo/commands/cargo-metadata.html).

Not examined: individual source/license/copyright/NOTICE files, transitive vendored or embedded code inside dependencies, non-Cargo tools (including Toxiproxy), OS libraries, the Rust standard library/toolchain, actual release package contents, or security vulnerabilities. All `license_file` metadata fields were null; that does not mean upstream license files are absent.

## Interpreting the choices

The evidence lists one candidate permissive path for each declaration, preferring Apache-2.0 where offered, otherwise MIT or Unicode-3.0. These are review suggestions rather than completed legal license elections. `OR` permits an alternative, `AND` retains both obligations, and `WITH` attaches an exception; see [Cargo’s license-expression guidance](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields).

- MIT/Apache alternatives can use Apache-2.0; MIT-only packages retain MIT.
- `memchr` offers MIT instead of Unlicense; `ryu` offers Apache-2.0 instead of BSL-1.0.
- `dispatch2` offers Apache-2.0 alongside Zlib and MIT.
- `wasi` offers plain Apache-2.0 or MIT as alternatives to Apache-2.0 with the LLVM exception.
- For `unicode-ident`, the candidate path is `Apache-2.0 AND Unicode-3.0`.

Recovery Lab’s own Apache-2.0 license does not relicense its dependencies. When redistributing dependency code or a binary containing it, preserve actual upstream notices and meet the applicable conditions. Apache-2.0 has license, notice and modification requirements; MIT requires its copyright and permission notice; Unicode-3.0 requires its copyright and permission notice with copies or associated documentation. Consult the actual upstream files rather than inventing copyright holders. This inventory is **not** a substitute for those files. Sources: [Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0), [MIT](https://spdx.org/licenses/MIT.html), [Unicode-3.0](https://spdx.org/licenses/Unicode-3.0.html).

## Declared-expression counts

| Declared expression | All targets | Linux |
| --- | ---: | ---: |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 | 1 |
| `Apache-2.0` | 1 | 1 |
| `Apache-2.0 OR BSL-1.0` | 1 | 1 |
| `Apache-2.0 OR MIT` | 4 | 4 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 1 | 0 |
| `MIT` | 23 | 20 |
| `MIT OR Apache-2.0` | 44 | 33 |
| `MIT/Apache-2.0` | 2 | 2 |
| `Unicode-3.0` | 18 | 18 |
| `Unlicense OR MIT` | 1 | 1 |
| `Zlib OR Apache-2.0 OR MIT` | 1 | 0 |

Counts are package versions, so both resolved `base64` versions are counted.

## Package inventory

The license column is verbatim metadata. “Linux” indicates membership in the filtered resolve graph. The complete machine-readable record, including lockfile checksums, registry IDs, repository metadata, resolved features and candidate license paths, is in [evidence/dependency-licenses.json](evidence/dependency-licenses.json).

| Package | Version | Declared license | Direct | Linux |
| --- | --- | --- | :---: | :---: |
| `atomic-waker` | `1.1.2` | `Apache-2.0 OR MIT` | no | yes |
| `base64` | `0.22.1` | `MIT OR Apache-2.0` | no | yes |
| `base64` | `0.23.1` | `MIT OR Apache-2.0` | no | yes |
| `bitflags` | `2.13.2` | `MIT OR Apache-2.0` | no | yes |
| `block2` | `0.6.2` | `MIT` | no | no |
| `bumpalo` | `3.20.3` | `MIT OR Apache-2.0` | no | no |
| `bytes` | `1.12.1` | `MIT` | no | yes |
| `cfg-if` | `1.0.5` | `MIT OR Apache-2.0` | no | yes |
| `cfg_aliases` | `0.2.2` | `MIT` | no | yes |
| `ctrlc` | `3.5.2` | `MIT/Apache-2.0` | yes | yes |
| `dispatch2` | `0.3.1` | `Zlib OR Apache-2.0 OR MIT` | no | no |
| `displaydoc` | `0.2.7` | `MIT OR Apache-2.0` | no | yes |
| `form_urlencoded` | `1.2.2` | `MIT OR Apache-2.0` | no | yes |
| `futures-channel` | `0.3.34` | `MIT OR Apache-2.0` | no | yes |
| `futures-core` | `0.3.34` | `MIT OR Apache-2.0` | no | yes |
| `futures-io` | `0.3.34` | `MIT OR Apache-2.0` | no | yes |
| `futures-sink` | `0.3.34` | `MIT OR Apache-2.0` | no | yes |
| `futures-task` | `0.3.34` | `MIT OR Apache-2.0` | no | yes |
| `futures-util` | `0.3.34` | `MIT OR Apache-2.0` | no | yes |
| `http` | `1.5.0` | `MIT OR Apache-2.0` | no | yes |
| `http-body` | `1.1.0` | `MIT` | no | yes |
| `http-body-util` | `0.1.5` | `MIT` | no | yes |
| `httparse` | `1.10.1` | `MIT OR Apache-2.0` | no | yes |
| `hyper` | `1.11.1` | `MIT` | no | yes |
| `hyper-util` | `0.1.21` | `MIT` | no | yes |
| `icu_collections` | `2.3.0` | `Unicode-3.0` | no | yes |
| `icu_locale_core` | `2.3.0` | `Unicode-3.0` | no | yes |
| `icu_normalizer` | `2.3.0` | `Unicode-3.0` | no | yes |
| `icu_normalizer_data` | `2.3.0` | `Unicode-3.0` | no | yes |
| `icu_properties` | `2.3.0` | `Unicode-3.0` | no | yes |
| `icu_properties_data` | `2.3.0` | `Unicode-3.0` | no | yes |
| `icu_provider` | `2.3.1` | `Unicode-3.0` | no | yes |
| `idna` | `1.1.0` | `MIT OR Apache-2.0` | no | yes |
| `idna_adapter` | `1.2.2` | `Apache-2.0 OR MIT` | no | yes |
| `ipnet` | `2.12.2` | `MIT OR Apache-2.0` | no | yes |
| `itoa` | `1.0.18` | `MIT OR Apache-2.0` | no | yes |
| `js-sys` | `0.3.106` | `MIT OR Apache-2.0` | no | no |
| `libc` | `0.2.189` | `MIT OR Apache-2.0` | no | yes |
| `litemap` | `0.8.3` | `Unicode-3.0` | no | yes |
| `log` | `0.4.34` | `MIT OR Apache-2.0` | no | yes |
| `memchr` | `2.8.3` | `Unlicense OR MIT` | no | yes |
| `mio` | `1.2.3` | `MIT` | no | yes |
| `nix` | `0.31.3` | `MIT` | no | yes |
| `objc2` | `0.6.4` | `MIT` | no | no |
| `objc2-encode` | `4.1.0` | `MIT` | no | no |
| `once_cell` | `1.21.4` | `MIT OR Apache-2.0` | no | yes |
| `percent-encoding` | `2.3.2` | `MIT OR Apache-2.0` | no | yes |
| `pin-project-lite` | `0.2.17` | `Apache-2.0 OR MIT` | no | yes |
| `potential_utf` | `0.1.6` | `Unicode-3.0` | no | yes |
| `proc-macro2` | `1.0.107` | `MIT OR Apache-2.0` | no | yes |
| `quote` | `1.0.47` | `MIT OR Apache-2.0` | no | yes |
| `reqwest` | `0.12.28` | `MIT OR Apache-2.0` | yes | yes |
| `rustversion` | `1.0.23` | `MIT OR Apache-2.0` | no | no |
| `ryu` | `1.0.23` | `Apache-2.0 OR BSL-1.0` | no | yes |
| `serde` | `1.0.229` | `MIT OR Apache-2.0` | yes | yes |
| `serde_core` | `1.0.229` | `MIT OR Apache-2.0` | no | yes |
| `serde_derive` | `1.0.229` | `MIT OR Apache-2.0` | no | yes |
| `serde_json` | `1.0.151` | `MIT OR Apache-2.0` | yes | yes |
| `serde_urlencoded` | `0.7.1` | `MIT/Apache-2.0` | no | yes |
| `slab` | `0.4.12` | `MIT` | no | yes |
| `smallvec` | `1.16.2` | `MIT OR Apache-2.0` | no | yes |
| `socket2` | `0.6.5` | `MIT OR Apache-2.0` | no | yes |
| `stable_deref_trait` | `1.2.1` | `MIT OR Apache-2.0` | no | yes |
| `syn` | `3.0.6` | `MIT OR Apache-2.0` | no | yes |
| `sync_wrapper` | `1.0.2` | `Apache-2.0` | no | yes |
| `synstructure` | `0.14.0` | `MIT` | no | yes |
| `tinystr` | `0.8.4` | `Unicode-3.0` | no | yes |
| `tokio` | `1.53.1` | `MIT` | no | yes |
| `tower` | `0.5.3` | `MIT` | no | yes |
| `tower-http` | `0.6.11` | `MIT` | no | yes |
| `tower-layer` | `0.3.3` | `MIT` | no | yes |
| `tower-service` | `0.3.3` | `MIT` | no | yes |
| `tracing` | `0.1.44` | `MIT` | no | yes |
| `tracing-core` | `0.1.36` | `MIT` | no | yes |
| `try-lock` | `0.2.5` | `MIT` | no | yes |
| `unicode-ident` | `1.0.26` | `(MIT OR Apache-2.0) AND Unicode-3.0` | no | yes |
| `url` | `2.5.8` | `MIT OR Apache-2.0` | no | yes |
| `utf8_iter` | `1.0.4` | `Apache-2.0 OR MIT` | no | yes |
| `want` | `0.3.1` | `MIT` | no | yes |
| `wasi` | `0.11.1+wasi-snapshot-preview1` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | no | no |
| `wasm-bindgen` | `0.2.129` | `MIT OR Apache-2.0` | no | no |
| `wasm-bindgen-futures` | `0.4.79` | `MIT OR Apache-2.0` | no | no |
| `wasm-bindgen-macro` | `0.2.129` | `MIT OR Apache-2.0` | no | no |
| `wasm-bindgen-macro-support` | `0.2.129` | `MIT OR Apache-2.0` | no | no |
| `wasm-bindgen-shared` | `0.2.129` | `MIT OR Apache-2.0` | no | no |
| `web-sys` | `0.3.106` | `MIT OR Apache-2.0` | no | no |
| `windows-link` | `0.2.1` | `MIT OR Apache-2.0` | no | no |
| `windows-sys` | `0.61.2` | `MIT OR Apache-2.0` | no | no |
| `writeable` | `0.6.4` | `Unicode-3.0` | no | yes |
| `yoke` | `0.8.3` | `Unicode-3.0` | no | yes |
| `yoke-derive` | `0.8.3` | `Unicode-3.0` | no | yes |
| `zerofrom` | `0.1.8` | `Unicode-3.0` | no | yes |
| `zerofrom-derive` | `0.1.8` | `Unicode-3.0` | no | yes |
| `zerotrie` | `0.2.5` | `Unicode-3.0` | no | yes |
| `zerovec` | `0.11.8` | `Unicode-3.0` | no | yes |
| `zerovec-derive` | `0.11.6` | `Unicode-3.0` | no | yes |
| `zmij` | `1.0.23` | `MIT` | no | yes |

## Before a redistribution release

1. Re-run the inventory when the lockfile, target or feature selection changes.
2. Inspect the actual dependency license and notice files, including embedded third-party material, for the code that will be shipped.
3. Package the required license texts/notices and review artifact contents; obtain qualified legal review if a compatibility determination is needed.
