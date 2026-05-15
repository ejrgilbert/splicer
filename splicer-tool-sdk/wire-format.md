# Recorder binary stream format (v1)

A `splicer:recorder` instance emits one binary stream per wasm instance,
appended to a standard output channel (stdout or stderr, per the `sink`
config key). Each event captures one `on-call` or `on-return` from the
tier-2 hook surface, carrying the lifted typed-value tree verbatim.
Downstream tools (replay, fuzz harnesses, fixture sanitizers) consume
the stream; translation to JSON/human can be done via postprocessing.

All multi-byte integers are little-endian. Signed integers use
two's-complement. `f64` payloads carry the IEEE-754 binary64 bit pattern
verbatim.

## Stream

```
stream  = header + event*

header  = "SPLR"            // 4-byte ASCII magic
        + u32 version       // = 1

event   = u32 rec_len       // bytes in the rest of this event
        + u8  phase         // 0 = call, 1 = return
        + u64 ts_ns         // wasi:clocks/wall-clock at hook entry,
                            //   nanoseconds since unix epoch
        + u64 call_id       // splicer:common.call-id.id (correlates
                            //   on-call ↔ on-return)
        + str interface     // splicer:common.call-id.interface-name
        + str function      // splicer:common.call-id.function-name
        + payload           // shape depends on `phase`, see below
```

`str` is `u32 len + utf-8 bytes`. `rec_len` prefixes everything after
itself, so a consumer can skip events without parsing them and can
detect a truncated tail by short-read.

### Payload (phase = 0, call)

```
u32 n_args
+ n * ( str arg_name + field_tree arg_tree )
```

`arg_name` is `splicer:common.field.name`; `arg_tree` is `splicer:common.field.tree`.

### Payload (phase = 1, return)

```
u8 has_result                    // 0 = void function, 1 = result tree follows
+ (field_tree result_tree)?      // present iff has_result == 1
```

## Field tree

Mirrors `splicer:common.field-tree` cell-array shape verbatim. The
recorder does no flattening or tree-walking. Encode the cells in order,
then each side table in order, then the root index:

```
field_tree = u32 n_cells          + n * cell
           + u32 n_record_infos   + n * record_info
           + u32 n_flags_infos    + n * flags_info
           + u32 n_enum_infos     + n * enum_info
           + u32 n_variant_infos  + n * variant_info
           + u32 n_handle_infos   + n * handle_info
           + u32 root             // index into `cells`
```

### Side-table entries

```
record_info  = str type_name
             + u32 n_fields    + n * (str field_name + u32 cell_idx)

flags_info   = str type_name
             + u32 n_set       + n * str   // set-bit names

enum_info    = str type_name
             + str case_name

variant_info = str type_name
             + str case_name
             + u8 has_payload
             + (u32 payload_idx)?   // present iff has_payload == 1

handle_info  = str type_name
             + u64 id
```

`type_name` is empty for `error-context-handle` (the cell tag already
names the kind; the WIT shape has no nested type to surface). All other
handles carry the resource / stream-element / future-element type name.

## Cell

```
cell = u8 tag + body
```

Tag values match the `splicer:common.cell` variant discriminant order.
Adding a new cell variant requires a wire-version bump (the consumer
can't skip an unknown tag because the body width is tag-dependent).

| Tag | Variant                | Body                                       |
|-----|------------------------|--------------------------------------------|
| 0   | `%bool`                | `u8` (0 or 1)                              |
| 1   | `integer`              | `i64` (s8...s64, u8...u64 widened)         |
| 2   | `floating`             | `f64` (f32 widened)                        |
| 3   | `text`                 | `str` (carries `string` and single `char`) |
| 4   | `bytes`                | `u32 len + bytes`                          |
| 5   | `list-of`              | `u32 n + n * u32` (child cell indices)     |
| 6   | `tuple-of`             | `u32 n + n * u32` (child cell indices)     |
| 7   | `option-some`          | `u32` (inner cell index)                   |
| 8   | `option-none`          | (no body)                                  |
| 9   | `result-ok`            | `u8 has_payload + (u32 payload_idx)?`      |
| 10  | `result-err`           | `u8 has_payload + (u32 payload_idx)?`      |
| 11  | `record-of`            | `u32` (index into `record_infos`)          |
| 12  | `flags-set`            | `u32` (index into `flags_infos`)           |
| 13  | `enum-case`            | `u32` (index into `enum_infos`)            |
| 14  | `variant-case`         | `u32` (index into `variant_infos`)         |
| 15  | `resource-handle`      | `u32` (index into `handle_infos`)          |
| 16  | `stream-handle`        | `u32` (index into `handle_infos`)          |
| 17  | `future-handle`        | `u32` (index into `handle_infos`)          |
| 18  | `error-context-handle` | `u32` (index into `handle_infos`)          |

## Flush semantics

The recorder flushes its in-memory buffer on every `on-return`. Events
between two returns may share one underlying write but always land
contiguously. A clean shutdown can leave at most one `on-call` whose
matching `on-return` never fired (the wrapped target was killed or the
runtime stopped polling mid-call); the consumer observes this as a
call event with no return event of the same `call_id`.

## Versioning

The header's `version` field is bumped on any incompatible wire change,
including adding a new cell tag. Within a fixed `version`, all events
are decodable by a consumer that knows that version.
