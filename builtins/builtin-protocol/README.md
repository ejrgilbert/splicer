# splicer-builtin-protocol

**Internal crate for the [`splicer`](https://crates.io/crates/splicer) project.**
Not intended for direct use outside the splicer ecosystem.

This crate is published to crates.io only so the splicer CLI can be
published with its dependency graph fully resolvable. Its API is not
stable in the SemVer sense and may change between splicer releases
without notice.

## What it does

The wire contracts between the splicer host and its builtin
middleware components: manifest schema, section-name conventions,
data wire formats, and the `build_helper::codegen` entry point that
each builtin uses at build time to bake an embedded manifest into
its component.

If you're looking for splicer itself, see
<https://github.com/ejrgilbert/splicer>.
