# splicer-builtin-manifest

**Internal crate for the [`splicer`](https://crates.io/crates/splicer) project.**
Not intended for direct use outside the splicer ecosystem.

This crate is published to crates.io only so the splicer CLI can be
published with its dependency graph fully resolvable. Its API is not
stable in the SemVer sense and may change between splicer releases
without notice.

## What it does

Shared schema + `build.rs` codegen for splicer's builtin middleware
manifests. The splicer CLI consumes the schema to read manifests
embedded in builtin components; each builtin uses the
`build_helper::codegen` entry point at build time to produce its own
embedded manifest.

If you're looking for splicer itself, see
<https://github.com/ejrgilbert/splicer>.
