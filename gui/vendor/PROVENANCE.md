# Macro backport provenance

The two directories are crates.io release sources, including their complete
`LICENSE` and `COPYRIGHT` files. Their normalized `Cargo.toml` files are used;
`Cargo.toml.orig` is the unmodified published manifest, not the active manifest.

| Package | Crate SHA-256 | Published source revision |
| --- | --- | --- |
| `gtk3-macros 0.18.2` | `52ff3c5b21f14f0736fed6dcfc0bfb4225ebf5725f3c0209edeec181e4d73e9d` | `00133512bfc8bbb8e9be59b20e93406c6a3eeb21`, marked dirty in the published archive |
| `glib-macros 0.18.5` | `0bb0228f477c0900c880fd78c8759b95c7636dbd7842707f49e132378aa2acdc` | `42b9caf98e03ded086362d9653ca58fe94dc8658` |

Both packages are MIT licensed. Their existing `syn 2` dependency is
MIT OR Apache-2.0 and is not vendored here.

## Local modifications

- Both normalized manifests drop `proc-macro-error` only.
- GTK: `attribute_parser.rs`, `composite_template_derive.rs` and `lib.rs`
  adapt [gtk3-rs commit 45782251fa35ecf8bfe759ffceb3db32659a0c97](https://github.com/gtk-rs/gtk3-rs/commit/45782251fa35ecf8bfe759ffceb3db32659a0c97).
  Abort paths become spanned `syn::Error` compile errors.
- GLib: `boxed_derive.rs`, `closure.rs`, `derived_properties_attribute.rs`,
  `enum_derive.rs`, `error_domain_derive.rs`, `flags_attribute.rs`, `lib.rs`,
  `object_interface_attribute.rs`, `object_subclass_attribute.rs`,
  `shared_boxed_derive.rs` and `variant_derive.rs` adapt
  [gtk-rs-core commit c74a40a16b23a1b9c8e67842d32d02bc187f551d](https://github.com/gtk-rs/gtk-rs-core/commit/c74a40a16b23a1b9c8e67842d32d02bc187f551d)
  to the older 0.18 APIs. The 0.18 flags, enum and subclass implementations
  differ from upstream's newer versions and retain their original generated
  code. Unlike that upstream commit, duplicate `@watch` diagnostics point to
  the second capture and also report the first, preserving both locations.
- No upstream feature upgrades or changes to application code are included.

These are local forks of unmaintained GTK 3-era packages. Their owner must
review security and compatibility changes and retire the patches when a
compatible maintained stack is adopted. The fixtures do not prove compatibility
for every macro or every input. GTK 0.18's existing `template(string = ...)`
expansion produces `&&str` for an API expecting `&[u8]`; this backport does not
change that unrelated behavior.
